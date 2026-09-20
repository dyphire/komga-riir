use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Multipart;
use image::ImageFormat;
use komga_application::media_assets::EntityThumbnailBinary;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Notify, Semaphore};

use crate::helpers::spring_error_response;
use crate::media_response_policy::MediaAssetResponse;
use crate::state::MediaAssetsState;

const MOSAIC_HEIGHT: u32 = 300;
const MOSAIC_RATIO: f32 = 0.70666664;

/// Pixel dimensions of the mosaic thumbnail as persisted in the DB.
pub(super) fn mosaic_dimensions() -> (i64, i64) {
    let height = MOSAIC_HEIGHT;
    let width = ((height as f32) * MOSAIC_RATIO).round() as u32;
    (i64::from(width), i64::from(height))
}

/// In-flight deduplication for mosaic generation, keyed by entity id
/// (e.g. `"readlist-<id>"` / `"collection-<id>"`). When several requests race
/// for the same missing thumbnail, only the first performs the (expensive)
/// generation; the others wait on the returned Notify and then re-read the
/// persisted thumbnail.
static GENERATED_THUMBNAIL_INFLIGHT: OnceLock<Mutex<HashMap<String, Arc<Notify>>>> =
    OnceLock::new();

/// Returns `(true, None)` when the caller should generate, or `(false, Some(notify))`
/// when another request is already generating for `key`.
pub(super) fn mosaic_inflight_start(key: &str) -> (bool, Option<Arc<Notify>>) {
    let mut map = GENERATED_THUMBNAIL_INFLIGHT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(notify) = map.get(key) {
        (false, Some(notify.clone()))
    } else {
        let notify = Arc::new(Notify::new());
        map.insert(key.to_string(), notify.clone());
        (true, None)
    }
}

/// Clears the in-flight marker for `key` and wakes any waiters.
pub(super) fn mosaic_inflight_finish(key: &str) {
    if let Ok(mut map) = GENERATED_THUMBNAIL_INFLIGHT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        if let Some(notify) = map.remove(key) {
            notify.notify_waiters();
        }
    }
}

pub(super) struct ThumbnailUpload {
    pub(super) bytes: Vec<u8>,
    pub(super) media_type: String,
    pub(super) selected: bool,
}

pub(super) struct ThumbnailDimensions {
    pub(super) width: i64,
    pub(super) height: i64,
}

pub(super) fn thumbnail_dimensions(bytes: &[u8]) -> Option<ThumbnailDimensions> {
    let image = image::load_from_memory(bytes).ok()?;
    Some(ThumbnailDimensions {
        width: i64::from(image.width()),
        height: i64::from(image.height()),
    })
}

fn repeated_thumbnail_source_ids(ids: Vec<String>) -> Vec<String> {
    let seed = ids.into_iter().take(4).collect::<Vec<_>>();
    if seed.is_empty() {
        return vec![];
    }

    let mut repeated = Vec::with_capacity(4);
    while repeated.len() < 4 {
        repeated.extend(seed.iter().cloned());
    }
    repeated.truncate(4);
    repeated
}

fn encode_mosaic_jpeg(image_bytes: &[Vec<u8>]) -> Option<Vec<u8>> {
    if image_bytes.is_empty() {
        return None;
    }

    let height = MOSAIC_HEIGHT;
    let width = ((height as f32) * MOSAIC_RATIO).round() as u32;
    let cell_width = (width / 2).max(1);
    let cell_height = (height / 2).max(1);
    let mut mosaic = image::RgbImage::new(width.max(1), height.max(1));
    let placements = [
        (0_i64, 0_i64),
        (i64::from(cell_width), 0_i64),
        (0_i64, i64::from(cell_height)),
        (i64::from(cell_width), i64::from(cell_height)),
    ];

    for (bytes, (x, y)) in image_bytes.iter().zip(placements) {
        let tile = image::load_from_memory(bytes)
            .ok()?
            .thumbnail(cell_width, cell_height)
            .to_rgb8();
        image::imageops::overlay(&mut mosaic, &tile, x, y);
    }

    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(mosaic)
        .write_to(&mut output, ImageFormat::Jpeg)
        .ok()?;
    Some(output.into_inner())
}

pub(super) fn encode_image_bytes_as_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let mut output = std::io::Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Jpeg).ok()?;
    Some(output.into_inner())
}

fn encode_image_bytes_as_small_jpeg(bytes: &[u8], max_edge: u32) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let resized = if image.width().max(image.height()) > max_edge {
        image.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    let mut output = std::io::Cursor::new(Vec::new());
    resized.write_to(&mut output, ImageFormat::Jpeg).ok()?;
    Some(output.into_inner())
}

pub(crate) fn response_from_thumbnail_bytes(
    headers: &HeaderMap,
    bytes: Vec<u8>,
    media_type: &str,
) -> Response {
    MediaAssetResponse::new(media_type, bytes)
        .with_etag()
        .into_response(Some(headers))
}

fn is_jpeg_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}

/// Serves persisted thumbnails without re-encoding them: most thumbnails are
/// already JPEG (analysis-time generated or mosaic), so decoding and re-encoding
/// on every request wasted CPU and allocated large transient buffers, stalling
/// the async workers under thumbnail-heavy scrolling. Non-JPEG bytes (e.g.
/// user-uploaded PNG) still go through the conversion path.
pub(crate) fn response_from_thumbnail_jpeg_bytes(headers: &HeaderMap, bytes: Vec<u8>) -> Response {
    if is_jpeg_bytes(&bytes) {
        return response_from_thumbnail_bytes(headers, bytes, "image/jpeg");
    }

    let Some(jpeg_bytes) = encode_image_bytes_as_jpeg(&bytes) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    response_from_thumbnail_bytes(headers, jpeg_bytes, "image/jpeg")
}

/// Runs the CPU-heavy mosaic composition on a blocking thread and bounds the
/// number of simultaneous compositions, so a burst of thumbnail requests cannot
/// stall unrelated API calls or balloon memory with concurrent image decodes.
async fn encode_mosaic_off_thread(images: Vec<Vec<u8>>) -> anyhow::Result<Option<Vec<u8>>> {
    static MOSAIC_ENCODE_LIMIT: OnceLock<Semaphore> = OnceLock::new();
    let semaphore = MOSAIC_ENCODE_LIMIT.get_or_init(|| Semaphore::new(2));
    let _permit = semaphore
        .acquire()
        .await
        .expect("mosaic encode semaphore should not be closed");
    Ok(tokio::task::spawn_blocking(move || encode_mosaic_jpeg(&images))
        .await
        .map_err(|error| anyhow::anyhow!("mosaic encode task failed: {error}"))?)
}

pub(crate) fn response_from_thumbnail_small_jpeg_bytes(
    headers: &HeaderMap,
    bytes: Vec<u8>,
    media_type: &str,
    max_edge: u32,
) -> Response {
    match encode_image_bytes_as_small_jpeg(&bytes, max_edge) {
        Some(jpeg_bytes) => response_from_thumbnail_bytes(headers, jpeg_bytes, "image/jpeg"),
        None => response_from_thumbnail_bytes(headers, bytes, media_type),
    }
}

pub(super) fn set_one_hour_private_cache_control(response: &mut Response) {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=3600, private"),
    );
}

/// Reads the persisted book thumbnail (analysis-time generated or user
/// uploaded) straight from the DB. Aligns with the Kotlin implementation,
/// which composes mosaics from persisted thumbnails and never touches the
/// source book file: books without a persisted thumbnail are skipped.
pub(super) async fn load_book_thumbnail_source_bytes(
    app: &MediaAssetsState,
    book_id: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(app
        .thumbnail_reader
        .selected_book_thumbnail(book_id)
        .await?
        .map(|thumbnail| thumbnail.thumbnail))
}

pub(super) async fn load_series_thumbnail(
    app: &MediaAssetsState,
    series_id: &str,
) -> anyhow::Result<Option<EntityThumbnailBinary>> {
    if let Some(thumbnail) = app
        .thumbnail_reader
        .selected_series_thumbnail(series_id)
        .await?
    {
        return Ok(Some(thumbnail));
    }

    let Some(book_id) = app
        .thumbnail_reader
        .series_book_ids(series_id)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };

    app.thumbnail_reader.selected_book_thumbnail(&book_id).await
}

pub(super) async fn load_series_thumbnail_source_bytes(
    app: &MediaAssetsState,
    series_id: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    match load_series_thumbnail(app, series_id).await {
        Ok(Some(thumbnail)) => Ok(Some(thumbnail.thumbnail)),
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) async fn load_readlist_mosaic_bytes(
    app: &MediaAssetsState,
    visible_book_ids: Vec<String>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let book_ids = repeated_thumbnail_source_ids(visible_book_ids);
    if book_ids.is_empty() {
        return Ok(None);
    }

    let mut images = Vec::new();
    for book_id in book_ids {
        if let Some(bytes) = load_book_thumbnail_source_bytes(app, &book_id).await? {
            images.push(bytes);
        }
    }

    encode_mosaic_off_thread(images).await
}

pub(super) async fn load_collection_mosaic_bytes(
    app: &MediaAssetsState,
    visible_series_ids: Vec<String>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let series_ids = repeated_thumbnail_source_ids(visible_series_ids);
    if series_ids.is_empty() {
        return Ok(None);
    }

    let mut images = Vec::new();
    for series_id in series_ids {
        if let Some(bytes) = load_series_thumbnail_source_bytes(app, &series_id).await? {
            images.push(bytes);
        }
    }

    encode_mosaic_off_thread(images).await
}

pub(super) async fn parse_thumbnail_upload(
    mut multipart: Multipart,
    entity_name: &str,
) -> Result<ThumbnailUpload, Box<Response>> {
    let mut image_bytes = None::<Vec<u8>>;
    let mut media_type = None::<String>;
    let mut selected = true;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return Err(Box::new(if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    thumbnail_upload_too_large_response()
                } else {
                    invalid_thumbnail_upload_response(entity_name, error)
                }));
            }
        };

        match field.name() {
            Some("file") => {
                let content_type = field.content_type().map(str::to_string);
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return Err(Box::new(if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                            thumbnail_upload_too_large_response()
                        } else {
                            invalid_thumbnail_upload_response(entity_name, error)
                        }));
                    }
                };
                if bytes.is_empty() {
                    return Err(Box::new(empty_thumbnail_upload_response(entity_name)));
                }
                if bytes.len() as u64 > crate::operational::MAX_UPLOAD_FILE_SIZE_BYTES {
                    return Err(Box::new(thumbnail_upload_too_large_response()));
                }

                let resolved_media_type =
                    match resolve_thumbnail_media_type(content_type.as_deref(), bytes.as_ref()) {
                        Some(media_type) => media_type,
                        None => {
                            return Err(Box::new(
                                StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
                            ));
                        }
                    };
                image_bytes = Some(bytes.to_vec());
                media_type = Some(resolved_media_type);
            }
            Some("selected") => {
                let value = match field.text().await {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(Box::new(invalid_thumbnail_upload_response(
                            entity_name,
                            error,
                        )));
                    }
                };
                selected = match value.trim().to_ascii_lowercase().as_str() {
                    "" | "true" => true,
                    "false" => false,
                    _ => {
                        return Err(Box::new(spring_error_response(
                            StatusCode::BAD_REQUEST,
                            format!("{entity_name} thumbnail selected field must be true or false"),
                        )));
                    }
                };
            }
            _ => {}
        }
    }

    let Some(bytes) = image_bytes else {
        return Err(Box::new(empty_thumbnail_upload_response(entity_name)));
    };
    let Some(media_type) = media_type else {
        return Err(Box::new(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response()));
    };

    Ok(ThumbnailUpload {
        bytes,
        media_type,
        selected,
    })
}

fn resolve_thumbnail_media_type(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    if let Some(content_type) = content_type
        && content_type.starts_with("image/")
    {
        return Some(content_type.to_string());
    }

    match image::guess_format(bytes).ok()? {
        ImageFormat::Jpeg => Some("image/jpeg".to_string()),
        ImageFormat::Png => Some("image/png".to_string()),
        ImageFormat::Gif => Some("image/gif".to_string()),
        ImageFormat::WebP => Some("image/webp".to_string()),
        ImageFormat::Avif => Some("image/avif".to_string()),
        _ => None,
    }
}

fn empty_thumbnail_upload_response(entity_name: &str) -> Response {
    spring_error_response(
        StatusCode::BAD_REQUEST,
        format!("{entity_name} thumbnail upload body must not be empty"),
    )
}

fn thumbnail_upload_too_large_response() -> Response {
    spring_error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Request payload is too large".to_string(),
    )
}

fn invalid_thumbnail_upload_response(entity_name: &str, error: impl std::fmt::Display) -> Response {
    spring_error_response(
        StatusCode::BAD_REQUEST,
        format!("invalid {entity_name} thumbnail upload: {error:#}"),
    )
}
