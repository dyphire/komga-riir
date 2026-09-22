use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

use komga_application::identity_access::{
    KoboMetadataRecord, KoboSyncBookSnapshot, KoboSyncEvent, KoboSyncReadListSnapshot,
    KoboSyncReadProgressSnapshot, now_sync_marker,
};

use super::reading_state::{
    KoboReadingStateBookmarkPayload, KoboReadingStateLocationPayload, KoboReadingStatePayload,
    KoboReadingStateStatisticsPayload, KoboReadingStateStatusPayload,
};

struct KoboBookDownloadFormat {
    product_format: &'static str,
    convert_kepub: bool,
}

impl KoboBookDownloadFormat {
    fn from_metadata(metadata: &KoboMetadataRecord) -> Self {
        if metadata.is_pre_paginated {
            Self {
                product_format: "EPUB3FL",
                convert_kepub: false,
            }
        } else {
            Self {
                product_format: "KEPUB",
                convert_kepub: !metadata.is_kepub,
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct KoboBookMetadataWire {
    categories: Vec<String>,
    contributor_roles: Vec<KoboContributorRoleWire>,
    contributors: Vec<String>,
    cover_image_id: Option<String>,
    cross_revision_id: String,
    current_display_price: KoboPriceWire,
    current_love_display_price: KoboPriceWire,
    description: String,
    download_urls: Vec<KoboDownloadUrlWire>,
    entitlement_id: String,
    external_ids: Vec<String>,
    genre: String,
    is_eligible_for_kobo_love: bool,
    is_internet_archive: bool,
    is_pre_order: bool,
    is_social_enabled: bool,
    #[serde(rename = "ISBN")]
    isbn: Option<String>,
    language: String,
    phonetic_pronunciations: BTreeMap<String, String>,
    publication_date: Option<String>,
    publisher: Option<KoboPublisherWire>,
    revision_id: String,
    series: Option<KoboSeriesWire>,
    // Standalone metadata includes these fields as explicit nulls; sync metadata omits them.
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<Option<String>>,
    #[serde(rename = "SubTitle", skip_serializing_if = "Option::is_none")]
    subtitle: Option<Option<String>>,
    title: String,
    work_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboContributorRoleWire {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboPriceWire {
    currency_code: &'static str,
    total_amount: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboDownloadUrlWire {
    drm_type: &'static str,
    format: String,
    platform: &'static str,
    size: u64,
    url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboPublisherWire {
    imprint: &'static str,
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboSeriesWire {
    id: String,
    name: String,
    number: String,
    #[serde(rename = "NumberFloat")]
    number_float: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct KoboNewEntitlementWire {
    book_entitlement: KoboEntitlementWire,
    book_metadata: KoboBookMetadataWire,
    reading_state: KoboReadingStatePayload,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct KoboChangedEntitlementWire {
    book_entitlement: KoboEntitlementWire,
    book_metadata: KoboBookMetadataWire,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct KoboChangedReadingStateWire {
    reading_state: KoboReadingStatePayload,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboEntitlementWire {
    accessibility: &'static str,
    active_period: KoboActivePeriodWire,
    created: String,
    cross_revision_id: String,
    id: String,
    is_hidden_from_archive: bool,
    is_locked: bool,
    is_removed: bool,
    last_modified: String,
    origin_category: &'static str,
    revision_id: String,
    status: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboActivePeriodWire {
    from: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct KoboTagEventWire {
    tag: KoboTagWire,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboTagWire {
    id: String,
    created: String,
    last_modified: String,
    name: String,
    #[serde(rename = "Type")]
    tag_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<KoboTagItemWire>>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KoboTagItemWire {
    revision_id: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
}

pub(super) enum KoboSyncEventPayload {
    Raw(Value),
    NewEntitlement(Box<KoboNewEntitlementWire>),
    ChangedProductMetadata(Box<KoboBookMetadataWire>),
    ChangedEntitlement(Box<KoboChangedEntitlementWire>),
    ChangedReadingState(Box<KoboChangedReadingStateWire>),
    NewTag(Box<KoboTagEventWire>),
    ChangedTag(Box<KoboTagEventWire>),
    DeletedTag(Box<KoboTagEventWire>),
}

impl Serialize for KoboSyncEventPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Raw(value) => value.serialize(serializer),
            Self::NewEntitlement(value) => {
                serialize_kobo_event(serializer, "NewEntitlement", value)
            }
            Self::ChangedProductMetadata(value) => {
                serialize_kobo_event(serializer, "ChangedProductMetadata", value)
            }
            Self::ChangedEntitlement(value) => {
                serialize_kobo_event(serializer, "ChangedEntitlement", value)
            }
            Self::ChangedReadingState(value) => {
                serialize_kobo_event(serializer, "ChangedReadingState", value)
            }
            Self::NewTag(value) => serialize_kobo_event(serializer, "NewTag", value),
            Self::ChangedTag(value) => serialize_kobo_event(serializer, "ChangedTag", value),
            Self::DeletedTag(value) => serialize_kobo_event(serializer, "DeletedTag", value),
        }
    }
}

fn serialize_kobo_event<S, T>(serializer: S, name: &str, value: &T) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    let mut map = serializer.serialize_map(Some(1))?;
    map.serialize_entry(name, value)?;
    map.end()
}

pub(super) fn build_kobo_book_metadata_payload(
    book_id: &str,
    metadata: &KoboMetadataRecord,
    base_url: &str,
    auth_token: &str,
) -> Vec<KoboBookMetadataWire> {
    let download_format = KoboBookDownloadFormat::from_metadata(metadata);

    vec![kobo_book_metadata_wire(KoboBookMetadataWireInput {
        id: book_id,
        title: &metadata.title,
        summary: &metadata.summary,
        publication_date: metadata.release_date.as_deref(),
        publication_fallback_date: metadata.created_date.as_deref(),
        language: &metadata.language,
        file_size: metadata.file_size,
        kepub_file_size: metadata.kepub_file_size,
        contributor_names: &metadata.contributor_names,
        isbn: metadata.isbn.as_deref(),
        publisher_name: metadata.publisher_name.as_deref(),
        cover_image_id: metadata.cover_image_id.as_deref(),
        series_id: metadata.series_id.as_deref(),
        series_name: metadata.series_name.as_deref(),
        series_number: metadata.series_number.as_deref(),
        series_number_float: metadata.series_number_float,
        oneshot: metadata.oneshot,
        download_format: download_format.product_format,
        download_url: format!(
            "{base_url}/kobo/{auth_token}/v1/books/{book_id}/file/epub?convert_kepub={}",
            download_format.convert_kepub,
        ),
        include_standalone_fields: true,
    })]
}

fn build_kobo_new_entitlement(
    book: &KoboSyncBookSnapshot,
    progress: Option<&KoboSyncReadProgressSnapshot>,
    base_url: &str,
    auth_token: &str,
) -> Result<KoboSyncEventPayload, &'static str> {
    let reading_state = kobo_reading_state_from_snapshot(book, progress)?;
    Ok(kobo_new_entitlement_event(
        book,
        reading_state,
        base_url,
        auth_token,
    ))
}

fn build_kobo_changed_product_metadata(
    book: &KoboSyncBookSnapshot,
    base_url: &str,
    auth_token: &str,
) -> KoboSyncEventPayload {
    KoboSyncEventPayload::ChangedProductMetadata(Box::new(kobo_book_metadata_from_snapshot(
        book, base_url, auth_token,
    )))
}

fn build_kobo_changed_entitlement_removed(
    book: &KoboSyncBookSnapshot,
    base_url: &str,
    auth_token: &str,
) -> KoboSyncEventPayload {
    KoboSyncEventPayload::ChangedEntitlement(Box::new(kobo_changed_entitlement_removed_event(
        book, base_url, auth_token,
    )))
}

fn build_kobo_changed_reading_state(
    book: &KoboSyncBookSnapshot,
    progress: &KoboSyncReadProgressSnapshot,
) -> Result<KoboSyncEventPayload, &'static str> {
    let reading_state = kobo_reading_state_from_snapshot(book, Some(progress))?;
    Ok(KoboSyncEventPayload::ChangedReadingState(Box::new(
        kobo_changed_reading_state_event(reading_state),
    )))
}

fn build_kobo_new_tag(readlist: &KoboSyncReadListSnapshot) -> KoboSyncEventPayload {
    KoboSyncEventPayload::NewTag(Box::new(KoboTagEventWire {
        tag: kobo_tag_from_snapshot(readlist, true),
    }))
}

fn build_kobo_changed_tag(readlist: &KoboSyncReadListSnapshot) -> KoboSyncEventPayload {
    KoboSyncEventPayload::ChangedTag(Box::new(KoboTagEventWire {
        tag: kobo_tag_from_snapshot(readlist, true),
    }))
}

fn build_kobo_deleted_tag(readlist: &KoboSyncReadListSnapshot) -> KoboSyncEventPayload {
    KoboSyncEventPayload::DeletedTag(Box::new(KoboTagEventWire {
        tag: kobo_tag_from_snapshot(readlist, false),
    }))
}

pub(super) fn build_kobo_sync_event_payload(
    event: KoboSyncEvent,
    base_url: &str,
    auth_token: &str,
) -> Result<KoboSyncEventPayload, &'static str> {
    match event {
        KoboSyncEvent::Raw(value) => Ok(KoboSyncEventPayload::Raw(value)),
        KoboSyncEvent::NewEntitlement { book, progress } => {
            build_kobo_new_entitlement(&book, progress.as_ref(), base_url, auth_token)
        }
        KoboSyncEvent::ChangedProductMetadata { book } => Ok(build_kobo_changed_product_metadata(
            &book, base_url, auth_token,
        )),
        KoboSyncEvent::ChangedEntitlementRemoved { book } => Ok(
            build_kobo_changed_entitlement_removed(&book, base_url, auth_token),
        ),
        KoboSyncEvent::ChangedReadingState { book, progress } => {
            build_kobo_changed_reading_state(&book, &progress)
        }
        KoboSyncEvent::NewTag { readlist } => Ok(build_kobo_new_tag(&readlist)),
        KoboSyncEvent::ChangedTag { readlist } => Ok(build_kobo_changed_tag(&readlist)),
        KoboSyncEvent::DeletedTag { readlist } => Ok(build_kobo_deleted_tag(&readlist)),
    }
}

fn kobo_description(summary: &str) -> String {
    if summary.trim().is_empty() {
        " ".to_string()
    } else {
        summary.to_string()
    }
}

fn kobo_language(language: &str) -> String {
    let language = language.trim();
    if language.is_empty() {
        "en".to_string()
    } else {
        language
            .chars()
            .take(2)
            .collect::<String>()
            .to_ascii_lowercase()
    }
}

fn kobo_publication_date_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else if value.len() == 10 && value.as_bytes().get(4) == Some(&b'-') {
        Some(format!("{value}T00:00:00Z"))
    } else {
        Some(value.to_string())
    }
}

fn kobo_reading_state_from_snapshot(
    book: &KoboSyncBookSnapshot,
    progress: Option<&KoboSyncReadProgressSnapshot>,
) -> Result<KoboReadingStatePayload, &'static str> {
    if let Some(progress) = progress {
        let locator = parse_locator_payload(progress.locator.as_deref())?;
        let source_progress = locator
            .get("locations")
            .and_then(|value| value.get("progression"))
            .and_then(Value::as_f64);
        let total_progress = locator
            .get("locations")
            .and_then(|value| value.get("totalProgression"))
            .and_then(Value::as_f64);
        let source = locator
            .get("href")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let value = locator
            .get("koboSpan")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let status = if progress.completed {
            "Finished"
        } else {
            "Reading"
        };
        Ok(KoboReadingStatePayload {
            created: progress.created.clone(),
            current_bookmark: KoboReadingStateBookmarkPayload {
                last_modified: progress.last_modified.clone(),
                progress_percent: total_progress.map(|value| value * 100.0),
                content_source_progress_percent: source_progress.map(|value| value * 100.0),
                location: source.map(|source| KoboReadingStateLocationPayload {
                    source,
                    location_type: "KoboSpan",
                    value: Some(value),
                }),
            },
            entitlement_id: book.id.clone(),
            last_modified: progress.last_modified.clone(),
            priority_timestamp: progress.last_modified.clone(),
            statistics: KoboReadingStateStatisticsPayload {
                last_modified: progress.last_modified.clone(),
            },
            status_info: KoboReadingStateStatusPayload {
                last_modified: progress.last_modified.clone(),
                status,
                times_started_reading: 1,
                last_time_finished: None,
                last_time_started_reading: None,
            },
        })
    } else {
        Ok(KoboReadingStatePayload {
            created: book.created.clone(),
            current_bookmark: KoboReadingStateBookmarkPayload {
                last_modified: book.created.clone(),
                progress_percent: None,
                content_source_progress_percent: None,
                location: None,
            },
            entitlement_id: book.id.clone(),
            last_modified: book.created.clone(),
            priority_timestamp: book.created.clone(),
            statistics: KoboReadingStateStatisticsPayload {
                last_modified: book.created.clone(),
            },
            status_info: KoboReadingStateStatusPayload {
                last_modified: book.created.clone(),
                status: "ReadyToRead",
                times_started_reading: 0,
                last_time_finished: None,
                last_time_started_reading: None,
            },
        })
    }
}

struct KoboBookMetadataWireInput<'a> {
    id: &'a str,
    title: &'a str,
    summary: &'a str,
    publication_date: Option<&'a str>,
    publication_fallback_date: Option<&'a str>,
    language: &'a str,
    file_size: u64,
    kepub_file_size: Option<u64>,
    contributor_names: &'a [String],
    isbn: Option<&'a str>,
    publisher_name: Option<&'a str>,
    cover_image_id: Option<&'a str>,
    series_id: Option<&'a str>,
    series_name: Option<&'a str>,
    series_number: Option<&'a str>,
    series_number_float: Option<f64>,
    oneshot: bool,
    download_format: &'a str,
    download_url: String,
    include_standalone_fields: bool,
}

fn kobo_book_metadata_wire(input: KoboBookMetadataWireInput<'_>) -> KoboBookMetadataWire {
    KoboBookMetadataWire {
        categories: vec!["00000000-0000-0000-0000-000000000001".to_string()],
        contributor_roles: input
            .contributor_names
            .iter()
            .map(|name| KoboContributorRoleWire { name: name.clone() })
            .collect(),
        contributors: input.contributor_names.to_vec(),
        cover_image_id: input.cover_image_id.map(str::to_owned),
        cross_revision_id: input.id.to_string(),
        current_display_price: KoboPriceWire {
            currency_code: "USD",
            total_amount: 0,
        },
        current_love_display_price: KoboPriceWire {
            currency_code: "USD",
            total_amount: 0,
        },
        description: kobo_description(input.summary),
        download_urls: vec![KoboDownloadUrlWire {
            drm_type: "None",
            format: input.download_format.to_string(),
            platform: "Generic",
            // A converted kepub projection is a different file with a different
            // size than the source epub; prefer it when reporting a KEPUB download.
            size: if input.download_format == "KEPUB" {
                input.kepub_file_size.unwrap_or(input.file_size)
            } else {
                input.file_size
            },
            url: input.download_url,
        }],
        entitlement_id: input.id.to_string(),
        external_ids: Vec::new(),
        genre: "00000000-0000-0000-0000-000000000001".to_string(),
        is_eligible_for_kobo_love: false,
        is_internet_archive: false,
        is_pre_order: false,
        is_social_enabled: true,
        isbn: input.isbn.map(str::to_owned),
        language: kobo_language(input.language),
        phonetic_pronunciations: BTreeMap::new(),
        publication_date: input
            .publication_date
            .or(input.publication_fallback_date)
            .and_then(kobo_publication_date_value),
        publisher: input.publisher_name.map(|name| KoboPublisherWire {
            imprint: "",
            name: name.to_string(),
        }),
        revision_id: input.id.to_string(),
        series: if input.oneshot {
            None
        } else if let (
            Some(series_id),
            Some(series_name),
            Some(series_number),
            Some(series_number_float),
        ) = (
            input.series_id,
            input.series_name,
            input.series_number,
            input.series_number_float,
        ) {
            Some(KoboSeriesWire {
                id: series_id.to_string(),
                name: series_name.to_string(),
                number: series_number.to_string(),
                number_float: series_number_float,
            })
        } else {
            None
        },
        slug: input.include_standalone_fields.then_some(None::<String>),
        subtitle: input.include_standalone_fields.then_some(None::<String>),
        title: input.title.to_string(),
        work_id: input.id.to_string(),
    }
}

fn kobo_book_metadata_from_snapshot(
    book: &KoboSyncBookSnapshot,
    base_url: &str,
    auth_token: &str,
) -> KoboBookMetadataWire {
    kobo_book_metadata_wire(KoboBookMetadataWireInput {
        id: &book.id,
        title: &book.title,
        summary: &book.summary,
        publication_date: book.release_date.as_deref(),
        publication_fallback_date: Some(book.created.as_str()),
        language: &book.language,
        file_size: book.file_size,
        kepub_file_size: None,
        contributor_names: &book.contributor_names,
        isbn: book.isbn.as_deref(),
        publisher_name: book.publisher_name.as_deref(),
        cover_image_id: book.cover_image_id.as_deref(),
        series_id: book.series_id.as_deref(),
        series_name: book.series_name.as_deref(),
        series_number: book.series_number.as_deref(),
        series_number_float: book.series_number_float,
        oneshot: book.oneshot,
        download_format: "EPUB",
        download_url: format!(
            "{base_url}/kobo/{auth_token}/v1/books/{}/file/epub",
            book.id
        ),
        include_standalone_fields: false,
    })
}

fn kobo_entitlement_from_snapshot(
    book: &KoboSyncBookSnapshot,
    is_removed: bool,
) -> KoboEntitlementWire {
    KoboEntitlementWire {
        accessibility: "Full",
        active_period: KoboActivePeriodWire {
            from: now_sync_marker(),
        },
        created: book.created.clone(),
        cross_revision_id: book.id.clone(),
        id: book.id.clone(),
        is_hidden_from_archive: false,
        is_locked: false,
        is_removed,
        last_modified: book.last_modified.clone(),
        origin_category: "Imported",
        revision_id: book.id.clone(),
        status: "Active",
    }
}

fn kobo_tag_from_snapshot(readlist: &KoboSyncReadListSnapshot, include_items: bool) -> KoboTagWire {
    KoboTagWire {
        id: readlist.id.clone(),
        created: readlist.created.clone(),
        last_modified: readlist.last_modified.clone(),
        name: readlist.name.clone(),
        tag_type: "UserTag",
        items: include_items.then(|| {
            readlist
                .items
                .iter()
                .map(|book_id| KoboTagItemWire {
                    revision_id: book_id.clone(),
                    item_type: "ProductRevisionTagItem",
                })
                .collect()
        }),
    }
}

fn kobo_new_entitlement_event(
    book: &KoboSyncBookSnapshot,
    reading_state: KoboReadingStatePayload,
    base_url: &str,
    auth_token: &str,
) -> KoboSyncEventPayload {
    KoboSyncEventPayload::NewEntitlement(Box::new(KoboNewEntitlementWire {
        book_entitlement: kobo_entitlement_from_snapshot(book, false),
        book_metadata: kobo_book_metadata_from_snapshot(book, base_url, auth_token),
        reading_state,
    }))
}

fn kobo_changed_entitlement_removed_event(
    book: &KoboSyncBookSnapshot,
    base_url: &str,
    auth_token: &str,
) -> KoboChangedEntitlementWire {
    KoboChangedEntitlementWire {
        book_entitlement: kobo_entitlement_from_snapshot(book, true),
        book_metadata: kobo_book_metadata_from_snapshot(book, base_url, auth_token),
    }
}

fn kobo_changed_reading_state_event(
    reading_state: KoboReadingStatePayload,
) -> KoboChangedReadingStateWire {
    KoboChangedReadingStateWire { reading_state }
}

fn parse_locator_payload(locator: Option<&[u8]>) -> Result<Value, &'static str> {
    let Some(locator) = locator else {
        return Ok(Value::Object(Default::default()));
    };

    let payload = serde_json::from_slice::<Value>(locator)
        .map_err(|_| "invalid persisted read-progress locator")?;
    if payload.is_object() {
        Ok(payload)
    } else {
        Err("invalid persisted read-progress locator")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn kobo_protocol_payload_builds_standalone_book_metadata() {
        let metadata = KoboMetadataRecord {
            title: "Book One".to_string(),
            summary: String::new(),
            release_date: Some("2026-02-03".to_string()),
            created_date: Some("2026-01-01T00:00:00Z".to_string()),
            language: "FR-ca".to_string(),
            file_size: 1234,
            kepub_file_size: None,
            file_name: "book.epub".to_string(),
            media_type: "application/epub+zip".to_string(),
            contributor_names: vec!["Jane Writer".to_string()],
            isbn: Some("9781234567890".to_string()),
            publisher_name: Some("PubHouse".to_string()),
            cover_image_id: Some("cover-1".to_string()),
            series_id: Some("series-1".to_string()),
            series_name: Some("Series One".to_string()),
            series_number: Some("1".to_string()),
            series_number_float: Some(1.0),
            oneshot: false,
            is_kepub: false,
            is_pre_paginated: false,
        };

        let payload = build_kobo_book_metadata_payload(
            "book-1",
            &metadata,
            "http://localhost:8080",
            "token-1",
        );
        let book = serde_json::to_value(payload.first().expect("metadata item expected"))
            .expect("metadata should serialize");

        assert_eq!(
            book.get("Description"),
            Some(&Value::String(" ".to_string()))
        );
        assert_eq!(book.get("Language"), Some(&Value::String("fr".to_string())));
        assert_eq!(
            book.get("PublicationDate"),
            Some(&Value::String("2026-02-03T00:00:00Z".to_string()))
        );
        assert_eq!(
            book.get("DownloadUrls")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|download| download.get("Format")),
            Some(&Value::String("KEPUB".to_string()))
        );
        assert_eq!(
            book.get("DownloadUrls")
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|download| download.get("Url")),
            Some(&Value::String(
                "http://localhost:8080/kobo/token-1/v1/books/book-1/file/epub?convert_kepub=true"
                    .to_string()
            ))
        );
        assert_eq!(
            book.get("Series")
                .and_then(|series| series.get("NumberFloat")),
            Some(&json!(1.0))
        );
        assert_eq!(book.get("Slug"), Some(&Value::Null));
        assert_eq!(book.get("SubTitle"), Some(&Value::Null));
    }

    #[test]
    fn kobo_metadata_uses_persisted_kepub_size_for_kepub_download() {
        let metadata = KoboMetadataRecord {
            title: "Book One".to_string(),
            summary: String::new(),
            release_date: Some("2026-02-03".to_string()),
            created_date: Some("2026-01-01T00:00:00Z".to_string()),
            language: "FR-ca".to_string(),
            file_size: 1234,
            kepub_file_size: Some(9876),
            file_name: "book.epub".to_string(),
            media_type: "application/epub+zip".to_string(),
            contributor_names: vec!["Jane Writer".to_string()],
            isbn: Some("9781234567890".to_string()),
            publisher_name: Some("PubHouse".to_string()),
            cover_image_id: Some("cover-1".to_string()),
            series_id: Some("series-1".to_string()),
            series_name: Some("Series One".to_string()),
            series_number: Some("1".to_string()),
            series_number_float: Some(1.0),
            oneshot: false,
            is_kepub: false,
            is_pre_paginated: false,
        };

        let payload = build_kobo_book_metadata_payload(
            "book-1",
            &metadata,
            "http://localhost:8080",
            "token-1",
        );
        let book = serde_json::to_value(payload.first().expect("metadata item expected"))
            .expect("metadata should serialize");

        let download = book
            .get("DownloadUrls")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .expect("download url expected");
        assert_eq!(
            download.get("Format"),
            Some(&Value::String("KEPUB".to_string()))
        );
        assert_eq!(download.get("Size"), Some(&json!(9876)));
    }

    #[test]
    fn kobo_metadata_falls_back_to_epub_size_without_kepub_projection() {
        let metadata = KoboMetadataRecord {
            title: "Book One".to_string(),
            summary: String::new(),
            release_date: Some("2026-02-03".to_string()),
            created_date: Some("2026-01-01T00:00:00Z".to_string()),
            language: "FR-ca".to_string(),
            file_size: 1234,
            kepub_file_size: None,
            file_name: "book.epub".to_string(),
            media_type: "application/epub+zip".to_string(),
            contributor_names: vec!["Jane Writer".to_string()],
            isbn: Some("9781234567890".to_string()),
            publisher_name: Some("PubHouse".to_string()),
            cover_image_id: Some("cover-1".to_string()),
            series_id: Some("series-1".to_string()),
            series_name: Some("Series One".to_string()),
            series_number: Some("1".to_string()),
            series_number_float: Some(1.0),
            oneshot: false,
            is_kepub: false,
            is_pre_paginated: false,
        };

        let payload = build_kobo_book_metadata_payload(
            "book-1",
            &metadata,
            "http://localhost:8080",
            "token-1",
        );
        let book = serde_json::to_value(payload.first().expect("metadata item expected"))
            .expect("metadata should serialize");

        let download = book
            .get("DownloadUrls")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .expect("download url expected");
        assert_eq!(download.get("Size"), Some(&json!(1234)));
    }

    #[test]
    fn kobo_sync_event_rejects_invalid_persisted_locator() {
        let result = build_kobo_sync_event_payload(
            KoboSyncEvent::ChangedReadingState {
                book: sync_book_snapshot(),
                progress: KoboSyncReadProgressSnapshot {
                    page: 3,
                    completed: false,
                    created: "2026-01-01T00:00:00Z".to_string(),
                    last_modified: "2026-01-02T00:00:00Z".to_string(),
                    locator: Some(b"not-json".to_vec()),
                },
            },
            "http://localhost:8080",
            "token-1",
        );

        assert!(matches!(
            result,
            Err("invalid persisted read-progress locator")
        ));
    }

    fn sync_book_snapshot() -> KoboSyncBookSnapshot {
        KoboSyncBookSnapshot {
            id: "book-1".to_string(),
            title: "Book One".to_string(),
            summary: String::new(),
            release_date: Some("2026-02-03".to_string()),
            language: "en".to_string(),
            file_size: 1234,
            page_count: 1,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_modified: "2026-01-02T00:00:00Z".to_string(),
            contributor_names: Vec::new(),
            isbn: None,
            publisher_name: None,
            cover_image_id: None,
            series_id: None,
            series_name: None,
            series_number: None,
            series_number_float: None,
            oneshot: false,
        }
    }
}
