use super::shared::{
    load_collection_mosaic_bytes, mosaic_dimensions, mosaic_inflight_finish, mosaic_inflight_start,
    parse_thumbnail_upload, response_from_thumbnail_bytes, response_from_thumbnail_jpeg_bytes,
    set_one_hour_private_cache_control, thumbnail_dimensions,
};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Multipart;

use crate::cache::asset_ok_response;
use crate::contracts::media_assets::CollectionThumbnailDto;
use crate::identity_access::auth::{Admin, Authenticated};
use crate::state::MediaAssetsState;

use super::super::access_control::{
    user_can_access_collection_media, visible_collection_series_ids_for_user,
};
use super::super::http_helpers::internal_error_response;

async fn ensure_collection_exists(
    app: &MediaAssetsState,
    collection_id: &str,
) -> Result<(), Box<Response>> {
    match app.thumbnail_reader.collection_exists(collection_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Box::new(StatusCode::NOT_FOUND.into_response())),
        Err(error) => Err(Box::new(internal_error_response(error))),
    }
}

async fn generate_collection_mosaic(
    app: &MediaAssetsState,
    headers: &HeaderMap,
    collection_id: &str,
    visible_series_ids: Vec<String>,
) -> Response {
    match load_collection_mosaic_bytes(app, visible_series_ids).await {
        Ok(Some(bytes)) => {
            let (width, height) = mosaic_dimensions();
            if let Err(error) = app
                .thumbnails
                .insert_generated_collection(collection_id, &bytes, "image/jpeg", width, height)
                .await
            {
                tracing::warn!(
                    collection_id,
                    %error,
                    "failed to persist generated collection thumbnail"
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

pub(crate) async fn collection_thumbnail(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    headers: HeaderMap,
    Path(collection_id): Path<String>,
) -> Response {
    let visible_series_ids =
        match visible_collection_series_ids_for_user(&app, &collection_id, &user).await {
            Ok(series_ids) if !series_ids.is_empty() => series_ids,
            Ok(_) => return StatusCode::NOT_FOUND.into_response(),
            Err(error) => return internal_error_response(error),
        };

    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }

    match app
        .thumbnail_reader
        .collection_thumbnails(&collection_id)
        .await
    {
        Ok(rows) => {
            if let Some(thumbnail) = rows.into_iter().next() {
                let mut response =
                    response_from_thumbnail_jpeg_bytes(&headers, thumbnail.thumbnail);
                set_one_hour_private_cache_control(&mut response);
                return response;
            }

            // No persisted thumbnail: generate once, deduplicated across
            // concurrent requests for the same collection, and persist the result.
            let key = format!("collection-{collection_id}");
            let (should_generate, notify) = mosaic_inflight_start(&key);
            if should_generate {
                let response = generate_collection_mosaic(
                    &app,
                    &headers,
                    &collection_id,
                    visible_series_ids,
                )
                .await;
                mosaic_inflight_finish(&key);
                return response;
            }
            if let Some(notify) = notify {
                // Another request is generating it; wait, then re-read the
                // persisted thumbnail.
                notify.notified().await;
                return match app
                    .thumbnail_reader
                    .collection_thumbnails(&collection_id)
                    .await
                {
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
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn collection_thumbnails(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    Path(collection_id): Path<String>,
) -> Response {
    match user_can_access_collection_media(&app, &collection_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }

    match app
        .thumbnail_reader
        .collection_thumbnails(&collection_id)
        .await
    {
        Ok(rows) => Json(
            rows.iter()
                .map(CollectionThumbnailDto::from_record)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn collection_thumbnail_by_id(
    State(app): State<MediaAssetsState>,
    Authenticated(user): Authenticated,
    Path((collection_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    match user_can_access_collection_media(&app, &collection_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }

    match app
        .thumbnail_reader
        .collection_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.collection_id != collection_id => {
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

pub(crate) async fn collection_thumbnail_upload(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path(collection_id): Path<String>,
    multipart: Multipart,
) -> Response {
    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }
    match user_can_access_collection_media(&app, &collection_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    let upload = match parse_thumbnail_upload(multipart, "collection").await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let Some(dimensions) = thumbnail_dimensions(&upload.bytes) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };

    match app
        .thumbnails
        .insert_collection(
            &collection_id,
            &upload.bytes,
            upload.media_type.as_str(),
            dimensions.width,
            dimensions.height,
            upload.selected,
        )
        .await
    {
        Ok(thumbnail) => Json(CollectionThumbnailDto::from_record(&thumbnail)).into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn collection_thumbnail_select(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path((collection_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }
    match user_can_access_collection_media(&app, &collection_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }

    match app
        .thumbnail_reader
        .collection_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.collection_id != collection_id => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Ok(_) => {}
        Err(error) => return internal_error_response(error),
    }

    match app.thumbnails.select_collection(&thumbnail_id).await {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(error) => internal_error_response(error),
    }
}

pub(crate) async fn collection_thumbnail_delete(
    State(app): State<MediaAssetsState>,
    Admin(user): Admin,
    Path((collection_id, thumbnail_id)): Path<(String, String)>,
) -> Response {
    if let Err(response) = ensure_collection_exists(&app, &collection_id).await {
        return *response;
    }
    match user_can_access_collection_media(&app, &collection_id, &user).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }
    match app
        .thumbnail_reader
        .collection_thumbnail_by_id(&thumbnail_id)
        .await
    {
        Ok(Some(thumbnail)) if thumbnail.collection_id != collection_id => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return internal_error_response(error),
    }
    match app
        .thumbnails
        .delete_collection(&collection_id, &thumbnail_id)
        .await
    {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => internal_error_response(error),
    }
}
