use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::{Multipart, multipart::MultipartRejection};
use komga_application::discovery::{
    ReadListReadModel, ReadlistMutationError, ReadlistMutationInput, parse_comicrack_readlist,
};
use komga_domain::discovery::PageEnvelope;
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use super::BookDetailReadModel;
use super::detail_utils::internal_error_response;
use super::readlists_support::{merge_readlist_write_input, readlists_page_payload};
use crate::contracts::common::{PageDto, SpringErrorDto, ViolationDto};
use crate::contracts::discovery::{BookDto, ReadListDto, ReadListRequestMatchDto};
use crate::discovery::query::{resolve_readlist_books_query, resolve_readlists_query};
use crate::helpers::{to_domain_query_context, validation_error_response};
use crate::identity_access::auth::{Admin, Authenticated};
use crate::state::DiscoveryState;

fn readlist_response(readlist: &ReadListReadModel) -> Response {
    match ReadListDto::from_read_model(readlist) {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => internal_error_response(format!("{error:#}")),
    }
}

pub(crate) async fn readlists(
    State(app): State<DiscoveryState>,
    _: Authenticated,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let query = resolve_readlists_query(&uri);
    let paged = !query.unpaged;

    let requested_context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(
            &app.identity,
            &headers,
            query.library_ids.as_deref(),
        )
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let visibility_context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(&app.identity, &headers, None)
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let page = match app
        .persisted_sets
        .list_readlists(
            &to_domain_query_context(requested_context),
            &to_domain_query_context(visibility_context),
            query,
        )
        .await
    {
        Ok(page) => page,
        Err(error) => return internal_error_response(error),
    };

    match readlists_page_payload(page, paged) {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => internal_error_response(format!("{error:#}")),
    }
}

pub(crate) async fn readlist_create(
    State(app): State<DiscoveryState>,
    _: Admin,
    body: Bytes,
) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            return readlist_create_bad_request("Request body must be a JSON object");
        }
    };
    let input = match parse_readlist_create_input(&payload) {
        Ok(input) => input,
        Err(response) => return *response,
    };

    let created = match app.persisted_sets.create_readlist(input).await {
        Ok(created) => created,
        Err(error) => return readlist_mutation_error_response(error, "/api/v1/readlists"),
    };

    match app
        .persisted_sets
        .readlist_for_mutation(&created.readlist_id)
        .await
    {
        Ok(Some(readlist)) => readlist_response(&readlist),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}

fn parse_readlist_create_input(payload: &Value) -> Result<ReadlistMutationInput, Box<Response>> {
    let Some(payload) = payload.as_object() else {
        return Err(Box::new(readlist_create_bad_request(
            "Request body must be a JSON object",
        )));
    };

    let name = match payload.get("name") {
        Some(value) => match value.as_str() {
            Some(value) => value,
            None => {
                return Err(Box::new(readlist_create_bad_request(
                    "name must be a string",
                )));
            }
        },
        None => {
            return Err(Box::new(readlist_create_bad_request(
                "Required field 'name' is not present",
            )));
        }
    };
    let summary = match payload.get("summary") {
        Some(value) => match value.as_str() {
            Some(value) => value,
            None => {
                return Err(Box::new(readlist_create_bad_request(
                    "summary must be a string",
                )));
            }
        },
        None => "",
    };
    let ordered = match payload.get("ordered") {
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                return Err(Box::new(readlist_create_bad_request(
                    "ordered must be a boolean",
                )));
            }
        },
        None => true,
    };
    let book_values = match payload.get("bookIds") {
        Some(value) => match value.as_array() {
            Some(value) => value,
            None => {
                return Err(Box::new(readlist_create_bad_request(
                    "bookIds must be an array",
                )));
            }
        },
        None => {
            return Err(Box::new(readlist_create_bad_request(
                "Required field 'bookIds' is not present",
            )));
        }
    };

    let mut violations = Vec::new();
    if name.trim().is_empty() {
        violations.push(ViolationDto {
            field_name: Some("name".to_string()),
            message: Some("must not be blank".to_string()),
        });
    }
    if book_values.is_empty() {
        violations.push(ViolationDto {
            field_name: Some("bookIds".to_string()),
            message: Some("must not be empty".to_string()),
        });
    }

    let mut seen_book_ids = BTreeSet::new();
    let mut book_ids = Vec::with_capacity(book_values.len());
    let mut saw_duplicate_book_id = false;
    for value in book_values {
        let Some(book_id) = value.as_str() else {
            return Err(Box::new(readlist_create_bad_request(
                "bookIds must be an array of strings",
            )));
        };
        let book_id = book_id.to_string();
        if !seen_book_ids.insert(book_id.clone()) {
            saw_duplicate_book_id = true;
            continue;
        }
        book_ids.push(book_id);
    }

    if saw_duplicate_book_id {
        violations.push(ViolationDto {
            field_name: Some("bookIds".to_string()),
            message: Some("must only contain unique elements".to_string()),
        });
    }

    if !violations.is_empty() {
        return Err(Box::new(validation_error_response(violations)));
    }

    Ok(ReadlistMutationInput {
        name: name.to_string(),
        summary: summary.to_string(),
        ordered,
        book_ids,
    })
}

fn readlist_create_bad_request(message: &str) -> Response {
    readlist_bad_request(message, "/api/v1/readlists")
}

fn readlist_bad_request(message: &str, path: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(SpringErrorDto {
            error: "Bad Request".to_string(),
            message: message.to_string(),
            path: path.to_string(),
            status: 400,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }),
    )
        .into_response()
}

fn readlist_mutation_error_response(error: ReadlistMutationError, path: &str) -> Response {
    match error {
        ReadlistMutationError::DuplicateName => {
            readlist_bad_request("Read list name already exists", path)
        }
        ReadlistMutationError::Persistence(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_match_comicrack(
    State(app): State<DiscoveryState>,
    _: Admin,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let xml = match extract_comicrack_upload_xml(multipart).await {
        Ok(xml) => xml,
        Err(response) => return response,
    };

    let request = match parse_comicrack_readlist(&xml) {
        Ok(request) => request,
        Err(error) => return comicrack_bad_request_response(error.error_code()),
    };

    match app.persisted_sets.match_comicrack_readlist(&request).await {
        Ok(result) => match ReadListRequestMatchDto::from_result(&result) {
            Ok(payload) => Json(payload).into_response(),
            Err(error) => internal_error_response(format!("{error:#}")),
        },
        Err(error) => internal_error_response(error),
    }
}

fn payload_too_large_response() -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(SpringErrorDto {
            error: "Payload Too Large".to_string(),
            message: "Request payload is too large".to_string(),
            path: "/api/v1/readlists/match/comicrack".to_string(),
            status: 413,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }),
    )
        .into_response()
}

fn comicrack_bad_request_response(error_code: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(SpringErrorDto {
            error: "Bad Request".to_string(),
            message: error_code.to_string(),
            path: "/api/v1/readlists/match/comicrack".to_string(),
            status: 400,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }),
    )
        .into_response()
}

/// When the caller carries content restrictions, refuse to mutate a read list
/// whose members are not all visible to them (parity with the Kotlin backend
/// passing `principal.user.restrictions` into the mutation load). Callers with
/// no restrictions are unaffected.
async fn ensure_readlist_visible_for_mutation(
    app: &DiscoveryState,
    headers: &HeaderMap,
    readlist_id: &str,
    full_book_ids: &[String],
) -> Result<(), Response> {
    let context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(&app.identity, headers, None)
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return Err(StatusCode::UNAUTHORIZED.into_response()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    let domain_context = to_domain_query_context(context);
    if domain_context.restrictions.is_none() {
        return Ok(());
    }
    let visible = match app
        .persisted_sets
        .visible_readlist_book_ids(&domain_context, readlist_id)
        .await
    {
        Ok(Some(ids)) => ids,
        Ok(None) => return Err(StatusCode::NOT_FOUND.into_response()),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    if visible.len() != full_book_ids.len() {
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    Ok(())
}

pub(crate) async fn readlist_update(
    State(app): State<DiscoveryState>,
    _: Admin,
    headers: HeaderMap,
    Path(readlist_id): Path<String>,
    body: Bytes,
) -> Response {
    let path = format!("/api/v1/readlists/{readlist_id}");
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => {
            return readlist_bad_request("Request body must be a JSON object", path.as_str());
        }
    };
    let Some(existing) = (match app.persisted_sets.readlist_for_mutation(&readlist_id).await {
        Ok(readlist) => readlist,
        Err(error) => return internal_error_response(error),
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(response) =
        ensure_readlist_visible_for_mutation(&app, &headers, &readlist_id, &existing.book_ids).await
    {
        return response;
    }
    let input = merge_readlist_write_input(&existing, &payload);

    match app
        .persisted_sets
        .update_readlist(&readlist_id, input)
        .await
    {
        Ok(true) => {
            // Membership changed: invalidate any auto-generated (mosaic)
            // thumbnail so the next request rebuilds it from current members.
            // User-uploaded thumbnails are kept.
            if payload.get("bookIds").is_some() {
                if let Err(error) = app
                    .thumbnails
                    .delete_generated_readlist(&readlist_id)
                    .await
                {
                    tracing::warn!(
                        readlist_id,
                        %error,
                        "failed to invalidate generated readlist thumbnail"
                    );
                }
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => readlist_mutation_error_response(error, path.as_str()),
    }
}

pub(crate) async fn readlist_delete(
    State(app): State<DiscoveryState>,
    _: Admin,
    headers: HeaderMap,
    Path(readlist_id): Path<String>,
) -> Response {
    let full_book_ids = match app.persisted_sets.readlist_for_mutation(&readlist_id).await {
        Ok(Some(readlist)) => readlist.book_ids,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    };
    if let Err(response) =
        ensure_readlist_visible_for_mutation(&app, &headers, &readlist_id, &full_book_ids).await
    {
        return response;
    }
    match app.persisted_sets.delete_readlist(&readlist_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_books(
    State(app): State<DiscoveryState>,
    _: Authenticated,
    headers: HeaderMap,
    Path(readlist_id): Path<String>,
    uri: Uri,
) -> Response {
    let query = match resolve_readlist_books_query(readlist_id, &uri) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let response_context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(
            &app.identity,
            &headers,
            query.library_ids.as_deref(),
        )
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let visibility_context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(&app.identity, &headers, None)
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let paged = !query.unpaged;
    let page = match app
        .persisted_sets
        .list_readlist_books(&to_domain_query_context(visibility_context), query)
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    };

    match book_details_page_payload(page, response_context.is_admin, paged) {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_detail(
    State(app): State<DiscoveryState>,
    _: Authenticated,
    headers: HeaderMap,
    Path(readlist_id): Path<String>,
) -> Response {
    let context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(&app.identity, &headers, None)
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    match app
        .persisted_sets
        .readlist_detail(&to_domain_query_context(context), &readlist_id)
        .await
    {
        Ok(Some(readlist)) => readlist_response(&readlist),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_book_sibling_previous(
    State(app): State<DiscoveryState>,
    _: Authenticated,
    headers: HeaderMap,
    Path((readlist_id, book_id)): Path<(String, String)>,
) -> Response {
    sibling_response(&app, &headers, &readlist_id, &book_id, false).await
}

pub(crate) async fn readlist_book_sibling_next(
    State(app): State<DiscoveryState>,
    _: Authenticated,
    headers: HeaderMap,
    Path((readlist_id, book_id)): Path<(String, String)>,
) -> Response {
    sibling_response(&app, &headers, &readlist_id, &book_id, true).await
}

async fn sibling_response(
    app: &DiscoveryState,
    headers: &HeaderMap,
    readlist_id: &str,
    book_id: &str,
    next: bool,
) -> Response {
    let context = match app
        .discovery_auth
        .resolve_query_context_with_persistence(&app.identity, headers, None)
        .await
    {
        Ok(Some(context)) => context,
        Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let is_admin = context.is_admin;
    let sibling = match app
        .persisted_sets
        .readlist_book_sibling(
            &to_domain_query_context(context),
            readlist_id,
            book_id,
            next,
        )
        .await
    {
        Ok(Some(sibling)) => sibling,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    };

    match BookDto::from_read_model(&sibling, is_admin) {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => internal_error_response(error),
    }
}

fn book_details_page_payload(
    page: PageEnvelope<BookDetailReadModel>,
    is_admin: bool,
    paged: bool,
) -> anyhow::Result<PageDto<BookDto>> {
    let content = page
        .content
        .iter()
        .map(|book| BookDto::from_read_model(book, is_admin))
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(PageDto::from_parts(
        content,
        page.page,
        page.size,
        page.total_elements,
        page.total_pages,
        paged,
        true,
    ))
}

async fn extract_comicrack_upload_xml(
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Vec<u8>, Response> {
    let mut multipart = match multipart {
        Ok(multipart) => multipart,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(payload_too_large_response());
        }
        Err(rejection) => return Err(comicrack_bad_request_response(&rejection.body_text())),
    };

    loop {
        let field = multipart
            .next_field()
            .await
            .map_err(|error| comicrack_multipart_error_response(error))?;
        let Some(field) = field else {
            return Err(comicrack_bad_request_response(
                "Required request part 'file' is not present",
            ));
        };

        if field.name() != Some("file") {
            continue;
        }

        let bytes = field
            .bytes()
            .await
            .map_err(|error| comicrack_multipart_error_response(error))?;
        if bytes.is_empty() {
            return Err(comicrack_bad_request_response("ERR_1015"));
        }
        if bytes.len() as u64 > crate::operational::MAX_UPLOAD_FILE_SIZE_BYTES {
            return Err(payload_too_large_response());
        }

        return Ok(bytes.to_vec());
    }
}

fn comicrack_multipart_error_response(error: axum_extra::extract::multipart::MultipartError) -> Response {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        payload_too_large_response()
    } else {
        comicrack_bad_request_response(&error.body_text())
    }
}
