use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

pub(crate) async fn cache_workflow_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let matched_path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    let method = request.method().clone();
    let request_headers = request.headers().clone();

    let mut response = next.run(request).await;

    if is_private_cache_scope(&path) && !skip_private_cache_control(&path, &method) {
        set_private_cache_control_if_missing(response.headers_mut());
    }

    if !matches!(method, Method::GET | Method::HEAD) || !is_conditional_scope(&path) {
        return response;
    }

    if response.status() != StatusCode::OK
        || response.status() == StatusCode::PARTIAL_CONTENT
        || response.headers().contains_key(header::CONTENT_RANGE)
        || response.headers().contains_key(header::CONTENT_DISPOSITION)
        || is_etag_excluded_path(&matched_path)
    {
        return response;
    }

    let existing_etag = response_etag(response.headers());
    let existing_last_modified = response_last_modified(response.headers());
    let has_if_none_match = request_headers.contains_key(header::IF_NONE_MATCH);

    if let Some(etag) = existing_etag.as_deref()
        && if_none_match_matches(&request_headers, etag)
    {
        return not_modified_from_response_headers(response.headers());
    }

    if !has_if_none_match
        && let Some(last_modified) = existing_last_modified.as_deref()
        && if_modified_since_matches(&request_headers, last_modified)
    {
        return not_modified_from_response_headers(response.headers());
    }

    if existing_etag.is_some() || existing_last_modified.is_some() {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let etag = asset_etag(bytes.as_ref());
    if if_none_match_matches(&request_headers, etag.as_str()) {
        let mut headers = parts.headers.clone();
        insert_header_if_valid(&mut headers, header::ETAG, etag.as_str());
        return not_modified_from_headers(&headers);
    }

    let mut response = Response::from_parts(parts, Body::from(bytes));
    insert_header_if_valid(response.headers_mut(), header::ETAG, etag.as_str());
    response
}

pub(crate) fn asset_etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    format!("\"{hex}\"")
}

/// ETag derived from the source file's mtime + size, salted with a request
/// variant key (page number / output format). Bytes served for a given
/// (source file, variant) are deterministic, so this avoids hashing the full
/// response body on every conditional request. Falls back to `None` when the
/// file metadata cannot be read; callers then use the content hash instead.
pub(crate) fn asset_file_etag(path: &Path, key: &str) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    let mtime_nanos =
        i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos());
    let digest = Sha256::digest(format!("{mtime_nanos}:{}:{key}", metadata.len()).as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Some(format!("\"{hex}\""))
}

pub(crate) fn format_http_date(time: SystemTime) -> Option<String> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    let timestamp = i64::try_from(duration.as_secs()).ok()?;
    OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()?
        .format(&Rfc2822)
        .ok()
        // HTTP-date per RFC 7231 requires the GMT zone name; time's Rfc2822
        // formatter emits the numeric "+0000" offset, which is not the
        // canonical IMF-fixdate that Spring (Kotlin backend) produces.
        .map(|value| value.replace(" +0000", " GMT"))
}

pub(crate) fn file_last_modified_header_value(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    format_http_date(modified)
}

pub(crate) fn if_none_match_matches(headers: &HeaderMap, expected_etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate == expected_etag)
        })
        .unwrap_or(false)
}

pub(crate) fn if_modified_since_matches(headers: &HeaderMap, expected_last_modified: &str) -> bool {
    let Some(expected) = OffsetDateTime::parse(expected_last_modified, &Rfc2822).ok() else {
        return false;
    };

    headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| OffsetDateTime::parse(value, &Rfc2822).ok())
        .is_some_and(|value| value >= expected)
}

pub(crate) fn asset_not_modified_response(
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    if let Some(last_modified) = last_modified {
        insert_header_if_valid(response.headers_mut(), header::LAST_MODIFIED, last_modified);
    }
    if let Some(etag) = etag {
        insert_header_if_valid(response.headers_mut(), header::ETAG, etag);
    }
    response
}

pub(crate) fn asset_ok_response(
    content_type: &str,
    bytes: Vec<u8>,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Response {
    let mut response = (StatusCode::OK, bytes).into_response();
    insert_header_if_valid(response.headers_mut(), header::CONTENT_TYPE, content_type);
    if let Some(last_modified) = last_modified {
        insert_header_if_valid(response.headers_mut(), header::LAST_MODIFIED, last_modified);
    }
    if let Some(etag) = etag {
        insert_header_if_valid(response.headers_mut(), header::ETAG, etag);
    }
    response
}

fn is_private_cache_scope(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/opds/")
}

fn skip_private_cache_control(path: &str, method: &Method) -> bool {
    path == "/api/v1/libraries" && matches!(*method, Method::GET | Method::HEAD)
}

fn is_conditional_scope(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/opds/") || path.starts_with("/kobo/")
}

fn is_etag_excluded_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/claim"
            | "/api/v1/libraries"
            | "/api/v1/oauth2/providers"
            | "/api/v1/client-settings/global/list"
            | "/api/v1/client-settings/user/list"
            | "/api/v2/users/me"
            | "/kobo/{auth_token}/v1/initialization"
            | "/kobo/{auth_token}/v1/library/sync"
            | "/api/v1/series/{series_id}/file"
            | "/api/v1/series/{series_id}/thumbnails/{thumbnail_id}"
            | "/api/v1/readlists/{readlist_id}/file"
            | "/api/v1/books/{book_id}/file"
            | "/api/v1/books/{book_id}/file/{*file_name}"
            | "/opds/v1.2/books/{book_id}/file/{file_name}"
            | "/opds/v2/books/{book_id}/file"
            | "/opds/v2/books/{book_id}/file/{*file_name}"
            | "/kobo/{auth_token}/v1/books/{book_id}/file/epub"
    )
}

fn set_private_cache_control_if_missing(headers: &mut HeaderMap) {
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("max-age=0, must-revalidate, private"),
        );
    }
}

fn response_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn response_last_modified(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn not_modified_from_response_headers(headers: &HeaderMap) -> Response {
    not_modified_from_headers(headers)
}

fn not_modified_from_headers(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    copy_response_header(headers, response.headers_mut(), header::CACHE_CONTROL);
    copy_response_header(headers, response.headers_mut(), header::ETAG);
    copy_response_header(headers, response.headers_mut(), header::LAST_MODIFIED);
    response
}

fn copy_response_header(from: &HeaderMap, to: &mut HeaderMap, name: header::HeaderName) {
    if let Some(value) = from.get(&name).cloned() {
        to.insert(name, value);
    }
}

fn insert_header_if_valid(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use futures_util::stream;
    use tower::util::ServiceExt;

    #[test]
    fn if_modified_since_roundtrip() {
        let date = format_http_date(std::time::UNIX_EPOCH + std::time::Duration::new(1_790_000_000, 0)).unwrap();
        println!("formatted: {date}");
        assert!(date.ends_with(" GMT"), "HTTP-date must use GMT zone name: {date}");
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MODIFIED_SINCE, date.parse().unwrap());
        assert!(if_modified_since_matches(&headers, &date), "same value must match");
        let older = "Mon, 01 Jan 2000 00:00:00 GMT";
        let mut older_headers = HeaderMap::new();
        older_headers.insert(header::IF_MODIFIED_SINCE, older.parse().unwrap());
        assert!(!if_modified_since_matches(&older_headers, &date), "older must not match");
        let mut future_headers = HeaderMap::new();
        future_headers.insert(header::IF_MODIFIED_SINCE, "Mon, 01 Jan 2030 00:00:00 GMT".parse().unwrap());
        assert!(if_modified_since_matches(&future_headers, &date), "future must match");
    }

    #[test]
    fn excluded_paths_match_expected_templates() {
        for path in [
            "/api/v1/claim",
            "/api/v1/libraries",
            "/api/v1/oauth2/providers",
            "/api/v1/client-settings/global/list",
            "/api/v1/client-settings/user/list",
            "/api/v2/users/me",
            "/kobo/{auth_token}/v1/initialization",
            "/kobo/{auth_token}/v1/library/sync",
            "/api/v1/books/{book_id}/file/{*file_name}",
            "/opds/v2/books/{book_id}/file",
            "/kobo/{auth_token}/v1/books/{book_id}/file/epub",
        ] {
            assert!(
                is_etag_excluded_path(path),
                "path should be excluded: {path}"
            );
        }
    }

    #[tokio::test]
    async fn cache_middleware_returns_internal_error_when_body_collection_fails() {
        async fn failing_body_response() -> Response {
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from_stream(stream::once(async {
                    Err::<Vec<u8>, std::io::Error>(std::io::Error::other("body stream failed"))
                })))
                .expect("failing response should build")
        }

        let app = Router::new()
            .route("/api/body-error", get(failing_body_response))
            .route_layer(axum::middleware::from_fn(cache_workflow_middleware));

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/body-error")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("middleware request should complete");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
