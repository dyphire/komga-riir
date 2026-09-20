use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use komga_domain::discovery::MediaStatus;
use tsid::create_tsid_256;

use super::{
    TransientBookAnalysis, TransientBookFileMetadata, TransientBookPage, TransientBookPageContent,
    TransientBookPort, TransientBookScanEntry, TransientBookSeriesInference,
};

#[derive(Clone, Debug, Default)]
pub struct TransientBooksStore {
    pub records: HashMap<String, TransientBookRecord>,
    last_access_epoch_seconds: HashMap<String, i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransientBookRecord {
    pub id: String,
    pub name: String,
    pub path: String,
    pub file_last_modified_unix_nanos: i128,
    pub size_bytes: u64,
    pub status: MediaStatus,
    pub media_type: String,
    pub page_count: u32,
    pub pages: Vec<TransientBookPage>,
    pub files: Vec<String>,
    pub comment: String,
    pub number: Option<f64>,
    pub series_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransientBookScanError {
    BadRequest(String),
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransientBookAnalyzeError {
    NotFound,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransientBookPageError {
    NotFound,
    AnalysisFailed,
    FileMissing,
    BadPageNumber,
    Internal,
}

#[derive(Clone)]
pub struct TransientBookService {
    port: Arc<dyn TransientBookPort>,
    store: Arc<Mutex<TransientBooksStore>>,
}

impl TransientBookService {
    pub fn new(port: Arc<dyn TransientBookPort>) -> Self {
        Self::with_store(port, TransientBooksStore::default())
    }

    pub fn with_store(port: Arc<dyn TransientBookPort>, store: TransientBooksStore) -> Self {
        Self {
            port,
            store: Arc::new(Mutex::new(store)),
        }
    }

    pub async fn scan(
        &self,
        requested_path: &str,
    ) -> Result<Vec<TransientBookRecord>, TransientBookScanError> {
        match self.port.validate_transient_scan_root(requested_path).await {
            Ok(()) => {}
            Err(error) => {
                let error_message = error.to_string();
                if matches!(error_message.as_str(), "ERR_1016" | "ERR_1017") {
                    return Err(TransientBookScanError::BadRequest(error_message));
                }
                return Err(TransientBookScanError::Internal);
            }
        }

        let mut records = self
            .port
            .list_transient_book_entries(Path::new(requested_path))
            .map_err(|_| TransientBookScanError::Internal)?
            .into_iter()
            .map(|entry| self.transient_book_record(entry))
            .collect::<Result<Vec<_>, _>>()?;
        records.sort_by(|left, right| left.path.cmp(&right.path));

        let mut store = self.lock_store();
        for record in &records {
            store.insert(record.clone());
        }

        Ok(records)
    }

    pub async fn analyze(
        &self,
        transient_book_id: &str,
    ) -> Result<TransientBookRecord, TransientBookAnalyzeError> {
        let record = self
            .lock_store()
            .get_cloned(transient_book_id)
            .ok_or(TransientBookAnalyzeError::NotFound)?;
        let analysis = self
            .port
            .analyze_transient_book(&record.path)
            .map_err(|_| TransientBookAnalyzeError::Internal)?;
        let series_inference = if analysis.status == MediaStatus::Ready {
            self.port
                .infer_transient_series_and_number(&record.path)
                .await
                .map_err(|_| TransientBookAnalyzeError::Internal)?
        } else {
            TransientBookSeriesInference {
                series_id: None,
                number: None,
            }
        };

        let mut store = self.lock_store();
        let entry = store
            .get_mut(transient_book_id)
            .ok_or(TransientBookAnalyzeError::NotFound)?;
        apply_transient_book_analysis(entry, analysis, series_inference);
        Ok(entry.clone())
    }

    /// Source-file last-modified time (unix nanos) for a transient book, used
    /// for HTTP conditional requests (`If-Modified-Since` / `Last-Modified`).
    pub fn file_last_modified_nanos(&self, transient_book_id: &str) -> Option<i128> {
        self.lock_store()
            .get_cloned(transient_book_id)
            .map(|record| record.file_last_modified_unix_nanos)
    }

    pub fn page_content(
        &self,
        transient_book_id: &str,
        page_number: i32,
    ) -> Result<TransientBookPageContent, TransientBookPageError> {
        if page_number <= 0 {
            return Err(TransientBookPageError::BadPageNumber);
        }
        let page_number = page_number as u32;

        let record = self
            .lock_store()
            .get_cloned(transient_book_id)
            .ok_or(TransientBookPageError::NotFound)?;
        if record.status != MediaStatus::Ready {
            return Err(TransientBookPageError::AnalysisFailed);
        }
        if !self
            .port
            .transient_book_exists(&record.path)
            .map_err(|_| TransientBookPageError::Internal)?
        {
            return Err(TransientBookPageError::FileMissing);
        }
        if record.media_type == "application/epub+zip" && record.pages.is_empty() {
            if record.page_count > 0 && page_number > record.page_count {
                return Err(TransientBookPageError::BadPageNumber);
            }
            return Err(TransientBookPageError::Internal);
        }

        self.port
            .transient_book_page_content(
                &record.path,
                &record.media_type,
                &record.pages,
                page_number,
            )
            .map_err(|_| TransientBookPageError::Internal)?
            .ok_or(TransientBookPageError::BadPageNumber)
    }

    fn transient_book_record(
        &self,
        entry: TransientBookScanEntry,
    ) -> Result<TransientBookRecord, TransientBookScanError> {
        let file_metadata = self
            .port
            .load_transient_book_file_metadata(&entry.path)
            .map_err(|_| TransientBookScanError::Internal)?;

        Ok(unknown_transient_book_record(entry, file_metadata))
    }

    fn lock_store(&self) -> MutexGuard<'_, TransientBooksStore> {
        self.store
            .lock()
            .expect("transient books state lock should not be poisoned")
    }
}

impl TransientBooksStore {
    pub fn with_records(records: HashMap<String, TransientBookRecord>) -> Self {
        let last_access_epoch_seconds = records
            .keys()
            .cloned()
            .map(|id| (id, current_unix_epoch_seconds()))
            .collect();
        Self {
            records,
            last_access_epoch_seconds,
        }
    }

    pub fn get_cloned(&mut self, id: &str) -> Option<TransientBookRecord> {
        self.prune_expired();
        self.touch(id)?;
        self.records.get(id).cloned()
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut TransientBookRecord> {
        self.prune_expired();
        self.touch(id)?;
        self.records.get_mut(id)
    }

    pub fn insert(&mut self, record: TransientBookRecord) {
        self.prune_expired();
        let id = record.id.clone();
        self.last_access_epoch_seconds
            .insert(id.clone(), current_unix_epoch_seconds());
        self.records.insert(id, record);
    }

    fn prune_expired(&mut self) {
        let now = current_unix_epoch_seconds();
        let expired_ids = self
            .last_access_epoch_seconds
            .iter()
            .filter(|(_, last_access)| {
                now.saturating_sub(**last_access)
                    >= TRANSIENT_BOOKS_EXPIRE_AFTER_ACCESS.as_secs() as i64
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();

        for id in expired_ids {
            self.last_access_epoch_seconds.remove(&id);
            self.records.remove(&id);
        }
    }

    fn touch(&mut self, id: &str) -> Option<()> {
        if !self.records.contains_key(id) {
            self.last_access_epoch_seconds.remove(id);
            return None;
        }

        self.last_access_epoch_seconds
            .insert(id.to_string(), current_unix_epoch_seconds());
        Some(())
    }
}

fn unknown_transient_book_record(
    entry: TransientBookScanEntry,
    file_metadata: TransientBookFileMetadata,
) -> TransientBookRecord {
    TransientBookRecord {
        id: transient_book_id(),
        name: entry.name,
        path: entry.path,
        file_last_modified_unix_nanos: file_metadata.file_last_modified_unix_nanos,
        size_bytes: file_metadata.size_bytes,
        status: MediaStatus::Unknown,
        media_type: String::new(),
        page_count: 0,
        pages: Vec::new(),
        files: Vec::new(),
        comment: String::new(),
        number: None,
        series_id: None,
    }
}

fn apply_transient_book_analysis(
    entry: &mut TransientBookRecord,
    analysis: TransientBookAnalysis,
    series_inference: TransientBookSeriesInference,
) {
    entry.status = analysis.status;
    entry.media_type = analysis.media_type;
    entry.page_count = analysis.page_count;
    entry.pages = analysis.pages;
    entry.files = analysis.files;
    entry.comment = analysis.comment;
    entry.number = analysis.number.or(series_inference.number);
    entry.series_id = analysis.series_id.or(series_inference.series_id);
}

fn transient_book_id() -> String {
    create_tsid_256().to_string()
}

const TRANSIENT_BOOKS_EXPIRE_AFTER_ACCESS: Duration = Duration::from_secs(60 * 60);

fn current_unix_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    use komga_domain::discovery::MediaStatus;

    use super::{
        TransientBookAnalyzeError, TransientBookPageContent, TransientBookPageError,
        TransientBookPort, TransientBookRecord, TransientBookScanEntry, TransientBookScanError,
        TransientBookService, TransientBooksStore, transient_book_id,
    };
    use crate::operational::{
        TransientBookAnalysis, TransientBookFileMetadata, TransientBookPage,
        TransientBookSeriesInference,
    };

    fn clone_result<T: Clone>(result: &Result<T, String>) -> anyhow::Result<T> {
        result.clone().map_err(anyhow::Error::msg)
    }

    #[test]
    fn transient_book_id_uses_kotlin_compatible_tsid_shape() {
        let id = transient_book_id();

        assert_eq!(id.len(), 13);
        assert!(matches!(id.chars().next(), Some('0'..='9' | 'A'..='F')));
        assert!(
            id.chars()
                .all(|ch| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(ch))
        );
    }

    #[test]
    fn page_content_accepts_typed_ready_status() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort::default()),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                TransientBookRecord {
                    id: "book-a".to_string(),
                    name: "Book".to_string(),
                    path: "/tmp/book.cbz".to_string(),
                    file_last_modified_unix_nanos: 0,
                    size_bytes: 1,
                    status: MediaStatus::Ready,
                    media_type: "application/zip".to_string(),
                    page_count: 1,
                    pages: vec![TransientBookPage {
                        number: 1,
                        file_name: "page.jpg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        width: None,
                        height: None,
                        size_bytes: None,
                    }],
                    files: Vec::new(),
                    comment: String::new(),
                    number: None,
                    series_id: None,
                },
            )])),
        );

        let content = service
            .page_content("book-a", 1)
            .expect("typed ready status should allow page delivery");

        assert_eq!(content.content_type, "image/jpeg");
        assert_eq!(content.bytes, b"page".to_vec());
    }

    #[tokio::test]
    async fn scan_returns_internal_when_listing_entries_fails() {
        let service = TransientBookService::new(Arc::new(TestTransientBookPort {
            scan_entries: Err("read transient directory failed".to_string()),
            ..TestTransientBookPort::default()
        }));

        let result = service.scan("/tmp/transient-books").await;

        assert_eq!(result, Err(TransientBookScanError::Internal));
    }

    #[tokio::test]
    async fn scan_returns_internal_when_file_metadata_fails() {
        let service = TransientBookService::new(Arc::new(TestTransientBookPort {
            scan_entries: Ok(vec![TransientBookScanEntry {
                path: "/tmp/transient-books/book.cbz".to_string(),
                name: "book".to_string(),
            }]),
            file_metadata: Err("read transient book metadata failed".to_string()),
            ..TestTransientBookPort::default()
        }));

        let result = service.scan("/tmp/transient-books").await;

        assert_eq!(result, Err(TransientBookScanError::Internal));
    }

    #[tokio::test]
    async fn analyze_returns_internal_when_series_inference_fails() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort {
                series_inference: Err("transient series lookup failed".to_string()),
                ..TestTransientBookPort::default()
            }),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                unknown_record("book-a"),
            )])),
        );

        let result = service.analyze("book-a").await;

        assert_eq!(result, Err(TransientBookAnalyzeError::Internal));
    }

    #[tokio::test]
    async fn analyze_returns_internal_when_media_analysis_fails() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort {
                analysis: Err("check transient book existence failed".to_string()),
                ..TestTransientBookPort::default()
            }),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                unknown_record("book-a"),
            )])),
        );

        let result = service.analyze("book-a").await;

        assert_eq!(result, Err(TransientBookAnalyzeError::Internal));
    }

    #[tokio::test]
    async fn analyze_keeps_failed_media_analysis_without_series_inference() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort {
                analysis: Ok(TransientBookAnalysis {
                    status: MediaStatus::Error,
                    media_type: "application/zip".to_string(),
                    page_count: 0,
                    pages: Vec::new(),
                    files: Vec::new(),
                    comment: "ERR_1006".to_string(),
                    number: None,
                    series_id: None,
                }),
                series_inference: Err("transient series lookup failed".to_string()),
                ..TestTransientBookPort::default()
            }),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                unknown_record("book-a"),
            )])),
        );

        let record = service
            .analyze("book-a")
            .await
            .expect("failed media analysis should be returned as an analysis result");

        assert_eq!(record.status, MediaStatus::Error);
        assert_eq!(record.comment, "ERR_1006");
        assert_eq!(record.series_id, None);
        assert_eq!(record.number, None);
    }

    #[test]
    fn page_content_returns_internal_when_existence_check_fails() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort {
                exists: Err("check transient book existence failed".to_string()),
                ..TestTransientBookPort::default()
            }),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                ready_record("book-a"),
            )])),
        );

        let result = service.page_content("book-a", 1);

        assert_eq!(result, Err(TransientBookPageError::Internal));
    }

    #[test]
    fn page_content_returns_internal_when_page_loader_fails() {
        let service = TransientBookService::with_store(
            Arc::new(TestTransientBookPort {
                page_content: Err("read transient page failed".to_string()),
                ..TestTransientBookPort::default()
            }),
            TransientBooksStore::with_records(HashMap::from([(
                "book-a".to_string(),
                ready_record("book-a"),
            )])),
        );

        let result = service.page_content("book-a", 1);

        assert_eq!(result, Err(TransientBookPageError::Internal));
    }

    #[derive(Clone)]
    struct TestTransientBookPort {
        analysis: Result<TransientBookAnalysis, String>,
        scan_entries: Result<Vec<TransientBookScanEntry>, String>,
        file_metadata: Result<TransientBookFileMetadata, String>,
        series_inference: Result<TransientBookSeriesInference, String>,
        exists: Result<bool, String>,
        page_content: Result<Option<TransientBookPageContent>, String>,
    }

    impl Default for TestTransientBookPort {
        fn default() -> Self {
            Self {
                analysis: Ok(TransientBookAnalysis {
                    status: MediaStatus::Ready,
                    media_type: "image/jpeg".to_string(),
                    page_count: 1,
                    pages: vec![TransientBookPage {
                        number: 1,
                        file_name: "page.jpg".to_string(),
                        media_type: "image/jpeg".to_string(),
                        width: None,
                        height: None,
                        size_bytes: None,
                    }],
                    files: Vec::new(),
                    comment: String::new(),
                    number: None,
                    series_id: None,
                }),
                scan_entries: Ok(Vec::new()),
                file_metadata: Ok(TransientBookFileMetadata {
                    file_last_modified_unix_nanos: 0,
                    size_bytes: 1,
                }),
                series_inference: Ok(TransientBookSeriesInference {
                    series_id: None,
                    number: None,
                }),
                exists: Ok(true),
                page_content: Ok(Some(TransientBookPageContent {
                    content_type: "image/jpeg".to_string(),
                    bytes: b"page".to_vec(),
                })),
            }
        }
    }

    #[async_trait::async_trait]
    impl TransientBookPort for TestTransientBookPort {
        fn analyze_transient_book(&self, _path: &str) -> anyhow::Result<TransientBookAnalysis> {
            clone_result(&self.analysis)
        }

        async fn infer_transient_series_and_number(
            &self,
            _transient_name: &str,
        ) -> anyhow::Result<TransientBookSeriesInference> {
            clone_result(&self.series_inference)
        }

        fn list_transient_book_entries(
            &self,
            _root: &Path,
        ) -> anyhow::Result<Vec<TransientBookScanEntry>> {
            clone_result(&self.scan_entries)
        }

        async fn validate_transient_scan_root(&self, _path: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn load_transient_book_file_metadata(
            &self,
            _path: &str,
        ) -> anyhow::Result<TransientBookFileMetadata> {
            clone_result(&self.file_metadata)
        }

        fn transient_book_exists(&self, _path: &str) -> anyhow::Result<bool> {
            clone_result(&self.exists)
        }

        fn transient_book_page_content(
            &self,
            _path: &str,
            _media_type: &str,
            _pages: &[TransientBookPage],
            _page_number: u32,
        ) -> anyhow::Result<Option<TransientBookPageContent>> {
            clone_result(&self.page_content)
        }
    }

    fn unknown_record(id: &str) -> TransientBookRecord {
        TransientBookRecord {
            id: id.to_string(),
            name: "Book".to_string(),
            path: "/tmp/book.cbz".to_string(),
            file_last_modified_unix_nanos: 0,
            size_bytes: 1,
            status: MediaStatus::Unknown,
            media_type: String::new(),
            page_count: 0,
            pages: Vec::new(),
            files: Vec::new(),
            comment: String::new(),
            number: None,
            series_id: None,
        }
    }

    fn ready_record(id: &str) -> TransientBookRecord {
        TransientBookRecord {
            status: MediaStatus::Ready,
            media_type: "image/jpeg".to_string(),
            page_count: 1,
            pages: vec![TransientBookPage {
                number: 1,
                file_name: "page.jpg".to_string(),
                media_type: "image/jpeg".to_string(),
                width: None,
                height: None,
                size_bytes: None,
            }],
            ..unknown_record(id)
        }
    }
}
