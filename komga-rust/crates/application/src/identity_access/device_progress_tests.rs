use std::sync::Arc;
use std::sync::Mutex;

use serde_json::Value;
use serde_json::json;

use crate::identity_access::{
    DeviceProgressPageCountPort, DeviceProgressService, DeviceSyncPort, DeviceThumbnailBinary,
    KoboReadingStateStatus, KoboReadingStateUpdate, KoreaderBookLookupError, KoreaderBookTarget,
    PersistedReadProgressRecord,
};
use crate::media_assets::{
    BookProgressionInput, BookProgressionRecord, BookProgressionWriteReaderPort,
    BookProgressionWriterPort, EpubExtensionBlob, EpubNavigationContentPort,
    EpubNavigationExtension, EpubNavigationExtensionReaderPort, EpubNavigationPosition,
    EpubNavigationReaderPort,
};

#[tokio::test]
async fn koreader_visual_progress_update_persists_typed_book_progression() {
    let device_sync = Arc::new(TestDeviceSync {
        koreader_target: Some(KoreaderBookTarget {
            id: "book-1".to_string(),
            page_count: 10,
            media_type: "application/vnd.comicbook+zip".to_string(),
        }),
        ..TestDeviceSync::default()
    });
    let progress = Arc::new(TestProgressWriter::default());
    let reader = NoopMediaReader::default();
    let content = NoopContentResolver::default();
    let service =
        DeviceProgressService::new(device_sync.as_ref(), &reader, &content, progress.as_ref());

    service
        .update_koreader_progress(
            "user-1",
            crate::identity_access::KoreaderProgressUpdate {
                document: "hash-book-1".to_string(),
                percentage: 0.7,
                progress: "7".to_string(),
                device: "KOReader".to_string(),
                device_id: "device-1".to_string(),
                modified: "2026-06-07T12:00:00Z".to_string(),
            },
        )
        .await
        .expect("visual KOReader progress should persist");

    let persisted = progress.persisted.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].book_id, "book-1");
    assert_eq!(persisted[0].user_id, "user-1");
    assert_eq!(persisted[0].page, 7);
    assert!(!persisted[0].completed);
    assert_eq!(
        persisted[0].locator,
        Some(json!({
            "koreaderProgress": "7",
            "locations": {
                "position": 7,
                "totalProgression": 0.7,
            },
        }))
    );
}

#[tokio::test]
async fn koreader_progress_maps_epub_locator_href_to_doc_fragment() {
    let device_sync = TestDeviceSync {
        koreader_target: Some(KoreaderBookTarget {
            id: "book-1".to_string(),
            page_count: 10,
            media_type: "application/epub+zip".to_string(),
        }),
        read_progress: Some(PersistedReadProgressRecord {
            page: 3,
            completed: false,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_modified: "2026-01-02T00:00:00Z".to_string(),
            device_id: "device-a".to_string(),
            device_name: "KOReader".to_string(),
            locator: Some(
                serde_json::to_vec(&json!({
                    "href": "chapter-2.xhtml",
                    "locations": {
                        "totalProgression": 0.5,
                    }
                }))
                .expect("locator should serialize"),
            ),
        }),
    };
    let reader = NoopMediaReader {
        epub_extension_blob: Some(EpubExtensionBlob {
            extension_class: "org.gotson.komga.domain.model.MediaExtensionEpub".to_string(),
            bytes: Vec::new(),
        }),
        ..NoopMediaReader::default()
    };
    let content = NoopContentResolver {
        positions_extension: EpubNavigationExtension {
            positions: vec![
                epub_position(json!({"href": "chapter-1.xhtml"})),
                epub_position(json!({"href": "chapter-2.xhtml"})),
            ],
            is_fixed_layout: false,
            ..EpubNavigationExtension::default()
        },
    };
    let progress = TestProgressWriter::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    let snapshot = service
        .koreader_progress("hash-book-1", "user-1")
        .await
        .expect("KOReader progress should load");

    assert_eq!(snapshot.percentage, 0.5);
    assert_eq!(snapshot.progress, "/body/DocFragment[2].0");
    assert_eq!(snapshot.device, "KOReader");
    assert_eq!(snapshot.device_id, "device-a");
}

#[tokio::test]
async fn koreader_progress_propagates_epub_navigation_load_error() {
    let device_sync = TestDeviceSync {
        koreader_target: Some(KoreaderBookTarget {
            id: "book-1".to_string(),
            page_count: 10,
            media_type: "application/epub+zip".to_string(),
        }),
        read_progress: Some(PersistedReadProgressRecord {
            page: 3,
            completed: false,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_modified: "2026-01-02T00:00:00Z".to_string(),
            device_id: "device-a".to_string(),
            device_name: "KOReader".to_string(),
            locator: Some(
                serde_json::to_vec(&json!({
                    "href": "chapter-2.xhtml",
                    "locations": {
                        "totalProgression": 0.5,
                    }
                }))
                .expect("locator should serialize"),
            ),
        }),
    };
    let reader = NoopMediaReader {
        book_media_files_error: Some("failed to load media files".to_string()),
        ..NoopMediaReader::default()
    };
    let content = NoopContentResolver::default();
    let progress = TestProgressWriter::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    let error = match service.koreader_progress("hash-book-1", "user-1").await {
        Ok(_) => panic!("epub navigation load error should propagate"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        crate::identity_access::DeviceProgressError::Persistence
    );
}

#[tokio::test]
async fn kobo_reading_state_returns_typed_progress_values() {
    let device_sync = TestDeviceSync {
        read_progress: Some(PersistedReadProgressRecord {
            page: 3,
            completed: false,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_modified: "2026-01-02T00:00:00Z".to_string(),
            device_id: "device-a".to_string(),
            device_name: "KOReader".to_string(),
            locator: Some(
                serde_json::to_vec(&json!({
                    "href": "/chapter-2.xhtml",
                    "koboSpan": "span-2",
                    "locations": {
                        "progression": 0.25,
                        "totalProgression": 0.5,
                    }
                }))
                .expect("locator should serialize"),
            ),
        }),
        ..TestDeviceSync::default()
    };
    let progress = TestProgressWriter::default();
    let reader = NoopMediaReader::default();
    let content = NoopContentResolver::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    let snapshot = service
        .kobo_reading_state("book-1", "user-1", "2026-01-01T00:00:00Z")
        .await
        .expect("kobo reading state should build");

    assert_eq!(snapshot.book_id, "book-1");
    assert_eq!(snapshot.created, "2026-01-01T00:00:00Z");
    assert_eq!(snapshot.last_modified, "2026-01-02T00:00:00Z");
    assert_eq!(snapshot.status.as_str(), "Reading");
    assert_eq!(snapshot.times_started_reading, 1);
    assert_eq!(snapshot.total_progress_percent, Some(50.0));
    assert_eq!(snapshot.content_source_progress_percent, Some(25.0));
    let location = snapshot
        .location
        .expect("kobo reading state should include location");
    assert_eq!(location.source, "/chapter-2.xhtml");
    assert_eq!(location.kobo_span.as_deref(), Some("span-2"));
}

#[tokio::test]
async fn kobo_reading_state_rejects_invalid_persisted_locator() {
    let device_sync = TestDeviceSync {
        read_progress: Some(PersistedReadProgressRecord {
            page: 3,
            completed: false,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_modified: "2026-01-02T00:00:00Z".to_string(),
            device_id: "device-a".to_string(),
            device_name: "KOReader".to_string(),
            locator: Some(b"not-json".to_vec()),
        }),
        ..TestDeviceSync::default()
    };
    let progress = TestProgressWriter::default();
    let reader = NoopMediaReader::default();
    let content = NoopContentResolver::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    let error = service
        .kobo_reading_state("book-1", "user-1", "2026-01-01T00:00:00Z")
        .await
        .expect_err("invalid persisted locator should reject the reading state");

    assert_eq!(
        error,
        crate::identity_access::DeviceProgressError::Persistence
    );
}

#[tokio::test]
async fn kobo_reading_state_update_normalizes_locator_and_persists_progression() {
    let device_sync = TestDeviceSync::default();
    let reader = NoopMediaReader {
        media_files: vec!["/book-1.xhtml".to_string()],
        epub_extension_blob: Some(EpubExtensionBlob {
            extension_class: "org.gotson.komga.domain.model.MediaExtensionEpub".to_string(),
            bytes: Vec::new(),
        }),
        page_count: Some(10),
        ..NoopMediaReader::default()
    };
    let content = NoopContentResolver {
        positions_extension: EpubNavigationExtension {
            positions: vec![epub_position(json!({
                "href": "book-1.xhtml",
                "type": "application/xhtml+xml",
                "koboSpan": "kobo.5.1",
                "locations": {
                    "progression": 0.5,
                    "totalProgression": 0.21,
                    "position": 2
                }
            }))],
            is_fixed_layout: false,
            ..EpubNavigationExtension::default()
        },
    };
    let progress = TestProgressWriter::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    service
        .update_kobo_reading_state(
            "book-1",
            "user-1",
            KoboReadingStateUpdate {
                last_modified: "2026-03-27T10:00:00Z".to_string(),
                status: KoboReadingStateStatus::Reading,
                progress_percent: Some(99.0),
                content_source_progress_percent: Some(50.0),
                location_source: "/book-1.xhtml#frag".to_string(),
                kobo_span: Some("kobo.5.1".to_string()),
                device_id: "api-key-validkobotoken".to_string(),
                device_name: "kobo sync".to_string(),
            },
        )
        .await
        .expect("kobo reading state update should persist");

    let persisted = progress.persisted.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].book_id, "book-1");
    assert_eq!(persisted[0].user_id, "user-1");
    assert_eq!(persisted[0].page, 2);
    assert!(!persisted[0].completed);
    assert_eq!(
        persisted[0]
            .locator
            .as_ref()
            .and_then(|locator| locator.pointer("/locations/totalProgression")),
        Some(&json!(0.21))
    );
    assert_eq!(
        persisted[0].device_id.as_deref(),
        Some("api-key-validkobotoken")
    );
    assert_eq!(persisted[0].device_name.as_deref(), Some("kobo sync"));
}

#[tokio::test]
async fn kobo_reading_state_update_accepts_fixed_layout_single_position() {
    let device_sync = TestDeviceSync::default();
    let reader = NoopMediaReader {
        media_files: vec!["/fixed.xhtml".to_string()],
        epub_extension_blob: Some(EpubExtensionBlob {
            extension_class: "org.gotson.komga.domain.model.MediaExtensionEpub".to_string(),
            bytes: Vec::new(),
        }),
        page_count: Some(10),
        ..NoopMediaReader::default()
    };
    let content = NoopContentResolver {
        positions_extension: EpubNavigationExtension {
            positions: vec![epub_position(json!({
                "href": "fixed.xhtml",
                "type": "application/xhtml+xml",
                "locations": {
                    "progression": 0.0,
                    "totalProgression": 0.73,
                    "position": 4
                }
            }))],
            is_fixed_layout: true,
            ..EpubNavigationExtension::default()
        },
    };
    let progress = TestProgressWriter::default();
    let service = DeviceProgressService::new(&device_sync, &reader, &content, &progress);

    service
        .update_kobo_reading_state(
            "book-1",
            "user-1",
            KoboReadingStateUpdate {
                last_modified: "2026-03-27T10:00:00Z".to_string(),
                status: KoboReadingStateStatus::Reading,
                progress_percent: Some(90.0),
                content_source_progress_percent: Some(90.0),
                location_source: "/fixed.xhtml#frag".to_string(),
                kobo_span: Some("kobo.1.1".to_string()),
                device_id: "api-key-validkobotoken".to_string(),
                device_name: "kobo sync".to_string(),
            },
        )
        .await
        .expect("fixed-layout Kobo update should accept the only matching position");

    let persisted = progress.persisted.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].page, 7);
    assert!(!persisted[0].completed);
    assert_eq!(
        persisted[0]
            .locator
            .as_ref()
            .and_then(|locator| locator.pointer("/locations/totalProgression")),
        Some(&json!(0.73))
    );
}

#[derive(Default)]
struct TestDeviceSync {
    koreader_target: Option<KoreaderBookTarget>,
    read_progress: Option<PersistedReadProgressRecord>,
}

#[async_trait::async_trait]
impl DeviceSyncPort for TestDeviceSync {
    async fn load_book_created_timestamp(&self, _book_id: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn load_kobo_metadata_record(
        &self,
        _book_id: &str,
    ) -> anyhow::Result<Option<crate::identity_access::KoboMetadataRecord>> {
        Ok(None)
    }

    async fn load_koreader_book_target(
        &self,
        _book_hash: &str,
    ) -> Result<Option<KoreaderBookTarget>, KoreaderBookLookupError> {
        Ok(self.koreader_target.clone())
    }

    async fn load_read_progress(
        &self,
        _book_id: &str,
        _user_id: &str,
    ) -> anyhow::Result<Option<crate::identity_access::PersistedReadProgressRecord>> {
        Ok(self.read_progress.clone())
    }

    async fn load_thumbnail_by_id(
        &self,
        _thumbnail_id: &str,
    ) -> anyhow::Result<Option<DeviceThumbnailBinary>> {
        Ok(None)
    }

    async fn persisted_book_exists(&self, _book_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn save_book_projection_file_size(
        &self,
        _book_id: &str,
        _profile: &str,
        _file_size: u64,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct TestProgressWriter {
    persisted: Mutex<Vec<BookProgressionInput>>,
}

#[async_trait::async_trait]
impl BookProgressionWriterPort for TestProgressWriter {
    async fn persist_book_progression(&self, input: BookProgressionInput) -> anyhow::Result<()> {
        self.persisted.lock().unwrap().push(input);
        Ok(())
    }
}

#[derive(Default)]
struct NoopContentResolver {
    positions_extension: EpubNavigationExtension,
}

impl EpubNavigationContentPort for NoopContentResolver {
    fn decode_epub_navigation_extension(
        &self,
        _blob: &[u8],
    ) -> anyhow::Result<EpubNavigationExtension> {
        Ok(self.positions_extension.clone())
    }
}

fn epub_position(raw: Value) -> EpubNavigationPosition {
    EpubNavigationPosition::from_raw(raw)
}

#[derive(Default)]
struct NoopMediaReader {
    media_files: Vec<String>,
    book_media_files_error: Option<String>,
    epub_extension_blob: Option<EpubExtensionBlob>,
    book_progression: Option<BookProgressionRecord>,
    page_count: Option<u64>,
}

#[async_trait::async_trait]
impl EpubNavigationExtensionReaderPort for NoopMediaReader {
    async fn epub_extension_blob(
        &self,
        _book_id: &str,
    ) -> anyhow::Result<Option<EpubExtensionBlob>> {
        Ok(self.epub_extension_blob.clone())
    }
}

#[async_trait::async_trait]
impl EpubNavigationReaderPort for NoopMediaReader {
    async fn book_media_files(&self, _book_id: &str) -> anyhow::Result<Vec<String>> {
        if let Some(error) = &self.book_media_files_error {
            return Err(anyhow::anyhow!(error.clone()));
        }
        Ok(self.media_files.clone())
    }
}

#[async_trait::async_trait]
impl BookProgressionWriteReaderPort for NoopMediaReader {
    async fn book_progression(
        &self,
        _book_id: &str,
        _user_id: &str,
    ) -> anyhow::Result<Option<BookProgressionRecord>> {
        Ok(self.book_progression.clone())
    }
}

#[async_trait::async_trait]
impl DeviceProgressPageCountPort for NoopMediaReader {
    async fn book_page_count(&self, _book_id: &str) -> anyhow::Result<Option<u64>> {
        Ok(self.page_count)
    }
}
