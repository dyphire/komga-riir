use axum::Json;
use axum::body::Bytes;
use axum::extract::Path as AxumPath;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use komga_application::operational::{
    TransientBookAnalyzeError, TransientBookPageError, TransientBookScanError,
};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::contracts::common::SpringErrorDto;
use crate::contracts::transient_books::TransientBookErrorDto;
use crate::identity_access::auth::Admin;
use crate::state::OperationalApiState;

mod payload;

use payload::transient_book_dto;

const TRANSIENT_BOOKS_PATH: &str = "/api/v1/transient-books";

fn transient_books_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(SpringErrorDto {
            error: "Bad Request".to_string(),
            message: message.to_string(),
            path: TRANSIENT_BOOKS_PATH.to_string(),
            status: 400,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }),
    )
        .into_response()
}

fn transient_books_error_response(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(TransientBookErrorDto {
            error: error.to_string(),
        }),
    )
        .into_response()
}

pub(crate) async fn post_transient_books(
    State(app): State<OperationalApiState>,
    _admin: Admin,
    body: Bytes,
) -> Response {
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(requested_path) = payload.get("path").and_then(Value::as_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let records = match app.transient_books.scan(requested_path).await {
        Ok(records) => records,
        Err(TransientBookScanError::BadRequest(error_code)) => {
            return transient_books_bad_request(&error_code);
        }
        Err(TransientBookScanError::Internal) => {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut payload = match records
        .iter()
        .map(transient_book_dto)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(?error, "transient book response mapping failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    payload.sort_by(|left, right| left.url.cmp(&right.url));
    Json(payload).into_response()
}

pub(crate) async fn post_transient_book_analyze(
    State(app): State<OperationalApiState>,
    _admin: Admin,
    AxumPath(transient_book_id): AxumPath<String>,
) -> Response {
    match app.transient_books.analyze(&transient_book_id).await {
        Ok(record) => match transient_book_dto(&record) {
            Ok(payload) => Json(payload).into_response(),
            Err(error) => {
                tracing::error!(?error, "transient book response mapping failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        },
        Err(TransientBookAnalyzeError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(TransientBookAnalyzeError::Internal) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) async fn get_transient_book_page(
    State(app): State<OperationalApiState>,
    _admin: Admin,
    headers: HeaderMap,
    AxumPath((transient_book_id, page_number)): AxumPath<(String, i32)>,
) -> Response {
    let content = match app
        .transient_books
        .page_content(&transient_book_id, page_number)
    {
        Ok(content) => content,
        Err(TransientBookPageError::NotFound) => return StatusCode::NOT_FOUND.into_response(),
        Err(TransientBookPageError::AnalysisFailed) => {
            return transient_books_error_response(StatusCode::NOT_FOUND, "Book analysis failed");
        }
        Err(TransientBookPageError::FileMissing) => {
            return transient_books_error_response(
                StatusCode::NOT_FOUND,
                "File not found, it may have moved",
            );
        }
        Err(TransientBookPageError::BadPageNumber) => {
            return transient_books_error_response(
                StatusCode::BAD_REQUEST,
                "Page number does not exist",
            );
        }
        Err(TransientBookPageError::Internal) => {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Conditional GET: 304 when the client's If-Modified-Since is not older
    // than the source file's last-modified time (parity with Kotlin).
    let last_modified = app
        .transient_books
        .file_last_modified_nanos(&transient_book_id)
        .and_then(|nanos| {
            let seconds = u64::try_from(nanos / 1_000_000_000).ok()?;
            let subsec_nanos = u32::try_from(nanos % 1_000_000_000).ok()?;
            crate::cache::format_http_date(
                std::time::UNIX_EPOCH + std::time::Duration::new(seconds, subsec_nanos),
            )
        });
    if let Some(last_modified) = last_modified.as_deref()
        && crate::cache::if_modified_since_matches(&headers, last_modified)
    {
        return crate::cache::asset_not_modified_response(None, Some(last_modified));
    }

    crate::cache::asset_ok_response(
        &content.content_type,
        content.bytes,
        None,
        last_modified.as_deref(),
    )
}
