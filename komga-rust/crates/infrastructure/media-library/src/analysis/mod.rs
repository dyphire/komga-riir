use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use anyhow::Context;
use komga_domain::discovery::MediaStatus;
use komga_epub::{MOBI_MEDIA_TYPE, analyze_epub_archive, normalize_mobi};
use lopdf::{Document as PdfDocument, Object};
use sha2::{Digest, Sha256};

use komga_infrastructure_media_core::content::metadata_sources::{
    CapturedMetadataDocument, CapturedMetadataSources, MetadataSourceRequest,
    read_comicinfo_from_zip_archive, take_comicinfo_from_rar_entries,
};
use komga_infrastructure_media_core::formats::rar::{
    detect_rar_media_type, read_rar_entries_bytes,
};

mod persistence;
mod task;

pub use task::{AnalyzeBookOutcome, BookAnalysisPurpose, analyze_book};

const IMAGE_DIMENSIONS_INITIAL_READ_BYTES: usize = 512;
const IMAGE_DIMENSIONS_READ_CHUNK_BYTES: usize = 16 * 1024;
const IMAGE_DIMENSIONS_MAX_READ_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaAnalysisProfile {
    PersistedBook {
        include_dimensions: bool,
        include_page_hashes: bool,
        metadata_sources: MetadataSourceRequest,
    },
    Transient,
}

impl MediaAnalysisProfile {
    fn include_dimensions(self) -> bool {
        match self {
            Self::PersistedBook {
                include_dimensions, ..
            } => include_dimensions,
            Self::Transient => true,
        }
    }

    fn include_page_hashes(self) -> bool {
        match self {
            Self::PersistedBook {
                include_page_hashes, ..
            } => include_page_hashes,
            Self::Transient => false,
        }
    }

    fn include_epub_resources(self) -> bool {
        matches!(self, Self::PersistedBook { .. })
    }

    fn supports_single_image(self) -> bool {
        matches!(self, Self::Transient)
    }

    fn pdf_page_media_type(self) -> &'static str {
        match self {
            Self::PersistedBook { .. } => "application/pdf",
            Self::Transient => "image/jpeg",
        }
    }

    fn pdf_page_file_name(self, index: usize) -> String {
        match self {
            Self::PersistedBook { .. } => format!("page-{index:04}.pdf"),
            Self::Transient => (index + 1).to_string(),
        }
    }

    fn scale_pdf_dimensions(self) -> bool {
        matches!(self, Self::Transient)
    }

    fn records_analysis_error(self) -> bool {
        matches!(self, Self::PersistedBook { .. })
    }

    fn media_type_from_path(self, path: &Path) -> anyhow::Result<String> {
        let detected = detected_media_type_from_path(path)?;
        Ok(if detected == "application/octet-stream" {
            let fallback = match self {
                Self::PersistedBook { .. } => persisted_media_type_from_file_name(
                    path.file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                ),
                Self::Transient => transient_media_type_from_file_name(
                    path.file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                ),
            }
            .to_string();
            if fallback == "application/epub+zip" {
                detected
            } else {
                fallback
            }
        } else {
            detected
        })
    }

    fn metadata_source_request(self) -> MetadataSourceRequest {
        match self {
            Self::PersistedBook {
                metadata_sources, ..
            } => metadata_sources,
            _ => MetadataSourceRequest::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzedMediaPage {
    pub file_name: String,
    pub media_type: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub file_size: i64,
    pub file_hash: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaFileAnalysis {
    pub status: MediaStatus,
    pub media_type: String,
    pub comment: Option<String>,
    pub page_count: u64,
    pub epub_divina_compatible: bool,
    pub epub_is_kepub: bool,
    pub pages: Vec<AnalyzedMediaPage>,
    pub files: Vec<String>,
    pub media_files: Vec<AnalyzedMediaFile>,
    pub epub_extension_blob: Option<Vec<u8>>,
    pub metadata_sources: CapturedMetadataSources,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzedMediaFile {
    pub file_name: String,
    pub media_type: Option<String>,
    pub sub_type: Option<String>,
    pub file_size: Option<i64>,
}

pub struct MediaFileAnalyzer;

#[derive(Default)]
struct AnalyzedMediaFileContents {
    comment: Option<String>,
    page_count: u64,
    epub_divina_compatible: bool,
    epub_is_kepub: bool,
    pages: Vec<AnalyzedMediaPage>,
    files: Vec<String>,
    media_files: Vec<AnalyzedMediaFile>,
    epub_extension_blob: Option<Vec<u8>>,
    metadata_sources: CapturedMetadataSources,
}

fn empty_media_analysis_with_comment(
    status: MediaStatus,
    media_type: String,
    comment: Option<String>,
) -> MediaFileAnalysis {
    MediaFileAnalysis {
        status,
        media_type,
        comment,
        page_count: 0,
        epub_divina_compatible: false,
        epub_is_kepub: false,
        pages: Vec::new(),
        files: Vec::new(),
        media_files: Vec::new(),
        epub_extension_blob: None,
        metadata_sources: CapturedMetadataSources {
            comicinfo: CapturedMetadataDocument::NotApplicable,
            epub: CapturedMetadataDocument::NotApplicable,
        },
    }
}

impl MediaFileAnalyzer {
    pub fn analyze(
        &self,
        file_path: &Path,
        profile: MediaAnalysisProfile,
    ) -> anyhow::Result<MediaFileAnalysis> {
        match file_path.try_exists() {
            Ok(true) => {}
            Ok(false) => {
                return Ok(empty_media_analysis_with_comment(
                    MediaStatus::Error,
                    String::new(),
                    Some("ERR_1018".to_string()),
                ));
            }
            Err(error) => {
                let debug = format!("{error:?}");
                let error = anyhow::Error::new(error).context(format!(
                    "check media file existence '{}': {debug}",
                    file_path.display()
                ));
                if profile.records_analysis_error() {
                    return Ok(empty_media_analysis_with_comment(
                        MediaStatus::Error,
                        String::new(),
                        Some(filesystem_error_code(&error).to_string()),
                    ));
                }
                return Err(error);
            }
        }

        let mut media_type = match profile.media_type_from_path(file_path) {
            Ok(media_type) => media_type,
            Err(error) if profile.records_analysis_error() => {
                return Ok(empty_media_analysis_with_comment(
                    MediaStatus::Error,
                    String::new(),
                    Some(filesystem_error_code(&error).to_string()),
                ));
            }
            Err(error) => return Err(error),
        };

        if is_epub_extension(file_path) && media_type != "application/epub+zip" {
            if is_epub_file(file_path) {
                media_type = "application/epub+zip".to_string();
            } else {
                return Ok(empty_media_analysis_with_comment(
                    MediaStatus::Error,
                    media_type,
                    Some("ERR_1032".to_string()),
                ));
            }
        }

        let result = match media_type.as_str() {
            value if value.starts_with("image/") && profile.supports_single_image() => {
                analyze_single_image(file_path)
            }
            "application/zip" => analyze_zip_media_pages(file_path, profile),
            "application/epub+zip" if profile.include_epub_resources() => {
                analyze_epub_media_pages(file_path, profile)
            }
            "application/epub+zip" => analyze_zip_media_pages(file_path, profile),
            MOBI_MEDIA_TYPE => analyze_mobi_media_pages(file_path, profile),
            "application/vnd.comicbook-rar"
            | "application/x-rar-compressed"
            | "application/x-rar-compressed; version=4"
            | "application/x-rar-compressed; version=5" => {
                analyze_rar_media_pages(file_path, profile)
            }
            "application/pdf" => analyze_pdf_media_pages(file_path, profile),
            _ => {
                return Ok(empty_media_analysis_with_comment(
                    MediaStatus::Unsupported,
                    media_type,
                    Some("ERR_1001".to_string()),
                ));
            }
        };

        let contents = match result {
            Ok(result) => result,
            Err(error) if profile.records_analysis_error() => {
                let comment = analysis_error_code(&media_type, &error).to_string();
                let status = if matches!(comment.as_str(), "ERR_1002" | "ERR_1004") {
                    MediaStatus::Unsupported
                } else {
                    MediaStatus::Error
                };
                return Ok(empty_media_analysis_with_comment(
                    status,
                    media_type,
                    Some(comment),
                ));
            }
            Err(error) => return Err(error),
        };
        let page_count = contents.page_count.max(contents.pages.len() as u64);
        let (status, comment) = if page_count == 0 {
            (MediaStatus::Error, Some("ERR_1006".to_string()))
        } else {
            (MediaStatus::Ready, contents.comment)
        };
        Ok(MediaFileAnalysis {
            status,
            media_type,
            comment,
            page_count,
            epub_divina_compatible: contents.epub_divina_compatible,
            epub_is_kepub: contents.epub_is_kepub,
            pages: contents.pages,
            files: contents.files,
            media_files: contents.media_files,
            epub_extension_blob: contents.epub_extension_blob,
            metadata_sources: contents.metadata_sources,
        })
    }
}

pub fn analyze_book_media_file(
    file_path: &Path,
    analyze_dimensions: bool,
) -> anyhow::Result<MediaFileAnalysis> {
    analyze_book_media_file_with_sources(
        file_path,
        analyze_dimensions,
        false,
        MetadataSourceRequest::default(),
    )
}

pub fn analyze_book_media_file_with_sources(
    file_path: &Path,
    analyze_dimensions: bool,
    include_page_hashes: bool,
    metadata_sources: MetadataSourceRequest,
) -> anyhow::Result<MediaFileAnalysis> {
    MediaFileAnalyzer.analyze(
        file_path,
        MediaAnalysisProfile::PersistedBook {
            include_dimensions: analyze_dimensions,
            include_page_hashes,
            metadata_sources,
        },
    )
}

fn filesystem_error_code(error: &anyhow::Error) -> &'static str {
    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .map(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => "ERR_1018",
                    std::io::ErrorKind::PermissionDenied => "ERR_1000",
                    _ => "ERR_1005",
                })
        })
        .unwrap_or("ERR_1005")
}

fn analysis_error_code(media_type: &str, error: &anyhow::Error) -> &'static str {
    if matches!(
        media_type,
        "application/vnd.comicbook-rar"
            | "application/x-rar-compressed"
            | "application/x-rar-compressed; version=4"
            | "application/x-rar-compressed; version=5"
    ) {
        for cause in error.chain() {
            if let Some(error) = cause.downcast_ref::<unrar::error::UnrarError>() {
                return match error.code {
                    unrar::error::Code::MissingPassword | unrar::error::Code::BadPassword => {
                        "ERR_1002"
                    }
                    unrar::error::Code::EOpen if error.when == unrar::error::When::Process => {
                        "ERR_1004"
                    }
                    unrar::error::Code::EOpen => "ERR_1008",
                    _ => "ERR_1008",
                };
            }
        }
        return "ERR_1008";
    }
    if media_type == "application/zip" {
        "ERR_1008"
    } else {
        "ERR_1005"
    }
}

fn is_epub_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("epub"))
}

fn is_epub_file(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return false;
    };
    let Ok(mut entry) = archive.by_name("mimetype") else {
        return false;
    };
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).is_ok()
        && String::from_utf8_lossy(&bytes).trim() == "application/epub+zip"
}

pub fn transient_media_type_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let fallback = transient_media_type_from_file_name(file_name).to_string();
    let detected = detected_media_type_from_path(path).unwrap_or_else(|_| fallback.clone());
    if detected == "application/octet-stream" {
        Some(fallback)
    } else {
        Some(detected)
    }
}

pub fn media_type_from_entry_name(file_name: &str) -> String {
    match extension(file_name).as_deref() {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("xhtml") | Some("html") | Some("htm") => "application/xhtml+xml",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub fn is_supported_page_image_file_name(file_name: &str) -> bool {
    matches!(
        extension(file_name).as_deref(),
        Some("jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "bmp")
    )
}

fn read_archive_entry_prefix<R: Read>(entry: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    entry
        .take(IMAGE_DIMENSIONS_MAX_READ_BYTES as u64)
        .read_to_end(&mut bytes)
        .context("read archive entry bytes")?;
    Ok(bytes)
}

fn media_type_from_entry_bytes(file_name: &str, bytes: &[u8]) -> String {
    if let Ok(format) = image::guess_format(bytes) {
        return image_format_media_type(format).to_string();
    }
    if bytes.starts_with(b"%PDF-") {
        return "application/pdf".to_string();
    }
    match extension(file_name).as_deref() {
        Some("xhtml" | "html" | "htm") => "application/xhtml+xml".to_string(),
        Some("svg") => "image/svg+xml".to_string(),
        Some("xml") => "application/xml".to_string(),
        Some("css") => "text/css".to_string(),
        Some("json") => "application/json".to_string(),
        Some("txt") => "text/plain".to_string(),
        _ if looks_like_text(bytes) => "text/plain".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn looks_like_text(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && !bytes.contains(&0)
        && std::str::from_utf8(bytes).is_ok()
        && bytes
            .iter()
            .all(|byte| *byte == b'\n' || *byte == b'\r' || *byte == b'\t' || *byte >= 0x20)
}

pub fn is_rar_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "application/x-rar-compressed; version=4" | "application/x-rar-compressed; version=5"
    )
}

fn transient_media_type_from_file_name(file_name: &str) -> &'static str {
    match extension(file_name).as_deref() {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("epub") => "application/epub+zip",
        Some("mobi") => MOBI_MEDIA_TYPE,
        Some("cbz") | Some("zip") => "application/zip",
        Some("cbr") | Some("rar") => "application/vnd.comicbook-rar",
        _ => "application/octet-stream",
    }
}

fn persisted_media_type_from_file_name(file_name: &str) -> &'static str {
    match extension(file_name).as_deref() {
        Some("cbz") | Some("zip") => "application/zip",
        Some("cbr") | Some("rar") => "application/vnd.comicbook-rar",
        Some("pdf") => "application/pdf",
        Some("epub") => "application/epub+zip",
        Some("mobi") => MOBI_MEDIA_TYPE,
        _ => "application/octet-stream",
    }
}

fn extension(file_name: &str) -> Option<String> {
    PathBuf::from(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MediaDimensions {
    width: i64,
    height: i64,
}

fn image_dimensions_from_bytes_i64(bytes: &[u8]) -> Option<MediaDimensions> {
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(MediaDimensions {
        width: i64::from(dimensions.0),
        height: i64::from(dimensions.1),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}

pub fn image_dimensions_from_bytes_u32(bytes: &[u8]) -> Option<ImageDimensions> {
    let dimensions = image_dimensions_from_bytes_i64(bytes)?;
    Some(ImageDimensions {
        width: dimensions.width.try_into().ok()?,
        height: dimensions.height.try_into().ok()?,
    })
}

fn image_dimensions_from_reader(reader: &mut dyn Read) -> std::io::Result<Option<MediaDimensions>> {
    let mut bytes = Vec::with_capacity(IMAGE_DIMENSIONS_INITIAL_READ_BYTES);
    let mut next_read_size = IMAGE_DIMENSIONS_INITIAL_READ_BYTES;
    let mut buffer = [0; 4096];

    loop {
        let remaining = IMAGE_DIMENSIONS_MAX_READ_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            return Ok(None);
        }

        let mut bytes_left = next_read_size.min(remaining);
        let bytes_before_read = bytes.len();
        while bytes_left > 0 {
            let read_size = buffer.len().min(bytes_left);
            let bytes_read = reader.read(&mut buffer[..read_size])?;
            if bytes_read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..bytes_read]);
            bytes_left -= bytes_read;
        }

        let bytes_read = bytes.len() - bytes_before_read;
        if bytes_read == 0 {
            return Ok(image_dimensions_from_bytes_i64(&bytes));
        }

        if let Some(dimensions) = image_dimensions_from_bytes_i64(&bytes) {
            return Ok(Some(dimensions));
        }

        next_read_size = IMAGE_DIMENSIONS_READ_CHUNK_BYTES;
    }
}

fn pdf_page_dimensions(document: &PdfDocument, page_number: u32) -> Option<MediaDimensions> {
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

    Some(MediaDimensions {
        width: width as i64,
        height: height as i64,
    })
}

fn analyze_single_image(file_path: &Path) -> anyhow::Result<AnalyzedMediaFileContents> {
    let file_name = file_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let metadata = std::fs::metadata(file_path).map_err(|error| {
        let debug = format!("{error:?}");
        anyhow::Error::new(error).context(format!(
            "read image metadata '{}': {debug}",
            file_path.display()
        ))
    })?;
    let size_bytes = i64::try_from(metadata.len()).map_err(|error| {
        anyhow::anyhow!(error).context(format!("image file too large '{}'", file_path.display()))
    })?;
    let bytes = std::fs::read(file_path).map_err(|error| {
        let debug = format!("{error:?}");
        anyhow::Error::new(error).context(format!(
            "read image bytes '{}': {debug}",
            file_path.display()
        ))
    })?;
    let dimensions = image_dimensions_from_bytes_i64(&bytes).ok_or_else(|| {
        anyhow::anyhow!(format!("decode image dimensions '{}'", file_path.display()))
    })?;
    let dimensions = analyzed_media_page_dimensions(Some(dimensions));
    let media_type = detected_media_type_from_path(file_path)
        .unwrap_or_else(|_| media_type_from_entry_name(&file_name));

    Ok(AnalyzedMediaFileContents {
        pages: vec![AnalyzedMediaPage {
            file_name: file_name.clone(),
            media_type,
            width: dimensions.width,
            height: dimensions.height,
            file_size: size_bytes,
            file_hash: None,
        }],
        files: vec![file_name],
        ..Default::default()
    })
}

fn analyze_zip_media_pages(
    file_path: &Path,
    profile: MediaAnalysisProfile,
) -> anyhow::Result<AnalyzedMediaFileContents> {
    let file = std::fs::File::open(file_path).map_err(|error| {
        let debug = format!("{error:?}");
        anyhow::Error::new(error).context(format!(
            "open zip file '{}': {debug}",
            file_path.display()
        ))
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        anyhow::anyhow!(error).context(format!("open zip archive '{}'", file_path.display()))
    })?;

    let mut files = Vec::new();
    let mut pages = Vec::new();
    let mut media_files = Vec::new();
    let mut entry_errors = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            anyhow::anyhow!(error).context(format!("read zip entry at index {index}"))
        })?;
        if entry.is_dir() {
            continue;
        }

        let file_name = entry
            .name()
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!("read zip entry name at index {index}"))
            })?
            .trim()
            .to_string();
        if file_name.is_empty() {
            continue;
        }
        files.push(file_name.clone());
        let file_size = i64::try_from(entry.size()).ok();

        let media_type_from_name = media_type_from_entry_name(&file_name);
        let extension_lower = extension(&file_name);
        let is_known_image = matches!(
            extension_lower.as_deref(),
            Some("jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "bmp" | "svg")
        );
        let is_vector_image = extension_lower.as_deref() == Some("svg");
        let needs_content_detection = !is_known_image && media_type_from_name == "application/octet-stream";

        if !is_known_image && !needs_content_detection {
            media_files.push(AnalyzedMediaFile {
                file_name,
                media_type: Some(media_type_from_name),
                sub_type: None,
                file_size,
            });
            continue;
        }

        if profile.include_page_hashes() {
            // Stream the full entry once: head bytes feed dimension decoding,
            // the whole payload feeds the SHA-256 hasher. Avoids a second
            // decompression pass in the standalone page-hash task.
            let mut bytes = Vec::new();
            match entry.read_to_end(&mut bytes) {
                Ok(_) => {}
                Err(_) => {
                    entry_errors.push(file_name.clone());
                    media_files.push(AnalyzedMediaFile {
                        file_name,
                        media_type: None,
                        sub_type: None,
                        file_size: None,
                    });
                    continue;
                }
            }
            let media_type = if is_known_image {
                media_type_from_name
            } else {
                media_type_from_entry_bytes(&file_name, &bytes)
            };
            if !media_type.starts_with("image/") {
                media_files.push(AnalyzedMediaFile {
                    file_name,
                    media_type: Some(media_type),
                    sub_type: None,
                    file_size,
                });
                continue;
            }
            let dimensions = if profile.include_dimensions() {
                image_dimensions_from_bytes_i64(&bytes)
            } else {
                None
            };
            if profile.include_dimensions() && dimensions.is_none() {
                entry_errors.push(file_name.clone());
                media_files.push(AnalyzedMediaFile {
                    file_name,
                    media_type: None,
                    sub_type: None,
                    file_size: None,
                });
                continue;
            }
            let dimensions = analyzed_media_page_dimensions(dimensions);
            let file_hash = sha256_hex(&bytes);
            pages.push(AnalyzedMediaPage {
                media_type,
                file_name,
                width: dimensions.width,
                height: dimensions.height,
                file_size: file_size.unwrap_or(i64::MAX),
                file_hash: Some(file_hash),
            });
            continue;
        }

        if is_known_image {
            if !profile.include_dimensions() || is_vector_image {
                pages.push(AnalyzedMediaPage {
                    media_type: media_type_from_name,
                    file_name,
                    width: None,
                    height: None,
                    file_size: file_size.unwrap_or(i64::MAX),
                    file_hash: None,
                });
                continue;
            }
            match image_dimensions_from_reader(&mut entry) {
                Ok(Some(dimensions)) => {
                    let dimensions = analyzed_media_page_dimensions(Some(dimensions));
                    pages.push(AnalyzedMediaPage {
                        media_type: media_type_from_name,
                        file_name,
                        width: dimensions.width,
                        height: dimensions.height,
                        file_size: file_size.unwrap_or(i64::MAX),
                        file_hash: None,
                    });
                }
                Ok(None) | Err(_) => {
                    entry_errors.push(file_name.clone());
                    media_files.push(AnalyzedMediaFile {
                        file_name,
                        media_type: None,
                        sub_type: None,
                        file_size: None,
                    });
                }
            }
            continue;
        }

        let entry_prefix = match read_archive_entry_prefix(&mut entry) {
            Ok(bytes) => bytes,
            Err(_) => {
                entry_errors.push(file_name.clone());
                media_files.push(AnalyzedMediaFile {
                    file_name,
                    media_type: None,
                    sub_type: None,
                    file_size: None,
                });
                continue;
            }
        };
        let media_type = media_type_from_entry_bytes(&file_name, &entry_prefix);
        if !media_type.starts_with("image/") {
            media_files.push(AnalyzedMediaFile {
                file_name,
                media_type: Some(media_type),
                sub_type: None,
                file_size,
            });
            continue;
        }

        let dimensions = if profile.include_dimensions() {
            image_dimensions_from_bytes_i64(&entry_prefix)
        } else {
            None
        };
        if profile.include_dimensions() && dimensions.is_none() {
            entry_errors.push(file_name.clone());
            media_files.push(AnalyzedMediaFile {
                file_name,
                media_type: None,
                sub_type: None,
                file_size: None,
            });
            continue;
        }
        let dimensions = analyzed_media_page_dimensions(dimensions);
        pages.push(AnalyzedMediaPage {
            media_type,
            file_name,
            width: dimensions.width,
            height: dimensions.height,
            file_size: file_size.unwrap_or(i64::MAX),
            file_hash: None,
        });
    }

    files.sort();
    let comment = (!entry_errors.is_empty()).then(|| {
        format!(
            "ERR_1007 [{}]",
            entry_errors
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let metadata_sources =
        capture_comicinfo(Some(&mut archive), profile.metadata_source_request().comicinfo);
    Ok(AnalyzedMediaFileContents {
        page_count: pages.len() as u64,
        pages,
        files,
        media_files,
        comment,
        metadata_sources: CapturedMetadataSources {
            comicinfo: metadata_sources,
            epub: CapturedMetadataDocument::NotApplicable,
        },
        ..Default::default()
    })
}

fn capture_comicinfo<R: std::io::Read + std::io::Seek>(
    archive: Option<&mut zip::ZipArchive<R>>,
    requested: bool,
) -> CapturedMetadataDocument {
    if !requested {
        return CapturedMetadataDocument::NotRequested;
    }
    match archive {
        Some(archive) => match read_comicinfo_from_zip_archive(archive) {
            Ok(Some(bytes)) => CapturedMetadataDocument::Present(bytes),
            Ok(None) => CapturedMetadataDocument::Absent,
            Err(error) => CapturedMetadataDocument::Failed(format!("{error:#}")),
        },
        None => CapturedMetadataDocument::Failed("archive unavailable".to_string()),
    }
}

fn analyze_epub_media_pages(
    file_path: &Path,
    profile: MediaAnalysisProfile,
) -> anyhow::Result<AnalyzedMediaFileContents> {
    let request = profile.metadata_source_request();
    let file = std::fs::File::open(file_path)
        .map_err(|error| {
            let debug = format!("{error:?}");
            anyhow::Error::new(error).context(format!(
                "open EPUB for analysis '{}': {debug}",
                file_path.display()
            ))
        })?;
    let mut archive = zip::ZipArchive::new(file).context("open EPUB archive for analysis")?;
    let analysis = analyze_epub_archive(&mut archive, request.epub)
        .map_err(|error| anyhow::anyhow!(error).context("analyze EPUB publication"))?;
    let mut archive = if profile.include_dimensions()
        || request.comicinfo
        || profile.include_page_hashes()
    {
        let file = std::fs::File::open(file_path).map_err(|error| {
            let debug = format!("{error:?}");
            anyhow::Error::new(error).context(format!(
                "open EPUB for dimensions '{}': {debug}",
                file_path.display()
            ))
        })?;
        Some(zip::ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "open EPUB archive for dimensions '{}'",
                file_path.display()
            ))
        })?)
    } else {
        None
    };

    let pages = analysis
        .pages
        .into_iter()
        .map(|page| {
            let is_image_page = page.media_type.starts_with("image/");
            if profile.include_page_hashes() && is_image_page {
                // Stream the full image once: head bytes feed dimension
                // decoding, the whole payload feeds the SHA-256 hasher.
                let archive = archive
                    .as_mut()
                    .expect("EPUB archive is opened when page hashes are enabled");
                let bytes = read_epub_image_bytes(archive, &page.file_name)?;
                let dimensions = if profile.include_dimensions() {
                    image_dimensions_from_bytes_i64(&bytes)
                } else {
                    None
                };
                let dimensions = analyzed_media_page_dimensions(dimensions);
                let file_hash = sha256_hex(&bytes);
                return Ok(AnalyzedMediaPage {
                    file_name: page.file_name,
                    media_type: page.media_type,
                    width: dimensions.width,
                    height: dimensions.height,
                    file_size: page.file_size,
                    file_hash: Some(file_hash),
                });
            }
            let dimensions = if profile.include_dimensions() && is_image_page {
                let archive = archive
                    .as_mut()
                    .expect("EPUB archive is opened when dimensions are enabled");
                Some(
                    read_epub_image_dimensions(archive, &page.file_name)?.ok_or_else(|| {
                        anyhow::anyhow!(format!(
                            "decode EPUB image dimensions for '{}'",
                            page.file_name
                        ))
                    })?,
                )
            } else {
                None
            };
            let dimensions = analyzed_media_page_dimensions(dimensions);
            Ok(AnalyzedMediaPage {
                file_name: page.file_name,
                media_type: page.media_type,
                width: dimensions.width,
                height: dimensions.height,
                file_size: page.file_size,
                file_hash: None,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let media_files = analysis
        .media_files
        .into_iter()
        .map(|file| AnalyzedMediaFile {
            file_name: file.file_name,
            media_type: Some(file.media_type),
            sub_type: Some(file.sub_type),
            file_size: file.file_size,
        })
        .collect();

    Ok(AnalyzedMediaFileContents {
        page_count: analysis.page_count,
        epub_divina_compatible: analysis.divina_compatible,
        epub_is_kepub: analysis.is_kepub,
        comment: analysis.comment,
        pages,
        files: analysis.files,
        media_files,
        metadata_sources: CapturedMetadataSources {
            comicinfo: capture_comicinfo(archive.as_mut(), request.comicinfo),
            epub: if request.epub {
                analysis
                    .package_document
                    .map(CapturedMetadataDocument::Present)
                    .unwrap_or(CapturedMetadataDocument::Absent)
            } else {
                CapturedMetadataDocument::NotRequested
            },
        },
        epub_extension_blob: Some(analysis.extension_blob),
    })
}

fn read_epub_image_dimensions<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> anyhow::Result<Option<MediaDimensions>> {
    let mut entry = archive.by_name(file_name).map_err(|error| {
        anyhow::anyhow!(error).context(format!("read EPUB image '{file_name}'"))
    })?;
    image_dimensions_from_reader(&mut entry).map_err(|error| {
        anyhow::anyhow!(error).context(format!("read EPUB image dimensions '{file_name}'"))
    })
}

fn read_epub_image_bytes<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    file_name: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut entry = archive.by_name(file_name).map_err(|error| {
        anyhow::anyhow!(error).context(format!("read EPUB image '{file_name}'"))
    })?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!("read EPUB image bytes '{file_name}'"))
    })?;
    Ok(bytes)
}

fn analyze_mobi_media_pages(
    file_path: &Path,
    profile: MediaAnalysisProfile,
) -> anyhow::Result<AnalyzedMediaFileContents> {
    let bytes = std::fs::read(file_path).map_err(|error| {
        let debug = format!("{error:?}");
        anyhow::Error::new(error).context(format!(
            "read MOBI file '{}': {debug}",
            file_path.display()
        ))
    })?;
    let publication = normalize_mobi(&bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!("normalize MOBI file '{}'", file_path.display()))
    })?;

    let mut files = publication
        .chapters
        .iter()
        .map(|chapter| chapter.path.clone())
        .chain(
            publication
                .resources
                .iter()
                .map(|resource| resource.path.clone()),
        )
        .collect::<Vec<_>>();
    files.push("OEBPS/content.opf".to_string());
    files.push("OEBPS/nav.xhtml".to_string());
    files.sort();

    let pages = publication
        .chapters
        .iter()
        .map(|chapter| AnalyzedMediaPage {
            file_name: chapter.path.clone(),
            media_type: "application/xhtml+xml".to_string(),
            width: None,
            height: None,
            file_size: 0,
            file_hash: None,
        })
        .collect::<Vec<_>>();

    let mut media_files = publication
        .chapters
        .iter()
        .map(|chapter| AnalyzedMediaFile {
            file_name: chapter.path.clone(),
            media_type: Some("application/xhtml+xml".to_string()),
            sub_type: Some("EPUB_PAGE".to_string()),
            file_size: Some(0),
        })
        .collect::<Vec<_>>();
    media_files.extend(
        publication
            .resources
            .iter()
            .map(|resource| AnalyzedMediaFile {
                file_name: resource.path.clone(),
                media_type: Some(resource.media_type.clone()),
                sub_type: Some("EPUB_ASSET".to_string()),
                file_size: Some(resource.bytes.len().try_into().unwrap_or(i64::MAX)),
            }),
    );
    media_files.extend([
        AnalyzedMediaFile {
            file_name: "OEBPS/content.opf".to_string(),
            media_type: Some("application/oebps-package+xml".to_string()),
            sub_type: Some("EPUB_ASSET".to_string()),
            file_size: Some(0),
        },
        AnalyzedMediaFile {
            file_name: "OEBPS/nav.xhtml".to_string(),
            media_type: Some("application/xhtml+xml".to_string()),
            sub_type: Some("EPUB_ASSET".to_string()),
            file_size: Some(0),
        },
    ]);

    let metadata_sources = if profile.metadata_source_request().epub {
        match publication.resource_bytes("OEBPS/content.opf") {
            Ok(Some(bytes)) => CapturedMetadataDocument::Present(bytes),
            Ok(None) => CapturedMetadataDocument::Absent,
            Err(error) => CapturedMetadataDocument::Failed(error.to_string()),
        }
    } else {
        CapturedMetadataDocument::NotRequested
    };

    Ok(AnalyzedMediaFileContents {
        comment: None,
        page_count: publication.page_count,
        epub_divina_compatible: false,
        epub_is_kepub: false,
        pages,
        files,
        media_files,
        metadata_sources: CapturedMetadataSources {
            comicinfo: CapturedMetadataDocument::NotApplicable,
            epub: metadata_sources,
        },
        epub_extension_blob: Some(
            publication
                .epub_extension_blob()
                .map_err(|error| anyhow::anyhow!(error))?,
        ),
    })
}

fn analyze_rar_media_pages(
    file_path: &Path,
    profile: MediaAnalysisProfile,
) -> anyhow::Result<AnalyzedMediaFileContents> {
    let mut entries = read_rar_entries_bytes(file_path).context("read rar entries failed")?;
    let mut files = entries
        .iter()
        .map(|entry| entry.file_name.clone())
        .collect::<Vec<_>>();
    files.sort();

    let mut pages = Vec::new();
    let mut media_files = Vec::new();
    let mut entry_errors = Vec::new();
    for entry in &entries {
        let media_type = media_type_from_entry_bytes(&entry.file_name, &entry.bytes);
        if !media_type.starts_with("image/") {
            media_files.push(AnalyzedMediaFile {
                file_name: entry.file_name.clone(),
                media_type: Some(media_type),
                sub_type: None,
                file_size: Some(entry.unpacked_size.try_into().unwrap_or(i64::MAX)),
            });
            continue;
        }
        let dimensions = if profile.include_dimensions() {
            image_dimensions_from_bytes_i64(&entry.bytes)
        } else {
            None
        };
        if profile.include_dimensions() && dimensions.is_none() {
            entry_errors.push(entry.file_name.clone());
            media_files.push(AnalyzedMediaFile {
                file_name: entry.file_name.clone(),
                media_type: None,
                sub_type: None,
                file_size: None,
            });
            continue;
        }
        let file_hash = profile
            .include_page_hashes()
            .then(|| sha256_hex(&entry.bytes));
        pages.push(analyzed_rar_media_page(
            entry.file_name.clone(),
            entry.unpacked_size,
            dimensions,
            file_hash,
        ));
    }

    let comment = (!entry_errors.is_empty()).then(|| {
        format!(
            "ERR_1007 [{}]",
            entry_errors
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        )
    });

    let metadata_sources = if profile.metadata_source_request().comicinfo {
        take_comicinfo_from_rar_entries(&mut entries)
            .map(CapturedMetadataDocument::Present)
            .unwrap_or(CapturedMetadataDocument::Absent)
    } else {
        CapturedMetadataDocument::NotRequested
    };
    Ok(AnalyzedMediaFileContents {
        pages,
        files,
        media_files,
        comment,
        metadata_sources: CapturedMetadataSources {
            comicinfo: metadata_sources,
            epub: CapturedMetadataDocument::NotApplicable,
        },
        ..Default::default()
    })
}

fn analyze_pdf_media_pages(
    file_path: &Path,
    profile: MediaAnalysisProfile,
) -> anyhow::Result<AnalyzedMediaFileContents> {
    let document = PdfDocument::load(file_path).map_err(|error| {
        anyhow::anyhow!(error).context(format!("load pdf '{}'", file_path.display()))
    })?;
    let page_count = document.get_pages().len();
    let pages = (0..page_count)
        .map(|index| {
            let dimensions = profile
                .include_dimensions()
                .then(|| pdf_page_dimensions(&document, (index + 1) as u32))
                .flatten()
                .map(|dimensions| {
                    if profile.scale_pdf_dimensions() {
                        scale_pdf_page_dimensions(dimensions)
                    } else {
                        dimensions
                    }
                });
            let dimensions = analyzed_media_page_dimensions(dimensions);

            AnalyzedMediaPage {
                file_name: profile.pdf_page_file_name(index),
                media_type: profile.pdf_page_media_type().to_string(),
                width: dimensions.width,
                height: dimensions.height,
                file_size: 0,
                file_hash: None,
            }
        })
        .collect::<Vec<_>>();
    let files = file_path
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| vec![value.to_string()])
        .unwrap_or_default();
    Ok(AnalyzedMediaFileContents {
        pages,
        files,
        metadata_sources: CapturedMetadataSources {
            comicinfo: CapturedMetadataDocument::NotApplicable,
            epub: CapturedMetadataDocument::NotApplicable,
        },
        ..Default::default()
    })
}

fn analyzed_rar_media_page(
    file_name: String,
    unpacked_size: u64,
    dimensions: Option<MediaDimensions>,
    file_hash: Option<String>,
) -> AnalyzedMediaPage {
    let dimensions = analyzed_media_page_dimensions(dimensions);
    AnalyzedMediaPage {
        media_type: media_type_from_entry_name(&file_name),
        file_name,
        width: dimensions.width,
        height: dimensions.height,
        file_size: unpacked_size.try_into().unwrap_or(i64::MAX),
        file_hash,
    }
}

struct AnalyzedMediaPageDimensions {
    width: Option<i64>,
    height: Option<i64>,
}

fn analyzed_media_page_dimensions(
    dimensions: Option<MediaDimensions>,
) -> AnalyzedMediaPageDimensions {
    dimensions
        .map(|dimensions| AnalyzedMediaPageDimensions {
            width: Some(dimensions.width),
            height: Some(dimensions.height),
        })
        .unwrap_or(AnalyzedMediaPageDimensions {
            width: None,
            height: None,
        })
}

fn pdf_numeric_value(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some((*value).into()),
        _ => None,
    }
}

fn scale_pdf_page_dimensions(dimensions: MediaDimensions) -> MediaDimensions {
    let min_edge = dimensions.width.min(dimensions.height) as f64;
    if min_edge <= 0.0 {
        return dimensions;
    }

    let scale = 3200.0 / min_edge;
    MediaDimensions {
        width: ((dimensions.width as f64) * scale).round().max(1.0) as i64,
        height: ((dimensions.height as f64) * scale).round().max(1.0) as i64,
    }
}

fn detected_media_type_from_path(path: &Path) -> anyhow::Result<String> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let fallback = persisted_media_type_from_file_name(file_name);
    let mut file = std::fs::File::open(path).map_err(|error| {
        let debug = format!("{error:?}");
        anyhow::Error::new(error).context(format!(
            "detect media type '{}': {debug}",
            path.display()
        ))
    })?;
    let mut header = Vec::new();
    file.by_ref()
        .take(IMAGE_DIMENSIONS_MAX_READ_BYTES as u64)
        .read_to_end(&mut header)
        .map_err(|error| {
            let debug = format!("{error:?}");
            anyhow::Error::new(error).context(format!(
                "read media header '{}': {debug}",
                path.display()
            ))
        })?;

    if header.starts_with(b"Rar!\x1A\x07") {
        return Ok(detect_rar_media_type(path).to_string());
    }
    if header.starts_with(b"%PDF-") {
        return Ok("application/pdf".to_string());
    }
    if let Ok(format) = image::guess_format(&header) {
        return Ok(image_format_media_type(format).to_string());
    }
    if header.len() >= 68 && &header[60..68] == b"BOOKMOBI" {
        return Ok(MOBI_MEDIA_TYPE.to_string());
    }

    if let Ok(mut archive) = zip::ZipArchive::new(file) {
        return Ok(detect_epub_media_type_from_archive(&mut archive).to_string());
    }

    if fallback == "application/epub+zip" {
        Ok("application/octet-stream".to_string())
    } else {
        Ok(fallback.to_string())
    }
}

fn image_format_media_type(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Avif => "image/avif",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::WebP => "image/webp",
        _ => "image/*",
    }
}

fn detect_epub_media_type_from_archive<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> &'static str {
    if archive.by_name("META-INF/container.xml").is_ok() {
        "application/epub+zip"
    } else {
        "application/zip"
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::{ImageBuffer, Rgba};
    use komga_domain::discovery::MediaStatus;
    use komga_infrastructure_media_core::content::metadata_sources::MetadataSourceRequest;
    use lopdf::{Document as PdfDocument, Object, Stream, dictionary};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::{
        MOBI_MEDIA_TYPE, MediaAnalysisProfile, MediaDimensions, MediaFileAnalyzer,
        detected_media_type_from_path, image_dimensions_from_bytes_i64,
        image_dimensions_from_reader, is_rar_media_type, transient_media_type_from_path,
    };

    struct CountingImageReader {
        bytes: Vec<u8>,
        position: usize,
    }

    impl CountingImageReader {
        fn new(bytes: Vec<u8>) -> Self {
            Self { bytes, position: 0 }
        }

        fn bytes_read(&self) -> usize {
            self.position
        }

        fn total_len(&self) -> usize {
            self.bytes.len()
        }
    }

    impl Read for CountingImageReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = self.bytes.len().saturating_sub(self.position);
            let bytes_to_read = available.min(buffer.len());
            if bytes_to_read == 0 {
                return Ok(0);
            }

            let end = self.position + bytes_to_read;
            buffer[..bytes_to_read].copy_from_slice(&self.bytes[self.position..end]);
            self.position = end;
            Ok(bytes_to_read)
        }
    }

    fn unique_temp_path(case: &str, extension: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("komga-media-analysis-{case}-{nanos}.{extension}"))
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(width, height, Rgba([1, 2, 3, 255]));
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut output, image::ImageFormat::Png)
            .expect("png fixture should encode");
        output.into_inner()
    }

    fn minimal_png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x10, 0x08, 0x02, 0x00, 0x00,
            0x00, 0xF8, 0x62, 0xEA, 0x0E, 0x00, 0x00, 0x00, 0x08, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01, 0x48, 0x06, 0x89, 0xD2, 0x00, 0x00, 0x00,
            0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    fn write_zip_as_epub(path: &Path) {
        let file = File::create(path).expect("zip-as-epub fixture should be created");
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o644);
        zip.start_file("page-1.png", options)
            .expect("zip-as-epub page entry should be created");
        zip.write_all(&png_bytes(1, 1))
            .expect("zip-as-epub page bytes should be written");
        zip.finish()
            .expect("zip-as-epub fixture should finish successfully");
    }

    fn write_single_page_pdf(path: &Path, width: i64, height: i64) {
        let mut document = PdfDocument::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();
        let content_id = document.add_object(Stream::new(dictionary! {}, Vec::new()));
        let resources_id = document.add_object(dictionary! {});

        document.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
                "Contents" => content_id,
                "Resources" => resources_id,
            }),
        );
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
        document.compress();
        document
            .save(path)
            .expect("single-page pdf fixture should save");
    }

    #[test]
    fn media_file_analyzer_uses_one_boundary_for_transient_image_and_persisted_pdf() {
        let image_path = unique_temp_path("single-image", "png");
        fs::write(&image_path, png_bytes(3, 5)).expect("png fixture should be written");
        let pdf_path = unique_temp_path("single-page", "pdf");
        write_single_page_pdf(&pdf_path, 595, 842);

        let analyzer = MediaFileAnalyzer;
        let image_analysis = analyzer
            .analyze(&image_path, MediaAnalysisProfile::Transient)
            .expect("single image should analyze");
        let pdf_analysis = analyzer
            .analyze(
                &pdf_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: true,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("pdf should analyze");

        assert_eq!(image_analysis.status, MediaStatus::Ready);
        assert_eq!(image_analysis.media_type.as_str(), "image/png");
        assert_eq!(image_analysis.pages[0].width, Some(3));
        assert_eq!(image_analysis.pages[0].height, Some(5));
        assert_eq!(pdf_analysis.status, MediaStatus::Ready);
        assert_eq!(pdf_analysis.media_type.as_str(), "application/pdf");
        assert_eq!(
            pdf_analysis.metadata_sources,
            super::CapturedMetadataSources {
                comicinfo: super::CapturedMetadataDocument::NotApplicable,
                epub: super::CapturedMetadataDocument::NotApplicable,
            }
        );
        assert_eq!(pdf_analysis.pages[0].width, Some(595));
        assert_eq!(pdf_analysis.pages[0].height, Some(842));

        let _ = fs::remove_file(image_path);
        let _ = fs::remove_file(pdf_path);
    }

    #[test]
    fn image_dimensions_from_bytes_reads_header_without_decoding_full_image() {
        assert_eq!(
            image_dimensions_from_bytes_i64(&minimal_png_bytes()),
            Some(MediaDimensions {
                width: 32,
                height: 16,
            }),
        );
    }

    #[test]
    fn image_dimensions_from_reader_stops_after_dimensions_are_known() {
        let mut png_with_large_tail = minimal_png_bytes();
        png_with_large_tail.resize(1024 * 1024, 0xFF);
        let mut reader = CountingImageReader::new(png_with_large_tail);

        assert_eq!(
            image_dimensions_from_reader(&mut reader).expect("dimension read should succeed"),
            Some(MediaDimensions {
                width: 32,
                height: 16,
            }),
        );
        assert!(
            reader.bytes_read() < reader.total_len(),
            "dimensions should not require reading the whole image entry"
        );
    }

    #[test]
    fn persisted_analysis_marks_invalid_pdf_as_error_instead_of_runtime_failure() {
        let fixture_path = unique_temp_path("invalid-pdf", "pdf");
        fs::write(&fixture_path, b"not a real pdf").expect("invalid pdf fixture should be written");

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("persisted invalid pdf analysis should record media error");

        assert_eq!(analysis.status, MediaStatus::Error);
        assert_eq!(analysis.media_type, "application/pdf");
        assert!(analysis.pages.is_empty());

        let _ = fs::remove_file(fixture_path);
    }

    #[test]
    fn persisted_analysis_reports_missing_file_with_err_1018() {
        let fixture_path = unique_temp_path("missing-persisted-media", "cbz");
        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("missing persisted media should be recorded");

        assert_eq!(analysis.status, MediaStatus::Error);
        assert_eq!(analysis.media_type, "");
        assert_eq!(analysis.comment.as_deref(), Some("ERR_1018"));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_analysis_reports_filesystem_probe_errors_before_missing_file_status() {
        let parent_file = unique_temp_path("probe-parent-file", "tmp");
        fs::write(&parent_file, b"not a directory").expect("parent file fixture should be written");
        let media_path = parent_file.join("book.cbz");

        let analysis = MediaFileAnalyzer
            .analyze(
                &media_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("filesystem probe error should be persisted as media error");

        assert_eq!(analysis.status, MediaStatus::Error);
        assert_eq!(analysis.media_type, "");
        assert_eq!(analysis.comment.as_deref(), Some("ERR_1005"));

        let _ = fs::remove_file(parent_file);
    }

    #[test]
    fn media_type_detection_prefers_container_over_epub_extension() {
        let path = unique_temp_path("zip-as-epub", "epub");
        write_zip_as_epub(&path);

        assert_eq!(
            detected_media_type_from_path(path.as_path())
                .ok()
                .as_deref(),
            Some("application/zip")
        );
        assert_eq!(
            transient_media_type_from_path(path.as_path()).as_deref(),
            Some("application/zip")
        );
        let analysis = MediaFileAnalyzer
            .analyze(
                &path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("ZIP content with EPUB extension should analyze as ZIP");
        assert_eq!(analysis.status, MediaStatus::Error);
        assert_eq!(analysis.comment.as_deref(), Some("ERR_1032"));
        assert_eq!(analysis.media_type, "application/zip");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_analysis_marks_unknown_media_as_unsupported_with_err_1001() {
        let path = unique_temp_path("unsupported-persisted-media", "txt");
        fs::write(&path, b"not a supported book").expect("unsupported fixture should be written");

        let analysis = MediaFileAnalyzer
            .analyze(
                &path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("unsupported persisted media should be recorded");

        assert_eq!(analysis.status, MediaStatus::Unsupported);
        assert_eq!(analysis.comment.as_deref(), Some("ERR_1001"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_analysis_detects_rar4_versioned_media_type() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../komga/src/test/resources/archives/rar4.rar");

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("rar4 fixture analysis should succeed");

        assert_eq!(analysis.status, MediaStatus::Ready);
        assert_eq!(
            analysis.media_type,
            "application/x-rar-compressed; version=4"
        );
        assert!(!analysis.pages.is_empty());
    }

    #[test]
    fn persisted_analysis_marks_encrypted_rar_as_unsupported() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../komga/src/test/resources/archives/rar4-encrypted.rar");

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("encrypted RAR analysis should be recorded");

        assert_eq!(analysis.status, MediaStatus::Unsupported);
        assert_eq!(analysis.comment.as_deref(), Some("ERR_1002"));
    }

    #[test]
    fn persisted_mobi_analysis_keeps_mobi_media_type_when_payload_is_invalid() {
        let path = unique_temp_path("invalid-mobi", "mobi");
        let mut bytes = vec![0_u8; 68];
        bytes[60..68].copy_from_slice(b"BOOKMOBI");
        fs::write(&path, bytes).expect("invalid mobi fixture should be written");

        let analysis = MediaFileAnalyzer
            .analyze(
                &path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("invalid mobi should be represented as media error");

        assert_eq!(analysis.media_type, MOBI_MEDIA_TYPE);
        assert_eq!(analysis.status, MediaStatus::Error);
        assert!(analysis.pages.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_mobi_analysis_reads_the_local_sample_when_available() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sample/epub3.mobi");
        if !path.is_file() {
            return;
        }

        let analysis = MediaFileAnalyzer
            .analyze(
                &path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("local MOBI sample should analyze");

        assert_eq!(analysis.status, MediaStatus::Ready);
        assert_eq!(analysis.media_type, MOBI_MEDIA_TYPE);
        assert!(!analysis.pages.is_empty());
        assert!(
            analysis
                .media_files
                .iter()
                .any(|file| file.sub_type.as_deref() == Some("EPUB_PAGE"))
        );
        assert!(analysis.epub_extension_blob.is_some());
    }

    #[test]
    fn persisted_epub_analysis_keeps_reflowable_content_as_resources() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../../../komga/src/test/resources/epub/The Incomplete Theft - Ralph Burke.epub",
        );

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("EPUB fixture analysis should succeed");

        assert_eq!(analysis.status, MediaStatus::Ready);
        assert!(!analysis.epub_divina_compatible);
        assert!(!analysis.epub_is_kepub);
        assert!(
            analysis.pages.is_empty(),
            "reflowable EPUB content must not be persisted as image pages"
        );
        assert_eq!(analysis.page_count, 14);
        assert!(
            analysis.epub_extension_blob.is_some(),
            "EPUB analysis must persist its extension metadata"
        );
    }

    #[test]
    fn persisted_epub_analysis_marks_complete_image_mapping_as_divina_compatible() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../komga/src/test/resources/archives/epub3.epub");

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: false,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("fixed-layout EPUB fixture analysis should succeed");

        assert_eq!(analysis.status, MediaStatus::Ready);
        assert!(analysis.epub_divina_compatible);
        assert_eq!(analysis.pages.len(), 2);
    }

    #[test]
    fn persisted_analysis_reads_rar_page_dimensions() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../komga/src/test/resources/archives/rar4.rar");

        let analysis = MediaFileAnalyzer
            .analyze(
                &fixture_path,
                MediaAnalysisProfile::PersistedBook {
                    include_dimensions: true,
                    metadata_sources: MetadataSourceRequest::default(),
                    include_page_hashes: false,
                },
            )
            .expect("rar4 fixture analysis should succeed");

        assert_eq!(analysis.status, MediaStatus::Ready);
        assert!(
            analysis
                .pages
                .iter()
                .any(|page| page.width == Some(48) && page.height == Some(48)),
            "rar analysis should populate page dimensions"
        );
    }

    #[test]
    fn is_rar_media_type_accepts_kotlin_versioned_rar_media_types() {
        assert!(is_rar_media_type("application/x-rar-compressed; version=4"));
        assert!(is_rar_media_type("application/x-rar-compressed; version=5"));
        assert!(!is_rar_media_type("application/vnd.comicbook-rar"));
        assert!(!is_rar_media_type("application/x-rar-compressed"));
    }

    #[test]
    fn media_type_detection_is_shared_between_transient_and_persisted_paths() {
        let rar4 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../../komga/src/test/resources/archives/rar4.rar");

        assert_eq!(
            transient_media_type_from_path(rar4.as_path()).as_deref(),
            Some("application/x-rar-compressed; version=4")
        );
        assert_eq!(
            detected_media_type_from_path(rar4.as_path())
                .ok()
                .as_deref(),
            Some("application/x-rar-compressed; version=4")
        );
    }
}
