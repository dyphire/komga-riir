use crate::media_assets::EpubNavigationContentPort;

/// Read progress entry surfaced to KOReader/Kobo device handlers.
///
/// `locator` carries the raw stored locator blob; callers decode it lazily.
#[derive(Clone)]
pub struct PersistedReadProgressRecord {
    pub page: i64,
    pub completed: bool,
    pub created: String,
    pub last_modified: String,
    pub device_id: String,
    pub device_name: String,
    pub locator: Option<Vec<u8>>,
}

/// Book identity surfaced to KOReader's hash-based lookup.
#[derive(Clone)]
pub struct KoreaderBookTarget {
    pub id: String,
    pub page_count: u64,
    pub media_type: String,
}

/// Book metadata snapshot served to Kobo sync.
#[derive(Clone)]
pub struct KoboMetadataRecord {
    pub title: String,
    pub summary: String,
    pub release_date: Option<String>,
    pub created_date: Option<String>,
    pub language: String,
    pub file_size: u64,
    /// File size of the persisted kepub projection, when it has been converted.
    pub kepub_file_size: Option<u64>,
    pub file_name: String,
    pub media_type: String,
    pub contributor_names: Vec<String>,
    pub isbn: Option<String>,
    pub publisher_name: Option<String>,
    pub cover_image_id: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub series_number: Option<String>,
    pub series_number_float: Option<f64>,
    pub oneshot: bool,
    pub is_kepub: bool,
    pub is_pre_paginated: bool,
}

pub fn kobo_metadata_pre_paginated(
    content: &dyn EpubNavigationContentPort,
    extension_blob: Option<&[u8]>,
) -> anyhow::Result<bool> {
    let Some(blob) = extension_blob else {
        return Ok(false);
    };

    content
        .decode_epub_navigation_extension(blob)
        .map(|extension| extension.is_fixed_layout)
}

/// Errors returned when KOReader resolves a book by content hash.
#[derive(Debug)]
pub enum KoreaderBookLookupError {
    Persistence,
    Conflict,
}
