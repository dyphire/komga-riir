use super::super::device_records::{
    KoboMetadataRecord, KoreaderBookLookupError, KoreaderBookTarget, PersistedReadProgressRecord,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceThumbnailBinary {
    pub book_id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[async_trait::async_trait]
pub trait DeviceSyncPort: Send + Sync {
    async fn load_book_created_timestamp(&self, book_id: &str) -> anyhow::Result<Option<String>>;

    async fn load_kobo_metadata_record(
        &self,
        book_id: &str,
    ) -> anyhow::Result<Option<KoboMetadataRecord>>;

    /// Upsert the file size of a converted book projection (e.g. kepub), so
    /// Kobo metadata can report the accurate download size for that profile.
    async fn save_book_projection_file_size(
        &self,
        book_id: &str,
        profile: &str,
        file_size: u64,
    ) -> anyhow::Result<()>;

    async fn load_koreader_book_target(
        &self,
        book_hash: &str,
    ) -> Result<Option<KoreaderBookTarget>, KoreaderBookLookupError>;

    async fn load_read_progress(
        &self,
        book_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Option<PersistedReadProgressRecord>>;

    async fn load_thumbnail_by_id(
        &self,
        thumbnail_id: &str,
    ) -> anyhow::Result<Option<DeviceThumbnailBinary>>;

    async fn persisted_book_exists(&self, book_id: &str) -> anyhow::Result<bool>;
}
