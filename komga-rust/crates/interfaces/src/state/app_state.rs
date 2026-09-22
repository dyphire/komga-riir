use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use komga_application::discovery::{
    AuthorFacetPort, BookDetailPort, BookSpecialListPort, CollectionSeriesPort,
    DiscoveryBrowseService, DiscoveryFacetService, LibraryIdMappingPort,
    PersistedBookIdResolverPort, PersistedSeriesIdResolverPort, PersistedSetService,
    PersistedSetVisibilityService, SeriesDetailPort, SeriesMetadataWritePort,
};
use komga_application::operational::{HttpServerRequestsState, StartupTimingState};
use komga_application::runtime_sse::RuntimeSseEventSource;
use komga_application::task_processing::TaskQueueAdmin;
use tokio::sync::watch;

use super::core::{
    AuthDatabaseState, OAuth2ClientConfig, OperationalBuildMetadata, RuntimeProfile, RuntimeState,
};
use super::identity::IdentityState;
use crate::discovery_auth::state::DiscoveryAuthState;

#[derive(Clone)]
pub struct HttpServices {
    pub library_catalog: Arc<dyn komga_application::library_catalog::LibraryCatalogPort>,
    pub task_queue: Arc<dyn TaskQueueAdmin>,
    pub server_settings: Arc<dyn komga_application::operational::ServerSettingsPort>,
    pub server_settings_control: Arc<komga_application::operational::ServerSettingsService>,
    pub runtime_events: Arc<dyn RuntimeSseEventSource>,
    pub identity: IdentityState,
    pub operational_runtime: Arc<dyn komga_application::operational::OperationalMetricsPort>,
    pub actuator_snapshots: Arc<dyn komga_application::operational::ActuatorSnapshotPort>,
    pub remote_feeds: Arc<komga_application::operational::RemoteFeedService>,
    pub claim: Arc<dyn komga_application::operational::ClaimPort>,
    pub client_settings: Arc<dyn komga_application::operational::ClientSettingsPort>,
    pub filesystem_browse: Arc<dyn komga_application::operational::FilesystemBrowsePort>,
    pub fonts: Arc<dyn komga_application::operational::FontPort>,
    pub history: Arc<dyn komga_application::operational::HistoryPort>,
    pub page_hash_control: Arc<komga_application::operational::PageHashService>,
    pub syncpoints: Arc<dyn komga_application::operational::SyncpointPort>,
    pub transient_books: Arc<komga_application::operational::TransientBookService>,
    pub opds_feed_catalog: Arc<dyn komga_application::opds::OpdsFeedCatalogPort>,
    pub opds_browse_catalog: Arc<dyn komga_application::opds::OpdsBrowseCatalogPort>,
    pub opds_feed_persisted: Arc<dyn komga_application::opds::OpdsFeedPersistedPort>,
    pub opds_library_persisted: Arc<dyn komga_application::opds::OpdsLibraryPersistedPort>,
    pub opds_publisher_persisted: Arc<dyn komga_application::opds::OpdsPublisherPersistedPort>,
    pub opds_collection_detail_persisted:
        Arc<dyn komga_application::opds::OpdsCollectionDetailPersistedPort>,
    pub opds_readlist_detail_persisted:
        Arc<dyn komga_application::opds::OpdsReadlistDetailPersistedPort>,
    pub opds_series_persisted: Arc<dyn komga_application::opds::OpdsSeriesPersistedPort>,
    pub opds_search_persisted: Arc<dyn komga_application::opds::OpdsSearchPersistedPort>,
    pub author_facets: Arc<dyn AuthorFacetPort>,
    pub library_id_mapping: Arc<dyn LibraryIdMappingPort>,
    pub book_special_lists: Arc<dyn BookSpecialListPort>,
    pub persisted_sets: Arc<dyn PersistedSetService>,
    pub persisted_set_visibility: Arc<dyn PersistedSetVisibilityService>,
    pub book_detail: Arc<dyn BookDetailPort>,
    pub series_detail: Arc<dyn SeriesDetailPort>,
    pub series_metadata: Arc<dyn SeriesMetadataWritePort>,
    pub series_access: Arc<dyn CollectionSeriesPort>,
    pub discovery_browse: Arc<dyn DiscoveryBrowseService>,
    pub discovery_facets: Arc<dyn DiscoveryFacetService>,
    pub book_id_resolver: Arc<dyn PersistedBookIdResolverPort>,
    pub series_id_resolver: Arc<dyn PersistedSeriesIdResolverPort>,
    pub book_media_reader: Arc<dyn komga_application::media_assets::BookMediaReaderPort>,
    pub manifest_reader: Arc<dyn komga_application::media_assets::ManifestReaderPort>,
    pub manifest_content: Arc<dyn komga_application::media_assets::ManifestContentPort>,
    pub manifest_metadata: Arc<dyn komga_application::media_assets::ManifestMetadataPort>,
    pub archive_reader: Arc<dyn komga_application::media_assets::ArchiveReaderPort>,
    pub archive_builder: Arc<dyn komga_application::media_assets::ArchiveBuilderPort>,
    pub thumbnail_reader: Arc<dyn komga_application::media_assets::ThumbnailReaderPort>,
    pub epub_navigation_reader: Arc<dyn komga_application::media_assets::EpubNavigationReaderPort>,
    pub book_progression_reader:
        Arc<dyn komga_application::media_assets::BookProgressionSurfacePort>,
    pub read_progress_reader: Arc<dyn komga_application::media_assets::ReadProgressReaderPort>,
    pub series_relation: Arc<dyn komga_application::media_assets::SeriesRelationPort>,
    pub device_progress_reader:
        Arc<dyn komga_application::identity_access::DeviceProgressReaderPort>,
    pub epub_navigation_content:
        Arc<dyn komga_application::media_assets::EpubNavigationContentPort>,
    pub book_media_content: Arc<dyn komga_application::media_assets::BookMediaContentPort>,
    pub content_resolver: Arc<dyn komga_application::media_assets::ContentResolverPort>,
    pub thumbnail_writer: Arc<dyn komga_application::media_assets::ThumbnailWriterPort>,
    pub progress_writer: Arc<dyn komga_application::media_assets::ProgressWriterPort>,
    pub read_progress_service: Arc<komga_application::media_assets::ReadProgressService>,
    pub metadata_writer: Arc<komga_application::media_assets::MetadataWriter>,
    pub import_service: Arc<komga_application::media_assets::BookImportService>,
}

pub struct HttpAppState {
    pub profile: RuntimeProfile,
    pub read_progress: ReadProgressState,
    pub discovery_auth: DiscoveryAuthState,
    pub auth_db: AuthDatabaseState,
    pub operational: OperationalState,
    pub services: HttpServices,
}

#[derive(Clone)]
pub struct ShutdownTrigger {
    sender: watch::Sender<bool>,
}

impl ShutdownTrigger {
    pub fn new(sender: watch::Sender<bool>) -> Self {
        Self { sender }
    }

    pub fn request_shutdown(&self) {
        let _ = self.sender.send(true);
    }
}

/// The directory currently served as the web UI. Initially the configured webui.dir
/// (or the updater's managed copy under <config-dir>/webui), swapped at runtime when
/// the webui auto-updater installs a newer kmweb release. Reading is lock-free per
/// request; writes are rare (startup + update installs).
#[derive(Clone, Default)]
pub struct WebUiDirState(Arc<RwLock<Option<PathBuf>>>);

impl WebUiDirState {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self(Arc::new(RwLock::new(dir)))
    }

    pub fn get(&self) -> Option<PathBuf> {
        self.0.read().expect("webui dir lock poisoned").clone()
    }

    pub fn set(&self, dir: Option<PathBuf>) {
        *self.0.write().expect("webui dir lock poisoned") = dir;
    }
}

#[derive(Clone)]
pub struct OperationalState {
    pub runtime: RuntimeState,
    pub startup_timing: StartupTimingState,
    pub http_server_requests: HttpServerRequestsState,
    pub remember_me_runtime_key: String,
    pub build_metadata: OperationalBuildMetadata,
    pub oauth2_clients: Vec<OAuth2ClientConfig>,
    pub oauth2_account_creation: bool,
    pub oidc_email_verification: bool,
    pub webui_dir: WebUiDirState,
    pub sse: SseConnectionState,
    pub shutdown_trigger: Option<ShutdownTrigger>,
}

#[derive(Clone)]
pub struct SseConnectionState {
    accepting_connections: Arc<Mutex<bool>>,
    shutdown_tx: watch::Sender<bool>,
}

impl Default for SseConnectionState {
    fn default() -> Self {
        Self::accepting()
    }
}

impl SseConnectionState {
    pub fn accepting() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self::accepting_with_shutdown(shutdown_tx)
    }

    pub fn accepting_with_shutdown(shutdown_tx: watch::Sender<bool>) -> Self {
        Self {
            accepting_connections: Arc::new(Mutex::new(true)),
            shutdown_tx,
        }
    }

    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    pub fn is_accepting(&self) -> bool {
        *self
            .accepting_connections
            .lock()
            .expect("sse connection state lock should not be poisoned")
    }

    pub fn stop_accepting(&self) {
        *self
            .accepting_connections
            .lock()
            .expect("sse connection state lock should not be poisoned") = false;
    }
}

#[derive(Clone, Default)]
pub struct ReadProgressState {
    progress_by_token: Arc<Mutex<HashMap<String, HashMap<String, ReadProgress>>>>,
}

#[derive(Clone)]
struct ReadProgress;

impl ReadProgressState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, token: String, book_id: String) {
        let mut all_progress = self
            .progress_by_token
            .lock()
            .expect("read-progress state lock should not be poisoned");
        all_progress
            .entry(token)
            .or_default()
            .insert(book_id, ReadProgress);
    }

    pub fn remove(&self, token: &str, book_id: &str) {
        let mut all_progress = self
            .progress_by_token
            .lock()
            .expect("read-progress state lock should not be poisoned");
        if let Some(user_progress) = all_progress.get_mut(token) {
            user_progress.remove(book_id);
        }
    }
}
