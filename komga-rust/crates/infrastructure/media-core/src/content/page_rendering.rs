use anyhow::Context;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

use image::GenericImageView;
use image::imageops::FilterType;
use komga_application::media_assets::{
    BookMediaRecord, BookPageRecord, ImageOutputFormat, MediaImageDimensions, RenderedImage,
    book_media_is_epub, book_media_is_pdf, book_media_is_rar_archive, book_media_is_single_image,
    book_media_is_zip_archive, content_type_from_filename, is_supported_page_image_file_name,
};
use komga_domain::discovery::compare_book_names;
use lopdf::Document as PdfDocument;
use pdfium_render::prelude::*;
use zip::ZipArchive;

use crate::formats::pdfium::load_pdfium;
use crate::formats::rar::{list_rar_entries, read_rar_entry_bytes};

// 300 PPI for an A4-sized page.
const PDF_MAX_RENDER_EDGE: u32 = 3_508;
const AVIF_ENCODING_SPEED: u8 = 7;
const AVIF_ENCODING_QUALITY: u8 = 80;

pub async fn resolve_book_page_bytes(
    media: &BookMediaRecord,
    page: &BookPageRecord,
    page_number: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    if book_media_is_pdf(media) {
        return render_pdf_page(media, page_number, ImageOutputFormat::Jpeg)
            .await
            .map(|rendered| rendered.map(|rendered| rendered.bytes));
    }

    let mut candidates = Vec::new();
    let media_path_is_directory = match tokio::fs::metadata(&media.file_path).await {
        Ok(metadata) => metadata.is_dir(),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "read media path metadata '{}': {error}",
                media.file_path.display()
            )));
        }
    };
    if !page.file_name.is_empty() {
        if media_path_is_directory {
            candidates.push(media.file_path.join(&page.file_name));
        }
        if let Some(parent) = media.file_path.parent() {
            candidates.push(parent.join(&page.file_name));
        }
    }
    if book_media_is_single_image(media) && page_number == 1 {
        candidates.push(media.file_path.clone());
    }
    for candidate in candidates {
        if let Some(bytes) = read_media_file_bytes(&candidate).await? {
            return Ok(Some(bytes));
        }
    }
    if book_media_is_epub(media) {
        return super::epub_resources::read_epub_resource_bytes(&media.file_path, &page.file_name)
            .await;
    }
    if let Some(bytes) = read_zip_archive_page_bytes(media, page, page_number).await? {
        return Ok(Some(bytes));
    }
    if let Some(bytes) = read_rar_archive_page_bytes(media, page, page_number).await? {
        return Ok(Some(bytes));
    }
    Ok(None)
}

pub async fn render_book_page_thumbnail(
    media: &BookMediaRecord,
    page: &BookPageRecord,
    page_number: u64,
    max_edge: u32,
    output_format: ImageOutputFormat,
) -> anyhow::Result<Option<RenderedImage>> {
    if book_media_is_pdf(media) {
        return render_pdf_page_thumbnail(media, page_number, max_edge, output_format).await;
    }

    let Some(bytes) = resolve_book_page_bytes(media, page, page_number).await? else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || render_image_thumbnail(bytes, max_edge, output_format))
        .await
        .context("join image thumbnail render task")?
        .map(Some)
}

pub async fn load_archive_page_row(
    media: &BookMediaRecord,
    page_number: u64,
) -> anyhow::Result<Option<BookPageRecord>> {
    if page_number == 0 {
        return Ok(None);
    }
    Ok(load_archive_page_rows(media)
        .await?
        .unwrap_or_default()
        .into_iter()
        .nth(usize::try_from(page_number - 1).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "convert archive page number {page_number} to index: "
            ))
        })?))
}

pub async fn load_archive_page_rows(
    media: &BookMediaRecord,
) -> anyhow::Result<Option<Vec<BookPageRecord>>> {
    if book_media_is_zip_archive(media) {
        return load_zip_archive_page_rows(media).await;
    }
    if book_media_is_rar_archive(media) {
        let file_path = media.file_path.clone();
        return tokio::task::spawn_blocking(move || load_rar_archive_page_rows(&file_path))
            .await
            .context("join rar archive row read task")?;
    }
    Ok(None)
}

pub fn load_pdf_page_row(
    media: &BookMediaRecord,
    page_number: u64,
) -> anyhow::Result<Option<BookPageRecord>> {
    if page_number == 0 {
        return Ok(None);
    }
    Ok(load_generated_pdf_page_rows(media)?.into_iter().nth(
        usize::try_from(page_number - 1).map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("convert pdf page number {page_number} to index"))
        })?,
    ))
}

pub fn load_generated_pdf_page_rows(
    media: &BookMediaRecord,
) -> anyhow::Result<Vec<BookPageRecord>> {
    if !book_media_is_pdf(media) {
        return Ok(vec![]);
    }
    let page_count = media.page_count;
    if page_count == 0 {
        return Ok(vec![]);
    }
    let document = PdfDocument::load(&media.file_path).map_err(|error| {
        anyhow::anyhow!(error).context(format!("open pdf '{}'", media.file_path.display()))
    })?;
    Ok((1..=page_count)
        .map(|number| {
            let dimensions = document
                .get_pages()
                .get(&(number as u32))
                .and_then(|_| pdf_page_dimensions(&document, number as u32));
            let scaled_dimensions = dimensions.map(scale_pdf_page_dimensions);

            BookPageRecord {
                number,
                file_name: number.to_string(),
                media_type: "image/jpeg".to_string(),
                width: scaled_dimensions.map(|dimensions| i64::from(dimensions.width)),
                height: scaled_dimensions.map(|dimensions| i64::from(dimensions.height)),
                file_size: -1,
            }
        })
        .collect())
}

pub async fn render_pdf_page(
    media: &BookMediaRecord,
    page_number: u64,
    output_format: ImageOutputFormat,
) -> anyhow::Result<Option<RenderedImage>> {
    if !book_media_is_pdf(media) || page_number == 0 {
        return Ok(None);
    }

    let media = media.clone();
    tokio::task::spawn_blocking(move || {
        render_pdf_page_blocking(&media, page_number, output_format)
    })
    .await
    .context("join PDF page render task")?
}

fn render_pdf_page_blocking(
    media: &BookMediaRecord,
    page_number: u64,
    output_format: ImageOutputFormat,
) -> anyhow::Result<Option<RenderedImage>> {
    // Resolve page dimensions from the Pdfium document itself instead of
    // re-parsing the whole PDF with lopdf: Pdfium reports the page size in the
    // same PDF points used by the MediaBox, so the single Pdfium load below
    // serves both sizing and rendering.
    let pdfium = load_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(&media.file_path, None)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "open pdf '{}' for page rendering: ",
                media.file_path.display()
            ))
        })?;
    let page_index = i32::try_from(page_number.saturating_sub(1)).map_err(|error| {
        anyhow::anyhow!(error).context(format!("convert pdf page number {page_number}"))
    })?;
    let page = match document.pages().get(page_index) {
        Ok(page) => page,
        Err(PdfiumError::PageIndexOutOfBounds) => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(error).context(format!(
                "load pdf page {page_number} from '{}'",
                media.file_path.display()
            )))
        }
    };

    let dimensions = PdfPageDimensions {
        width: page.width().value.round().max(0.0) as u32,
        height: page.height().value.round().max(0.0) as u32,
    };
    let dimensions = if dimensions.width > 0 && dimensions.height > 0 {
        scale_pdf_page_dimensions(dimensions)
    } else {
        PdfPageDimensions {
            width: PDF_MAX_RENDER_EDGE,
            height: PDF_MAX_RENDER_EDGE,
        }
    };

    let rendered = page
        .render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(i32::try_from(dimensions.width).unwrap_or(i32::MAX))
                .set_maximum_height(i32::try_from(dimensions.height).unwrap_or(i32::MAX)),
        )
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "render pdf page {page_number} from '{}'",
                media.file_path.display()
            ))
        })?
        .as_image()
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "convert pdf page {page_number} from '{}' to image: ",
                media.file_path.display()
            ))
        })?
        .into_rgb8();

    let image = image::DynamicImage::ImageRgb8(rendered);
    encode_image_with_jpeg_fallback(
        &image,
        output_format,
        &format!(
            "encode pdf page {page_number} page from '{}'",
            media.file_path.display()
        ),
    )
    .map(Some)
}

pub fn read_pdf_page_as_single_page_pdf(
    media: &BookMediaRecord,
    page_number: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !book_media_is_pdf(media) || page_number == 0 {
        return Ok(None);
    }
    let mut document = PdfDocument::load(&media.file_path).map_err(|error| {
        anyhow::anyhow!(error).context(format!("open pdf '{}'", media.file_path.display()))
    })?;
    let pages = document.get_pages();
    if !pages.contains_key(&(page_number as u32)) {
        return Ok(None);
    }
    let to_delete = pages
        .keys()
        .copied()
        .filter(|number| *number != page_number as u32)
        .collect::<Vec<_>>();
    document.delete_pages(&to_delete);
    document.prune_objects();
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "save pdf page {page_number} from '{}'",
            media.file_path.display()
        ))
    })?;
    Ok(Some(bytes))
}

fn render_pdf_page_at_size(
    media: &BookMediaRecord,
    page_number: u64,
    target_width: u32,
    maximum_height: u32,
    output_format: ImageOutputFormat,
    output_description: &str,
) -> anyhow::Result<Option<RenderedImage>> {
    let pdfium = load_pdfium()?;
    let document = pdfium
        .load_pdf_from_file(&media.file_path, None)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "open pdf '{}' for page rendering: ",
                media.file_path.display()
            ))
        })?;
    let page = document
        .pages()
        .get(
            i32::try_from(page_number.saturating_sub(1)).map_err(|error| {
                anyhow::anyhow!(error).context(format!("convert pdf page number {page_number}"))
            })?,
        )
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "load pdf page {page_number} from '{}'",
                media.file_path.display()
            ))
        })?;
    let rendered = page
        .render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(i32::try_from(target_width).unwrap_or(i32::MAX))
                .set_maximum_height(i32::try_from(maximum_height).unwrap_or(i32::MAX)),
        )
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "render pdf page {page_number} from '{}'",
                media.file_path.display()
            ))
        })?
        .as_image()
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "convert pdf page {page_number} from '{}' to image: ",
                media.file_path.display()
            ))
        })?
        .into_rgb8();

    let image = image::DynamicImage::ImageRgb8(rendered);
    encode_image_with_jpeg_fallback(
        &image,
        output_format,
        &format!(
            "encode pdf page {page_number} {output_description} from '{}'",
            media.file_path.display()
        ),
    )
    .map(Some)
}

async fn render_pdf_page_thumbnail(
    media: &BookMediaRecord,
    page_number: u64,
    max_edge: u32,
    output_format: ImageOutputFormat,
) -> anyhow::Result<Option<RenderedImage>> {
    if !book_media_is_pdf(media) || page_number == 0 {
        return Ok(None);
    }

    let media = media.clone();
    tokio::task::spawn_blocking(move || {
        render_pdf_page_at_size(
            &media,
            page_number,
            max_edge,
            max_edge,
            output_format,
            "thumbnail",
        )
    })
    .await
    .context("join PDF thumbnail render task")?
}

fn render_image_thumbnail(
    bytes: Vec<u8>,
    max_edge: u32,
    output_format: ImageOutputFormat,
) -> anyhow::Result<RenderedImage> {
    let image =
        image::load_from_memory(&bytes).context("render image thumbnail: decode image bytes")?;
    let dimensions = RasterImageDimensions::from_image(&image);
    let resized = if dimensions.max_edge() > max_edge {
        image.resize(max_edge, max_edge, FilterType::Lanczos3)
    } else {
        image
    };
    encode_image_with_jpeg_fallback(
        &resized,
        output_format,
        "render image thumbnail: encode image",
    )
}

fn encode_image_with_jpeg_fallback(
    image: &image::DynamicImage,
    output_format: ImageOutputFormat,
    context: &str,
) -> anyhow::Result<RenderedImage> {
    match encode_image(image, output_format) {
        Ok(bytes) => Ok(RenderedImage {
            bytes,
            format: output_format,
        }),
        Err(error) if output_format != ImageOutputFormat::Jpeg => {
            let jpeg_bytes = encode_image(image, ImageOutputFormat::Jpeg).map_err(|fallback| {
                fallback.context(format!(
                    "{context}: {} encoding failed: {error}; jpeg fallback failed",
                    output_format.content_type()
                ))
            })?;
            Ok(RenderedImage {
                bytes: jpeg_bytes,
                format: ImageOutputFormat::Jpeg,
            })
        }
        Err(error) => Err(error).context(context.to_string()),
    }
}

fn encode_image(
    image: &image::DynamicImage,
    output_format: ImageOutputFormat,
) -> anyhow::Result<Vec<u8>> {
    let mut output = std::io::Cursor::new(Vec::new());
    match output_format {
        ImageOutputFormat::Avif => {
            let encoder = image::codecs::avif::AvifEncoder::new_with_speed_quality(
                &mut output,
                AVIF_ENCODING_SPEED,
                AVIF_ENCODING_QUALITY,
            );
            image.write_with_encoder(encoder)
        }
        ImageOutputFormat::Webp => image.write_to(&mut output, image::ImageFormat::WebP),
        ImageOutputFormat::Jpeg => image.write_to(&mut output, image::ImageFormat::Jpeg),
    }
    .with_context(|| format!("encode image as {}", output_format.content_type()))?;
    Ok(output.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RasterImageDimensions {
    width: u32,
    height: u32,
}

impl RasterImageDimensions {
    fn from_image(image: &image::DynamicImage) -> Self {
        let dimensions = image.dimensions();
        Self {
            width: dimensions.0,
            height: dimensions.1,
        }
    }

    fn max_edge(self) -> u32 {
        self.width.max(self.height)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PdfPageDimensions {
    width: u32,
    height: u32,
}

fn pdf_page_dimensions(document: &PdfDocument, page_number: u32) -> Option<PdfPageDimensions> {
    let object_id = *document.get_pages().get(&page_number)?;
    let page = document.get_dictionary(object_id).ok()?;
    let media_box = page.get(b"MediaBox").ok()?.as_array().ok()?;
    if media_box.len() != 4 {
        return None;
    }

    let left = pdf_numeric_value(&media_box[0])?;
    let bottom = pdf_numeric_value(&media_box[1])?;
    let right = pdf_numeric_value(&media_box[2])?;
    let top = pdf_numeric_value(&media_box[3])?;
    let width = (right - left).abs().round();
    let height = (top - bottom).abs().round();
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    Some(PdfPageDimensions {
        width: width as u32,
        height: height as u32,
    })
}

fn scale_pdf_page_dimensions(dimensions: PdfPageDimensions) -> PdfPageDimensions {
    let max_edge = dimensions.width.max(dimensions.height);
    if max_edge == 0 {
        return dimensions;
    }

    let scale = f64::from(PDF_MAX_RENDER_EDGE) / f64::from(max_edge);
    PdfPageDimensions {
        width: (f64::from(dimensions.width) * scale).round().max(1.0) as u32,
        height: (f64::from(dimensions.height) * scale).round().max(1.0) as u32,
    }
}

fn pdf_numeric_value(object: &lopdf::Object) -> Option<f64> {
    match object {
        lopdf::Object::Integer(value) => Some(*value as f64),
        lopdf::Object::Real(value) => Some((*value).into()),
        _ => None,
    }
}

async fn read_zip_archive_page_bytes(
    media: &BookMediaRecord,
    page: &BookPageRecord,
    page_number: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !book_media_is_zip_archive(media) || page_number == 0 {
        return Ok(None);
    }
    let path = media.file_path.clone();
    let page_file_name = page.file_name.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<u8>>> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "open zip archive '{}': {error}",
                    path.display()
                )));
            }
        };
        let mut archive = ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!("read zip archive '{}'", path.display()))
        })?;
        if !page_file_name.is_empty()
            && let Ok(mut entry) = archive.by_name(&page_file_name)
            && let Ok(entry_name) = entry.name()
            && is_supported_page_image_file_name(entry_name.as_ref())
        {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "read zip archive entry '{}' from '{}'",
                    page_file_name,
                    path.display()
                ))
            })?;
            return Ok(Some(bytes));
        }
        let target_index = usize::try_from(page_number.saturating_sub(1)).map_err(|error| {
            anyhow::anyhow!(error).context(format!("convert zip page number {page_number}"))
        })?;
        let mut supported = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "read zip archive entry #{index} from '{}'",
                    path.display()
                ))
            })?;
            let entry_name = entry
                .name()
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "read zip archive entry #{index} name from '{}'",
                        path.display()
                    ))
                })?
                .into_owned();
            if !is_supported_page_image_file_name(&entry_name) {
                continue;
            }
            supported.push(entry_name);
        }
        supported.sort_by(|left, right| compare_book_names(left, right));
        let Some(entry_name) = supported.get(target_index).map(|name| name.as_str()) else {
            return Ok(None);
        };
        let mut entry = archive.by_name(entry_name).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read zip archive entry '{entry_name}' from '{}'",
                path.display()
            ))
        })?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read zip archive entry '{entry_name}' from '{}'",
                path.display()
            ))
        })?;
        Ok(Some(bytes))
    })
    .await
    .context("join zip archive page read task")?
}

async fn load_zip_archive_page_rows(
    media: &BookMediaRecord,
) -> anyhow::Result<Option<Vec<BookPageRecord>>> {
    let path = media.file_path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<BookPageRecord>>> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "open zip archive '{}': {error}",
                    path.display()
                )));
            }
        };
        let mut archive = ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!("read zip archive '{}'", path.display()))
        })?;
        let mut rows = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "read zip archive entry #{index} from '{}'",
                    path.display()
                ))
            })?;
            let file_name = entry
                .name()
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "read zip archive entry #{index} name from '{}'",
                        path.display()
                    ))
                })?
                .into_owned();
            if !is_supported_page_image_file_name(&file_name) {
                continue;
            }
            rows.push(BookPageRecord {
                number: (rows.len() as u64) + 1,
                media_type: content_type_from_filename(&file_name, "image/jpeg"),
                file_name,
                width: None,
                height: None,
                file_size: entry.size().try_into().unwrap_or(i64::MAX),
            });
        }
        rows.sort_by(|left, right| compare_book_names(&left.file_name, &right.file_name));
        for (index, row) in rows.iter_mut().enumerate() {
            row.number = (index as u64) + 1;
        }
        Ok((!rows.is_empty()).then_some(rows))
    })
    .await
    .context("join zip archive row read task")?
}

fn load_rar_archive_page_rows(
    file_path: &Path,
) -> anyhow::Result<Option<Vec<BookPageRecord>>> {
    let rows = list_rar_entries(file_path)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read rar archive '{}'",
                file_path.display()
            ))
        })?
        .into_iter()
        .filter(|entry| is_supported_page_image_file_name(&entry.file_name))
        .enumerate()
        .map(|(index, entry)| BookPageRecord {
            number: (index as u64) + 1,
            file_name: entry.file_name.clone(),
            media_type: content_type_from_filename(&entry.file_name, "image/jpeg"),
            width: None,
            height: None,
            file_size: entry.unpacked_size.try_into().unwrap_or(i64::MAX),
        })
        .collect::<Vec<_>>();
    let mut rows = rows;
    rows.sort_by(|left, right| compare_book_names(&left.file_name, &right.file_name));
    for (index, row) in rows.iter_mut().enumerate() {
        row.number = (index as u64) + 1;
    }
    Ok((!rows.is_empty()).then_some(rows))
}

async fn read_rar_archive_page_bytes(
    media: &BookMediaRecord,
    page: &BookPageRecord,
    page_number: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !book_media_is_rar_archive(media) || page_number == 0 {
        return Ok(None);
    }
    let file_path = media.file_path.clone();
    let file_name = page.file_name.clone();
    let page_index = usize::try_from(page_number.saturating_sub(1)).map_err(|error| {
        anyhow::anyhow!(error).context(format!("convert rar page number {page_number}"))
    })?;
    tokio::task::spawn_blocking(move || {
        if !file_name.is_empty()
            && let Some(bytes) = read_rar_entry_bytes(&file_path, &file_name)?
        {
            return Ok(Some(bytes));
        }
        let Some(page_file_name) = load_rar_archive_page_rows(&file_path)?
            .unwrap_or_default()
            .into_iter()
            .nth(page_index)
            .map(|row| row.file_name)
        else {
            return Ok(None);
        };
        read_rar_entry_bytes(&file_path, &page_file_name)
    })
    .await
    .context("join rar archive page read task")?
}

pub async fn read_media_file_bytes(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(format!(
            "read media file '{}': {error}",
            path.display()
        ))),
    }
}

pub async fn read_media_file_size(path: &Path) -> anyhow::Result<Option<i64>> {
    match tokio::fs::metadata(path).await {
        Ok(value) if value.is_file() => i64::try_from(value.len()).map(Some).map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("convert media file size '{}'", path.display()))
        }),
        Ok(_) => Err(anyhow::anyhow!(format!(
            "media path '{}' is not a file",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(format!(
            "read media file metadata '{}': {error}",
            path.display()
        ))),
    }
}

pub async fn read_media_image_dimensions(
    path: &Path,
) -> anyhow::Result<Option<MediaImageDimensions>> {
    let Some(bytes) = read_media_file_bytes(path).await? else {
        return Ok(None);
    };
    let image = image::load_from_memory(&bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "decode media image dimensions '{}'",
            path.display()
        ))
    })?;
    Ok(Some(MediaImageDimensions {
        width: i64::from(image.width()),
        height: i64::from(image.height()),
    }))
}

pub fn convert_image_bytes(
    bytes: &[u8],
    source_content_type: &str,
    target_content_type: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    if source_content_type.eq_ignore_ascii_case(target_content_type) {
        return Ok(Some(bytes.to_vec()));
    }

    if !source_content_type
        .to_ascii_lowercase()
        .starts_with("image/")
    {
        return Ok(None);
    }

    let source =
        image::load_from_memory(bytes).context("convert image bytes: decode image bytes")?;
    let target_format = match target_content_type {
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/png" => image::ImageFormat::Png,
        _ => return Ok(None),
    };
    let mut output = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut output, target_format)
        .context("convert image bytes: encode image bytes")?;
    Ok(Some(output.into_inner()))
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use komga_application::media_assets::{BookMediaRecord, BookPageRecord, ImageOutputFormat};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::{
        convert_image_bytes, load_archive_page_rows, load_generated_pdf_page_rows,
        read_media_file_bytes, read_media_file_size, read_media_image_dimensions,
        read_pdf_page_as_single_page_pdf, render_book_page_thumbnail, resolve_book_page_bytes,
    };

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn build_test_zip_archive(entries: Vec<(String, Vec<u8>)>) -> anyhow::Result<Vec<u8>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);

        for (file_name, bytes) in entries {
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer
                .start_file(file_name.as_str(), options)
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!("start zip entry '{file_name}'"))
                })?;
            writer.write_all(&bytes).map_err(|error| {
                anyhow::anyhow!(error).context(format!("write zip entry '{file_name}'"))
            })?;
        }

        writer
            .finish()
            .map(|cursor| cursor.into_inner())
            .context("finalize zip archive")
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_does_not_use_whole_archive_for_non_image() {
        let file_path = unique_temp_path("komga-media-archive");
        let archive = build_test_zip_archive(vec![("meta.txt".to_string(), b"meta".to_vec())])
            .expect("zip payload should be created");
        fs::write(&file_path, archive).expect("archive test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 12,
        };
        let page = BookPageRecord {
            number: 5,
            file_name: "page-005.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let bytes = resolve_book_page_bytes(&media, &page, 5)
            .await
            .expect("non-image archive content should not fail");
        assert!(bytes.is_none());

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_allows_single_image_first_page() {
        let file_path = unique_temp_path("komga-media-image");
        fs::write(&file_path, b"image-bytes").expect("image test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "cover.jpg".to_string(),
            file_path: file_path.clone(),
            media_type: "image/jpeg".to_string(),
            page_count: 1,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "missing-derived-page.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let bytes = resolve_book_page_bytes(&media, &page, 1)
            .await
            .expect("single image page should read");
        assert_eq!(bytes, Some(b"image-bytes".to_vec()));

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn render_book_page_thumbnail_propagates_image_decode_errors() {
        let file_path = unique_temp_path("komga-media-invalid-image-thumbnail");
        fs::write(&file_path, b"not-an-image").expect("invalid image fixture should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "cover.jpg".to_string(),
            file_path: file_path.clone(),
            media_type: "image/jpeg".to_string(),
            page_count: 1,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "cover.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let error = render_book_page_thumbnail(&media, &page, 1, 300, ImageOutputFormat::Jpeg)
            .await
            .expect_err("invalid image bytes must not become a missing thumbnail");

        assert!(
            error.to_string().contains("render image thumbnail"),
            "unexpected thumbnail render error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[test]
    fn convert_image_bytes_propagates_image_decode_errors() {
        let error = convert_image_bytes(b"not-an-image", "image/png", "image/jpeg")
            .expect_err("invalid image bytes must not become a missing converted page");

        assert!(
            error.to_string().contains("convert image bytes"),
            "unexpected conversion error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_book_page_bytes_propagates_media_path_probe_errors() {
        let root = unique_temp_path("komga-media-symlink-loop");
        fs::create_dir(&root).expect("media symlink fixture root should be created");
        let media_path = root.join("loop");
        std::os::unix::fs::symlink(&media_path, &media_path)
            .expect("media path symlink loop should be created");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book".to_string(),
            file_path: media_path.clone(),
            media_type: "application/octet-stream".to_string(),
            page_count: 1,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "page.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let error = resolve_book_page_bytes(&media, &page, 1)
            .await
            .expect_err("media path probe errors must not become missing page bytes");

        assert!(
            error.to_string().contains("read media path metadata"),
            "unexpected media path probe error: {error}"
        );

        let _ = fs::remove_file(media_path);
        let _ = fs::remove_dir(root);
    }

    #[tokio::test]
    async fn read_media_image_dimensions_reports_file_read_errors() {
        let path = unique_temp_path("komga-media-image-directory");
        fs::create_dir(&path).expect("image directory fixture should be created");

        let error = read_media_image_dimensions(&path)
            .await
            .expect_err("directory media path should fail image byte loading");

        assert!(
            error.to_string().contains("read media file"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir(path);
    }

    #[tokio::test]
    async fn read_media_file_size_reports_non_file_paths() {
        let path = unique_temp_path("komga-media-size-directory");
        fs::create_dir(&path).expect("size directory fixture should be created");

        let error = read_media_file_size(&path)
            .await
            .expect_err("directory media path should fail size loading");

        assert!(
            error.to_string().contains("not a file"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_dir(path);
    }

    #[tokio::test]
    async fn read_media_image_dimensions_propagates_image_decode_errors() {
        let path = unique_temp_path("komga-media-invalid-image");
        fs::write(&path, b"not an image").expect("invalid image fixture should be written");

        let error = read_media_image_dimensions(&path)
            .await
            .expect_err("invalid image bytes should fail dimension loading");

        assert!(
            error.to_string().contains("decode media image dimensions"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn load_archive_page_rows_uses_zip_image_entries_only() {
        let file_path = unique_temp_path("komga-media-zip-rows");
        let archive = build_test_zip_archive(vec![
            ("001.jpg".to_string(), b"page-1".to_vec()),
            ("meta.txt".to_string(), b"meta".to_vec()),
            ("002.png".to_string(), b"page-2".to_vec()),
        ])
        .expect("zip payload should be created");
        fs::write(&file_path, archive).expect("zip test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 2,
        };

        let rows = load_archive_page_rows(&media)
            .await
            .expect("archive row load should not fail")
            .expect("archive rows should be parsed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 1);
        assert_eq!(rows[0].file_name, "001.jpg");
        assert_eq!(rows[1].number, 2);
        assert_eq!(rows[1].file_name, "002.png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_extracts_zip_page_by_logical_index() {
        let file_path = unique_temp_path("komga-media-zip-by-index");
        let archive = build_test_zip_archive(vec![
            ("001.jpg".to_string(), b"page-1".to_vec()),
            ("meta.txt".to_string(), b"meta".to_vec()),
            ("002.png".to_string(), b"page-2".to_vec()),
        ])
        .expect("zip payload should be created");
        fs::write(&file_path, archive).expect("zip test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 2,
        };
        let page = BookPageRecord {
            number: 2,
            file_name: "not-present.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let bytes = resolve_book_page_bytes(&media, &page, 2)
            .await
            .expect("zip page bytes should read");
        assert_eq!(bytes, Some(b"page-2".to_vec()));

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_fallback_matches_sorted_rows_for_unsorted_archive() {
        let file_path = unique_temp_path("komga-media-zip-unsorted");
        let archive = build_test_zip_archive(vec![
            ("002.png".to_string(), b"page-2".to_vec()),
            ("001.jpg".to_string(), b"page-1".to_vec()),
        ])
        .expect("zip payload should be created");
        fs::write(&file_path, archive).expect("zip test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 2,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "not-present.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let bytes = resolve_book_page_bytes(&media, &page, 1)
            .await
            .expect("zip page bytes should read");
        assert_eq!(bytes, Some(b"page-1".to_vec()));

        let page2 = BookPageRecord {
            number: 2,
            file_name: String::new(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };
        let bytes2 = resolve_book_page_bytes(&media, &page2, 2)
            .await
            .expect("zip page bytes should read");
        assert_eq!(bytes2, Some(b"page-2".to_vec()));

        let _ = fs::remove_file(file_path);
    }

    #[test]
    fn generated_pdf_rows_do_not_infer_page_count_when_media_count_missing() {
        let file_path = unique_temp_path("komga-media-pdf-archive");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.pdf".to_string(),
            file_path: file_path.clone(),
            media_type: "application/pdf".to_string(),
            page_count: 0,
        };

        let rows = load_generated_pdf_page_rows(&media).expect("generated PDF rows should load");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_propagates_invalid_zip_errors() {
        let file_path = unique_temp_path("komga-media-invalid-zip");
        fs::write(&file_path, b"not-a-zip").expect("invalid zip test file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 1,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "001.jpg".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: 0,
        };

        let error = resolve_book_page_bytes(&media, &page, 1)
            .await
            .expect_err("invalid zip must not become missing page bytes");

        assert!(
            error.to_string().contains("read zip archive"),
            "unexpected zip page error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_archive_page_rows_propagates_invalid_zip_errors() {
        let file_path = unique_temp_path("komga-media-invalid-zip-rows");
        fs::write(&file_path, b"not-a-zip").expect("invalid zip row file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.cbz".to_string(),
            file_path: file_path.clone(),
            media_type: "application/vnd.comicbook+zip".to_string(),
            page_count: 1,
        };

        let error = load_archive_page_rows(&media)
            .await
            .expect_err("invalid zip must not become missing archive rows");

        assert!(
            error.to_string().contains("read zip archive"),
            "unexpected zip row error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[test]
    fn read_pdf_page_as_single_page_pdf_propagates_pdf_load_errors() {
        let file_path = unique_temp_path("komga-media-invalid-pdf");
        fs::write(&file_path, b"not-a-pdf").expect("invalid pdf file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.pdf".to_string(),
            file_path: file_path.clone(),
            media_type: "application/pdf".to_string(),
            page_count: 1,
        };

        let error = read_pdf_page_as_single_page_pdf(&media, 1)
            .expect_err("invalid pdf must not become missing raw page");

        assert!(
            error.to_string().contains("open pdf"),
            "unexpected raw pdf page error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn resolve_book_page_bytes_propagates_invalid_pdf_errors() {
        let file_path = unique_temp_path("komga-media-invalid-pdf-page-bytes");
        fs::write(&file_path, b"not-a-pdf").expect("invalid pdf file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.pdf".to_string(),
            file_path: file_path.clone(),
            media_type: "application/pdf".to_string(),
            page_count: 1,
        };
        let page = BookPageRecord {
            number: 1,
            file_name: "1".to_string(),
            media_type: "image/jpeg".to_string(),
            width: None,
            height: None,
            file_size: -1,
        };

        let error = resolve_book_page_bytes(&media, &page, 1)
            .await
            .expect_err("invalid pdf must not become missing page bytes");

        assert!(
            error.to_string().contains("open pdf"),
            "unexpected pdf page bytes error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[test]
    fn generated_pdf_rows_propagates_invalid_pdf_errors() {
        let file_path = unique_temp_path("komga-media-invalid-pdf-rows");
        fs::write(&file_path, b"not-a-pdf").expect("invalid pdf file should be written");

        let media = BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.pdf".to_string(),
            file_path: file_path.clone(),
            media_type: "application/pdf".to_string(),
            page_count: 1,
        };

        let error = load_generated_pdf_page_rows(&media)
            .expect_err("invalid pdf must not become empty generated page rows");

        assert!(
            error.to_string().contains("open pdf"),
            "unexpected generated pdf row error: {error}"
        );

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn read_media_file_bytes_distinguishes_read_errors_from_missing_files() {
        let missing_path = unique_temp_path("komga-media-missing-file");
        let missing = read_media_file_bytes(&missing_path)
            .await
            .expect("missing file should not be an internal read error");
        assert!(missing.is_none());

        let directory_path = unique_temp_path("komga-media-file-read-error");
        fs::create_dir(&directory_path).expect("read error fixture should be a directory");
        let error = read_media_file_bytes(&directory_path)
            .await
            .expect_err("directory read error must not become a missing file");

        assert!(
            error.to_string().contains("read media file"),
            "unexpected media file read error: {error}"
        );

        let _ = fs::remove_dir(directory_path);
    }
}
