use super::shared::{
    load_readlist_mosaic_bytes, mosaic_dimensions, mosaic_inflight_finish, mosaic_inflight_start,
    parse_thumbnail_upload, response_from_thumbnail_bytes, response_from_thumbnail_jpeg_bytes,
    set_one_hour_private_cache_control, thumbnail_dimensions,
};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Multipart;

use crate::cache::asset_ok_response;
use crate::contracts::media_assets::ReadListThumbnailDto;
use crate::identity_access::auth::{Admin, Authenticated};
use crate::state::MediaAssetsState;

use super::super::access_control::{
    user_can_access_readlist_media, visible_readlist_book_ids_for_user,
};
use super::super::http_helpers::internal_error_response;

async fn ensure_readlist_exists(
    app: &MediaAssetsState,
    readlist_id: &str,
) -> Result<(), Box<Response>> {
    match app.thumbnail_reader.readlist_exists(readlist_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        Err(error) => Err(Box::new(internal_error_response(error))),
    }
}

async fn generate_readlist_mosaic(
    app: &MediaAssetsState,
    headers: &HeaderMap,
    readlist_id: &str,
    visible_book_ids: Vec<String>,
) -> Response {
    match load_readlist_mosaic_bytes(app, visible_book_ids).await {
        Ok(Some(bytes)) => {
            let (width, height) = mosaic_dimensions();
            if let Err(error) = app
                .thumbnails
                .insert_generated_readlist(readlist_id, &bytes, "image/jpeg", width, height)
                .await
            {
                tracing::warn!(
                    readlist_id,
                    %error,
                    "failed to persist generated readlist thumbnail"
                );
            }
            let mut response = response_from_thumbnail_bytes(headers, bytes, "image/jpeg");
            set_one_hour_private_cache_control(&mut response);
            response
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_thumbnail(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    headers: HeaderMap,
    Path(readlist_id): Path<String>,
) -> Response {
    let visible_book_ids = match visible_readlist_book_ids_for_user(&app, &readlist_id, &user).await
    {
        Ok(Some(book_ids)) if !book_ids.is_empty() => book_ids,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    };

    match app.thumbnail_reader.readlist_thumbnails(&readlist_id).await {
        Ok(rows) => {
            if let Some(thumbnail) = rows.into_iter().next() {
                let mut response =
                    response_from_thumbnail_jpeg_bytes(&headers, thumbnail.thumbnail);
                set_one_hour_private_cache_control(&mut response);
                return response;
            }

            // No persisted thumbnail: generate once, deduplicated across
            // concurrent requests for the same readlist, and persist the result.
            let key = format!("readlist-{readlist_id}");
            let (should_generate, notify) = mosaic_inflight_start(&key);
            if should_generate {
                let response =
                    generate_readlist_mosaic(&app, &headers, &readlist_id, visible_book_ids).await;
                mosaic_inflight_finish(&key);
                return response;
            }
            if let Some(notify) = notify {
                // Another request is generating it; wait, then re-read the
                // persisted thumbnail.
                notify.notified().await;
                return match app.thumbnail_reader.readlist_thumbnails(&readlist_id).await {
                    Ok(rows) => {
                        if let Some(thumbnail) = rows.into_iter().next() {
                            let mut response = response_from_thumbnail_jpeg_bytes(
                                &headers,
                                thumbnail.thumbnail,
                            );
                            set_one_hour_private_cache_control(&mut response);
                            response
                        } else {
                            StatusCode::NOT_FOUND.into_response()
                        }
                    }
                    Err(error) => internal_error_response(error),
                };
            }

            if let Err(error) = app.thumbnail_reader.readlist_exists(&readlist_id).await {
                return internal_error_response(error);
            }
        }
        Err(error) => return internal_error_response(error),
    }

    StatusCode::NOT_FOUND.into_response()
}

pub(crate) async fn readlist_thumbnails(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    Path(readlist_id): Path<String>,
) -> Response {
    match user_can_access_readlist_media(&app, &readlist_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    match app.thumbnail_reader.readlist_thumbnails(&readlist_id).await {
        Ok(rows) => {
            if !rows.is_empty() {
                return Json(
                    rows.iter()
                        .map(ReadListThumbnailDto::from_record)
                        .collect::<Vec<_>>(),
                )
                .into_response();
            }

            match app.thumbnail_reader.readlist_exists(&readlist_id).await {
                Ok(true) => return Json(Vec::<ReadListThumbnailDto>::new()).into_response(),
                Ok(false) => {}
                Err(error) => return internal_error_response(error),
            }
        }
        Err(error) => return internal_error_response(error),
    }

    StatusCode::NOT_FOUND.into_response()
}

pub(crate) async fn readlist_thumbnail_by_id(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    Path((readlist_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    match user_can_access_readlist_media(&app, &readlist_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    match app
        .thumbnail_reader
        .readlist_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.readlist_id != readlist_id => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Ok(Some(thumbnail)) => asset_ok_response(
            thumbnail.media_type.as_str(),
            thumbnail.thumbnail,
            None,
            None,
        ),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_thumbnail_upload(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path(readlist_id): Path<String>,
    multipart: Multipart,
) -> Response {
    if let Err(response) = ensure_readlist_exists(&app, &readlist_id).await {
        return *response;
    }
    match user_can_access_readlist_media(&app, &readlist_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    let upload = match parse_thumbnail_upload(multipart, "readlist").await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let Some(dimensions) = thumbnail_dimensions(&upload.bytes) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };

    match app
        .thumbnails
        .insert_readlist(
            &readlist_id,
            &upload.bytes,
            upload.media_type.as_str(),
            dimensions.width,
            dimensions.height,
            upload.selected,
        )
        .await
    {
        Ok(thumbnail) => Json(ReadListThumbnailDto::from_record(&thumbnail)).into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_thumbnail_select(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path((readlist_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = ensure_readlist_exists(&app, &readlist_id).await {
        return *response;
    }
    match user_can_access_readlist_media(&app, &readlist_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    match app
        .thumbnail_reader
        .readlist_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.readlist_id != readlist_id => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Ok(_) => {}
        Err(error) => return internal_error_response(error),
    }

    match app
        .thumbnails
        .select_readlist(&readlist_id, &thumbnail_id)
        .await
    {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn readlist_thumbnail_delete(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path((readlist_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = ensure_readlist_exists(&app, &readlist_id).await {
        return *response;
    }
    match user_can_access_readlist_media(&app, &readlist_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }
    match app
        .thumbnail_reader
        .readlist_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.readlist_id != readlist_id => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }
    match app
        .thumbnails
        .delete_readlist(&readlist_id, &thumbnail_id)
        .await
    {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}
