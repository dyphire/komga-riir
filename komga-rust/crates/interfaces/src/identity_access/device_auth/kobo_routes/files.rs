use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use komga_application::identity_access::{AuthUserRole, user_has_role, user_is_admin};
use serde::Deserialize;
use std::path::Path as FsPath;

use crate::access_log::RequestConnectionInfo;
use crate::helpers::spring_error_response;
use crate::identity_access::device_auth::auth_resolvers::required_kobo_user;
use crate::media_assets::access_control::user_can_access_book_media;
use crate::media_assets::http_helpers::{attachment_disposition, internal_error_response};
use crate::state::IdentityAccessState;

#[derive(Deserialize, Default)]
pub(crate) struct KoboBookFileQuery {
    convert_kepub: Option<bool>,
}

fn convert_epub_to_kepub_bytes(input_file: &FsPath) -> anyhow::Result<Vec<u8>> {
    komga_kepubify::convert_epub_file_to_bytes(input_file)
}

fn missing_kobo_file_response() -> Response {
    spring_error_response(StatusCode::NOT_FOUND, "File not found, it may have moved")
}

fn kepub_conversion_failed_response() -> Response {
    spring_error_response(StatusCode::SERVICE_UNAVAILABLE, "Kepub conversion failed")
}

fn kobo_epub_base(file_name: &str) -> &str {
    if let Some((base, ext)) = file_name.rsplit_once('.')
        && (ext.eq_ignore_ascii_case("epub") || ext.eq_ignore_ascii_case("mobi"))
    {
        return base;
    }
    file_name
}

fn kobo_kepub_file_name(file_name: &str) -> String {
    format!("{}.kepub.epub", kobo_epub_base(file_name))
}

fn kobo_epub_file_name(file_name: &str) -> String {
    format!("{}.epub", kobo_epub_base(file_name))
}

/// Persist the converted kepub file size so Kobo metadata can report the
/// accurate download size for the KEPUB profile. Failures are non-fatal for
/// the download itself.
async fn persist_kepub_projection(
    device_sync: &dyn komga_application::identity_access::DeviceSyncPort,
    book_id: &str,
    file_size: u64,
) {
    if let Err(error) = device_sync
        .save_book_projection_file_size(book_id, "kepub", file_size)
        .await
    {
        tracing::warn!(
            error = ?error,
            book_id,
            "failed to persist kepub projection file size"
        );
    }
}

pub(crate) async fn kobo_book_file_epub(
    State(app): State<IdentityAccessState>,
    Path((auth_token, book_id)): Path<(String, String)>,
    Extension(connection_info): Extension<RequestConnectionInfo>,
    headers: HeaderMap,
    Query(query): Query<KoboBookFileQuery>,
) -> Response {
    let current_user = match required_kobo_user(
        &app.identity,
        auth_token.as_str(),
        &headers,
        connection_info.remote_addr(),
    )
    .await
    {
        Ok(current_user) => current_user,
        Err(status) => return status.into_response(),
    };

    if !user_is_admin(&current_user) && !user_has_role(&current_user, AuthUserRole::FileDownload) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let media = match app.book_media_reader.book_media(&book_id).await {
        Ok(Some(media)) => media,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    match user_can_access_book_media(
        app.book_media_reader.as_ref(),
        &book_id,
        &current_user,
        &media,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return StatusCode::FORBIDDEN.into_response(),
        Err(error) => return internal_error_response(error),
    }

    let mut file_name = media.file_name.clone();
    let mut media_type = media.media_type.clone();

    let convert_kepub = query.convert_kepub.unwrap_or(false);
    let media_is_mobi = media.media_type == "application/x-mobipocket-ebook";
    let body = if convert_kepub && media.media_type == "application/epub+zip" {
        match app.content_resolver.media_file_exists(&media.file_path) {
            Ok(true) => {}
            Ok(false) => return missing_kobo_file_response(),
            Err(error) => return internal_error_response(error),
        }

        match convert_epub_to_kepub_bytes(&media.file_path) {
            Ok(converted_body) => {
                file_name = kobo_kepub_file_name(media.file_name.as_str());
                media_type = "application/epub+zip".to_string();
                persist_kepub_projection(
                    app.identity.device_sync(),
                    &book_id,
                    converted_body.len() as u64,
                )
                .await;
                converted_body
            }
            Err(_) => {
                return kepub_conversion_failed_response();
            }
        }
    } else if media_is_mobi {
        let normalized = match app
            .content_resolver
            .read_epub_publication_bytes(&media)
            .await
        {
            Ok(Some(body)) => body,
            Ok(None) => return missing_kobo_file_response(),
            Err(error) => return internal_error_response(error),
        };
        if convert_kepub {
            match komga_kepubify::convert_epub_bytes(&normalized) {
                Ok(converted_body) => {
                    file_name = kobo_kepub_file_name(media.file_name.as_str());
                    media_type = "application/epub+zip".to_string();
                    persist_kepub_projection(
                        app.identity.device_sync(),
                        &book_id,
                        converted_body.len() as u64,
                    )
                    .await;
                    converted_body
                }
                Err(_) => {
                    return kepub_conversion_failed_response();
                }
            }
        } else {
            file_name = kobo_epub_file_name(media.file_name.as_str());
            media_type = "application/epub+zip".to_string();
            normalized
        }
    } else {
        match app
            .content_resolver
            .read_media_file_bytes(&media.file_path)
            .await
        {
            Ok(Some(body)) => body,
            Ok(None) => return missing_kobo_file_response(),
            Err(error) => return internal_error_response(error),
        }
    };

    let content_disposition = attachment_disposition(&file_name);

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&media_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(content_disposition.as_str())
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{kobo_epub_file_name, kobo_kepub_file_name};

    #[test]
    fn normalized_mobi_download_uses_an_epub_file_name() {
        assert_eq!(kobo_epub_file_name("book.mobi"), "book.epub");
    }

    #[test]
    fn normalized_mobi_kepub_download_uses_a_kepub_epub_file_name() {
        assert_eq!(kobo_kepub_file_name("book.mobi"), "book.kepub.epub");
    }
}
