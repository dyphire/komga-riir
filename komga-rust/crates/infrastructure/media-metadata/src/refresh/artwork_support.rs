use std::io::{Cursor, ErrorKind};
use std::path::Path;

use komga_application::media_assets::BookMediaRecord;
use komga_domain::media_assets::ThumbnailType;
use pdfium_render::prelude::*;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use tokio::fs;

use crate::codecs::parse_thumbnail_type;
use komga_infrastructure_base::resolve_rooted_path;
use komga_infrastructure_media_core::formats::pdfium::load_pdfium;

pub(super) struct RenderedThumbnail {
    pub(super) bytes: Vec<u8>,
    pub(super) media_type: String,
    pub(super) width: i64,
    pub(super) height: i64,
}

pub(super) async fn book_thumbnail_housekeeping(
    tx: &mut Transaction<'_, Sqlite>,
    book_id: &str,
    library_root: &Path,
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT ID, URL, THUMBNAIL, SELECTED
        FROM THUMBNAIL_BOOK
        WHERE BOOK_ID = ?
        ORDER BY LAST_MODIFIED_DATE DESC, ID ASC
        "#,
    )
    .bind(book_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to load thumbnails for '{book_id}' housekeeping: "
        ))
    })?;

    let mut retained_ids = Vec::new();
    let mut selected_ids = Vec::new();
    for row in rows {
        let thumbnail_id = row.get::<String, _>("ID");
        let thumbnail_url = row.get::<Option<String>, _>("URL");
        let thumbnail_blob = row.get::<Option<Vec<u8>>, _>("THUMBNAIL");
        let selected = row.get::<bool, _>("SELECTED");

        let blob_exists = thumbnail_blob
            .as_ref()
            .is_some_and(|thumbnail| !thumbnail.is_empty());
        let url_exists = if blob_exists {
            false
        } else if let Some(url) = thumbnail_url.as_deref() {
            let resolved = resolve_rooted_path(library_root, url);
            match fs::metadata(&resolved).await {
                Ok(_) => true,
                Err(error) if error.kind() == ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(anyhow::anyhow!(format!(
                        "failed to inspect thumbnail URL '{}' for '{book_id}': {error}",
                        resolved.display()
                    )));
                }
            }
        } else {
            false
        };

        let exists = blob_exists || url_exists;

        if !exists {
            sqlx::query("DELETE FROM THUMBNAIL_BOOK WHERE ID = ?")
                .bind(&thumbnail_id)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "failed to delete invalid thumbnail '{thumbnail_id}' for '{book_id}': "
                    ))
                })?;
            continue;
        }

        if selected {
            selected_ids.push(thumbnail_id.clone());
        }
        retained_ids.push(thumbnail_id);
    }

    let Some(target_selected_id) = (if selected_ids.len() > 1 {
        selected_ids.into_iter().next()
    } else if selected_ids.is_empty() {
        retained_ids.into_iter().next()
    } else {
        None
    }) else {
        return Ok(());
    };

    sqlx::query(
        "UPDATE THUMBNAIL_BOOK SET SELECTED = CASE WHEN ID = ? THEN 1 ELSE 0 END WHERE BOOK_ID = ?",
    )
    .bind(target_selected_id)
    .bind(book_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to normalize selected thumbnail for '{book_id}': "
        ))
    })?;

    Ok(())
}

pub(super) fn render_generated_thumbnail_from_image_bytes(
    book_id: &str,
    thumbnail_bytes: &[u8],
    configured_max_edge: u32,
) -> anyhow::Result<RenderedThumbnail> {
    let image = image::load_from_memory(thumbnail_bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to decode generated thumbnail source for '{book_id}': "
        ))
    })?;
    let source_max_edge = image.width().max(image.height()).max(1);
    let effective_max_edge = configured_max_edge.min(source_max_edge);
    let resized = image.thumbnail(effective_max_edge, effective_max_edge);
    let width = i64::from(resized.width());
    let height = i64::from(resized.height());
    let mut output = Cursor::new(Vec::new());
    resized
        .write_to(&mut output, image::ImageFormat::Jpeg)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to encode generated thumbnail for '{book_id}': "
            ))
        })?;
    Ok(RenderedThumbnail {
        bytes: output.into_inner(),
        media_type: "image/jpeg".to_string(),
        width,
        height,
    })
}

pub(super) fn render_pdf_thumbnail(
    media: &BookMediaRecord,
    configured_max_edge: u32,
) -> anyhow::Result<Option<RenderedThumbnail>> {
    let pdfium = load_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(&media.file_path, None)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to load PDF for thumbnail generation '{}'",
                media.file_path.display()
            ))
        })?;
    let page = match document.pages().first() {
        Ok(page) => page,
        Err(PdfiumError::NoPagesInDocument) => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "failed to load first PDF page for thumbnail generation '{}': {error}",
                media.file_path.display()
            )));
        }
    };

    let rendered = page
        .render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(i32::try_from(configured_max_edge).unwrap_or(i32::MAX))
                .set_maximum_height(i32::try_from(configured_max_edge).unwrap_or(i32::MAX)),
        )
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to render PDF page for thumbnail generation '{}'",
                media.file_path.display()
            ))
        })?
        .as_image()
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to convert PDF render to image '{}'",
                media.file_path.display()
            ))
        })?
        .into_rgb8();

    let width = i64::from(rendered.width());
    let height = i64::from(rendered.height());
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rendered)
        .write_to(&mut output, image::ImageFormat::Jpeg)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to encode PDF thumbnail for '{}'",
                media.file_path.display()
            ))
        })?;

    Ok(Some(RenderedThumbnail {
        bytes: output.into_inner(),
        media_type: "image/jpeg".to_string(),
        width,
        height,
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MarkSelectedPreference {
    IfNoneOrGenerated,
    No,
}

pub(super) async fn load_book_local_artwork_urls(
    library_root: &Path,
    book_url: &str,
) -> anyhow::Result<Vec<String>> {
    let book_path = resolve_rooted_path(library_root, book_url);
    let Some(book_dir) = book_path.parent() else {
        return Ok(Vec::new());
    };
    let Some(book_base_name) = book_path.file_stem().and_then(|value| value.to_str()) else {
        return Ok(Vec::new());
    };

    let mut artwork_urls = Vec::new();
    let mut entries = fs::read_dir(book_dir).await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to scan local artwork directory '{}' for '{}'",
            book_dir.display(),
            book_url,
        ))
    })?;
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to read local artwork entry in '{}' for '{}'",
            book_dir.display(),
            book_url,
        ))
    })? {
        let file_type = entry.file_type().await.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to inspect local artwork entry in '{}' for '{}'",
                book_dir.display(),
                book_url,
            ))
        })?;
        let path = entry.path();
        if !file_type.is_file() || !supported_book_local_artwork_path(path.as_path()) {
            continue;
        }
        let Some(candidate_stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !book_local_artwork_name_matches(candidate_stem, book_base_name) {
            continue;
        }

        let relative_url = path
            .strip_prefix(library_root)
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to relativize local artwork '{}' against library root '{}'",
                    path.display(),
                    library_root.display(),
                ))
            })?
            .to_string_lossy()
            .replace('\\', "/");
        artwork_urls.push(relative_url);
    }

    Ok(artwork_urls)
}

pub(super) async fn load_series_local_artwork_urls(
    library_root: &Path,
    series_url: &str,
) -> anyhow::Result<Vec<String>> {
    let series_path = resolve_rooted_path(library_root, series_url);
    let mut artwork_urls = Vec::new();
    let mut entries = fs::read_dir(&series_path).await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to scan series local artwork directory '{}' for '{}'",
            series_path.display(),
            series_url,
        ))
    })?;
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to read series local artwork entry in '{}' for '{}'",
            series_path.display(),
            series_url,
        ))
    })? {
        let file_type = entry.file_type().await.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to inspect series local artwork entry in '{}' for '{}'",
                series_path.display(),
                series_url,
            ))
        })?;
        let path = entry.path();
        if !file_type.is_file() || !supported_book_local_artwork_path(path.as_path()) {
            continue;
        }
        let Some(candidate_stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !series_local_artwork_name_matches(candidate_stem) {
            continue;
        }

        let relative_url = path
            .strip_prefix(library_root)
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to relativize series local artwork '{}' against library root '{}'",
                    path.display(),
                    library_root.display(),
                ))
            })?
            .to_string_lossy()
            .replace('\\', "/");
        artwork_urls.push(relative_url);
    }

    Ok(artwork_urls)
}

fn series_local_artwork_name_matches(candidate_stem: &str) -> bool {
    matches!(
        candidate_stem.to_ascii_lowercase().as_str(),
        "cover" | "default" | "folder" | "poster" | "series"
    )
}

fn supported_book_local_artwork_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpeg") | Some("jpg") | Some("tbn") | Some("webp") | Some("gif")
    )
}

fn book_local_artwork_name_matches(candidate_stem: &str, book_base_name: &str) -> bool {
    let candidate_stem = candidate_stem.to_ascii_lowercase();
    let book_base_name = book_base_name.to_ascii_lowercase();
    if candidate_stem == book_base_name {
        return true;
    }

    candidate_stem
        .strip_prefix(&format!("{book_base_name}-"))
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

pub(super) async fn import_book_local_artwork_thumbnail(
    pool: &SqlitePool,
    book_id: &str,
    library_root: &Path,
    artwork_url: &str,
    selected_preference: MarkSelectedPreference,
) -> anyhow::Result<bool> {
    let artwork_path = library_root.join(artwork_url);
    let metadata = tokio::fs::metadata(&artwork_path).await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to read local artwork '{}' for book '{}'",
            artwork_path.display(),
            book_id,
        ))
    })?;
    let thumbnail_id = format!("thumbnail-book-sidecar:{book_id}:{artwork_url}");
    let selected = should_select_book_local_artwork(pool, book_id, selected_preference).await?;

    sqlx::query("DELETE FROM THUMBNAIL_BOOK WHERE BOOK_ID = ? AND TYPE = ? AND URL = ?")
        .bind(book_id)
        .bind(ThumbnailType::Sidecar.persisted_name())
        .bind(artwork_url)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to remove duplicated sidecar thumbnail '{}' for '{}'",
                artwork_url, book_id,
            ))
        })?;

    sqlx::query(
        r#"
        INSERT INTO THUMBNAIL_BOOK
            (ID, URL, SELECTED, TYPE, BOOK_ID, MEDIA_TYPE, FILE_SIZE, LAST_MODIFIED_DATE)
        VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
        "#,
    )
    .bind(&thumbnail_id)
    .bind(artwork_url)
    .bind(selected)
    .bind(ThumbnailType::Sidecar.persisted_name())
    .bind(book_id)
    .bind(media_type_from_sidecar_path(artwork_path.as_path()))
    .bind(metadata.len() as i64)
    .execute(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to insert local artwork '{}' for book '{}'",
            artwork_url, book_id,
        ))
    })?;

    if selected {
        sqlx::query(
            "UPDATE THUMBNAIL_BOOK SET SELECTED = CASE WHEN ID = ? THEN 1 ELSE 0 END WHERE BOOK_ID = ?",
        )
        .bind(&thumbnail_id)
        .bind(book_id)
        .execute(pool)
        .await
        .map_err(|error| { anyhow::anyhow!(error).context( format!(
                "failed to mark local artwork '{}' as selected for '{}'",
                artwork_url, book_id,
            ))
        })?;
    }

    Ok(selected)
}

pub(super) async fn import_series_local_artwork_thumbnail(
    pool: &SqlitePool,
    series_id: &str,
    library_root: &Path,
    artwork_url: &str,
    selected_preference: MarkSelectedPreference,
) -> anyhow::Result<bool> {
    let artwork_path = library_root.join(artwork_url);
    let metadata = tokio::fs::metadata(&artwork_path).await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to read series local artwork '{}' for '{}'",
            artwork_path.display(),
            series_id,
        ))
    })?;
    let thumbnail_id = format!("thumbnail-series-sidecar:{series_id}:{artwork_url}");
    let selected = should_select_series_local_artwork(pool, series_id, selected_preference).await?;

    sqlx::query("DELETE FROM THUMBNAIL_SERIES WHERE SERIES_ID = ? AND TYPE = ? AND URL = ?")
        .bind(series_id)
        .bind(ThumbnailType::Sidecar.persisted_name())
        .bind(artwork_url)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to remove duplicated series sidecar thumbnail '{}' for '{}'",
                artwork_url, series_id,
            ))
        })?;

    sqlx::query(
        r#"
        INSERT INTO THUMBNAIL_SERIES
            (ID, URL, SELECTED, TYPE, SERIES_ID, MEDIA_TYPE, FILE_SIZE, LAST_MODIFIED_DATE)
        VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
        "#,
    )
    .bind(&thumbnail_id)
    .bind(artwork_url)
    .bind(selected)
    .bind(ThumbnailType::Sidecar.persisted_name())
    .bind(series_id)
    .bind(media_type_from_sidecar_path(artwork_path.as_path()))
    .bind(metadata.len() as i64)
    .execute(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to insert series local artwork '{}' for '{}'",
            artwork_url, series_id,
        ))
    })?;

    if selected {
        sqlx::query(
            "UPDATE THUMBNAIL_SERIES SET SELECTED = CASE WHEN ID = ? THEN 1 ELSE 0 END WHERE SERIES_ID = ?",
        )
        .bind(&thumbnail_id)
        .bind(series_id)
        .execute(pool)
        .await
        .map_err(|error| { anyhow::anyhow!(error).context( format!(
                "failed to mark series local artwork '{}' as selected for '{}'",
                artwork_url, series_id,
            ))
        })?;
    }

    Ok(selected)
}

async fn should_select_book_local_artwork(
    pool: &SqlitePool,
    book_id: &str,
    selected_preference: MarkSelectedPreference,
) -> anyhow::Result<bool> {
    if selected_preference == MarkSelectedPreference::No {
        return Ok(false);
    }

    let selected_row = sqlx::query(
        "SELECT TYPE FROM THUMBNAIL_BOOK WHERE BOOK_ID = ? AND SELECTED = 1 ORDER BY ID ASC LIMIT 1",
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!(error).context( format!("failed to load selected thumbnail for '{}'", book_id)))?;

    let thumbnail_type =
        selected_row.map(|row| parse_thumbnail_type(&row.get::<String, _>("TYPE")));
    Ok(thumbnail_type.is_none_or(|value| value == ThumbnailType::Generated))
}

async fn should_select_series_local_artwork(
    pool: &SqlitePool,
    series_id: &str,
    selected_preference: MarkSelectedPreference,
) -> anyhow::Result<bool> {
    if selected_preference == MarkSelectedPreference::No {
        return Ok(false);
    }

    let selected_row = sqlx::query(
        "SELECT TYPE FROM THUMBNAIL_SERIES WHERE SERIES_ID = ? AND SELECTED = 1 ORDER BY ID ASC LIMIT 1",
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| { anyhow::anyhow!(error).context( format!(
            "failed to load selected series thumbnail for '{}'",
            series_id
        ))
    })?;

    let thumbnail_type =
        selected_row.map(|row| parse_thumbnail_type(&row.get::<String, _>("TYPE")));
    Ok(thumbnail_type.is_none_or(|value| value == ThumbnailType::Generated))
}

fn media_type_from_sidecar_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("tbn") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    }
}

pub(super) fn is_suitable_cover_image(bytes: &[u8]) -> bool {
    const MAX_IMAGE_SIZE: usize = 10 * 1024 * 1024;
    const MAX_DIMENSION: u32 = 5000;
    const WHITE_THRESHOLD: u8 = 240;
    const BLACK_THRESHOLD: u8 = 15;
    const SUITABLE_RATIO: f64 = 0.95;

    if bytes.len() > MAX_IMAGE_SIZE {
        return true;
    }

    let Ok(image) = image::load_from_memory(bytes) else {
        return false;
    };
    let width = image.width();
    let height = image.height();

    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return true;
    }

    let rgb_image = image.to_rgb8();
    let sample_step = (width.min(height) / 100).max(1);
    let cols = width.div_ceil(sample_step);
    let rows = height.div_ceil(sample_step);
    let total_samples = (cols as u64) * (rows as u64);

    let mut white_pixels = 0u64;
    let mut black_pixels = 0u64;

    for y in (0..height).step_by(sample_step as usize) {
        for x in (0..width).step_by(sample_step as usize) {
            let pixel = rgb_image.get_pixel(x, y);
            let r = pixel[0];
            let g = pixel[1];
            let b = pixel[2];

            if r > WHITE_THRESHOLD && g > WHITE_THRESHOLD && b > WHITE_THRESHOLD {
                white_pixels += 1;
            } else if r < BLACK_THRESHOLD && g < BLACK_THRESHOLD && b < BLACK_THRESHOLD {
                black_pixels += 1;
            }
        }
    }

    let total_samples = total_samples as f64;
    let white_ratio = white_pixels as f64 / total_samples;
    let black_ratio = black_pixels as f64 / total_samples;

    white_ratio < SUITABLE_RATIO && black_ratio < SUITABLE_RATIO
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use komga_application::media_assets::BookMediaRecord;
    use lopdf::{Document as PdfDocument, Object, dictionary};
    #[cfg(unix)]
    use sqlx::Row;

    #[cfg(unix)]
    use super::book_thumbnail_housekeeping;
    use super::render_pdf_thumbnail;
    #[cfg(unix)]
    use komga_infrastructure_test_support::BootstrappedBookFixture;

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn write_pdf_with_broken_first_page(path: &Path) {
        let mut document = PdfDocument::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();

        document.objects.insert(page_id, Object::Null);
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );

        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document
            .save(path)
            .expect("broken first-page PDF fixture should be saved");
    }

    #[test]
    fn render_pdf_thumbnail_propagates_first_page_load_errors() {
        let path = unique_temp_path("komga-pdf-thumbnail-broken-first-page");
        write_pdf_with_broken_first_page(&path);
        let media = BookMediaRecord {
            library_id: "library-1".to_string(),
            media_type: "application/pdf".to_string(),
            file_path: path.clone(),
            file_name: "broken.pdf".to_string(),
            page_count: 1,
        };

        let error = match render_pdf_thumbnail(&media, 300) {
            Ok(_) => panic!("broken first PDF page must not become a missing generated thumbnail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("failed to load first PDF page for thumbnail generation"),
            "unexpected PDF thumbnail error: {error}"
        );

        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn book_thumbnail_housekeeping_propagates_sidecar_metadata_errors() {
        let fixture = BootstrappedBookFixture::open("thumbnail-housekeeping-metadata-error").await;
        fixture.insert_library_series().await;
        fixture.insert_book("book-1").await;

        let library_root = fixture
            .db_path
            .with_extension("thumbnail-housekeeping-root");
        fs::create_dir_all(&library_root).expect("library root should be created");
        fs::write(library_root.join("blocked"), b"not a directory")
            .expect("blocking file should be written");

        sqlx::query(
            "INSERT INTO THUMBNAIL_BOOK (ID, BOOK_ID, URL, TYPE, SELECTED) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("sidecar-thumbnail")
        .bind("book-1")
        .bind("blocked/cover.jpg")
        .bind("SIDECAR")
        .bind(true)
        .execute(&fixture.pool)
        .await
        .expect("sidecar thumbnail row should be inserted");

        let mut tx = fixture
            .pool
            .begin()
            .await
            .expect("housekeeping transaction should start");
        let error = book_thumbnail_housekeeping(&mut tx, "book-1", &library_root)
            .await
            .expect_err("metadata error should be propagated");
        tx.rollback()
            .await
            .expect("housekeeping transaction should roll back");

        assert!(
            error
                .to_string()
                .contains("failed to inspect thumbnail URL")
        );
        assert!(error.to_string().contains("book-1"));

        let remaining = sqlx::query("SELECT COUNT(*) AS COUNT FROM THUMBNAIL_BOOK WHERE ID = ?")
            .bind("sidecar-thumbnail")
            .fetch_one(&fixture.pool)
            .await
            .expect("sidecar thumbnail row should be queryable")
            .get::<i64, _>("COUNT");
        assert_eq!(remaining, 1);

        fixture.close().await;
        let _ = fs::remove_dir_all(library_root);
    }
}
