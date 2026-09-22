use std::path::Path;
use std::sync::Arc;

use komga_application::discovery::{
    AuthorFacetPort, BookDetailPort, BookSpecialListPort, CollectionSeriesPort,
    DiscoveryBrowseService, DiscoveryFacetService, LibraryIdMappingPort,
    PersistedBookIdResolverPort, PersistedSeriesIdResolverPort, PersistedSetService,
    PersistedSetVisibilityService, SeriesDetailPort, SeriesMetadataWritePort,
};
use komga_application::media_assets::{
    ArchiveBuilderPort, BookImportService, MetadataWriter, ReadProgressService,
};
use komga_application::operational::{
    ActuatorRuntimeMetadata, ActuatorSnapshotPort, HttpServerRequestsState, OperationalMetricsPort,
    PageHashService, RemoteFeedService, ServerSettingsService, StartupTimingState,
    TransientBookService,
};
use komga_application::runtime_sse::{RuntimeSseEventSink, RuntimeSseEventSource};
use komga_config::env_config::RuntimeConfig;
use komga_config::profile::RuntimeProfile as ConfigRuntimeProfile;
use komga_config::writer_ownership::WriterKind;
use komga_infrastructure_discovery::{
    DiscoveryDetailAccess, DiscoveryQuerySupportAccess, LibraryCatalogAccess,
    SqliteDiscoveryBrowseService,
};
use komga_infrastructure_identity::{ClaimAccess, IdentityAccess};
use komga_infrastructure_media_access::{
    FilesystemBookImport, MediaReader, ProgressWriter, SseBookEventEmitter, TransientBookAccess,
};
use komga_infrastructure_media_core::{ContentResolver, ZipArchiveBuilder};
use komga_infrastructure_media_metadata::{SqliteBookMetadataPort, ThumbnailWriter};
use komga_infrastructure_opds::{OpdsCatalogAccess, OpdsPersistedAccess};
use komga_infrastructure_operational::{
    ActuatorSnapshotAccess, AnnouncementAccess, ClientSettingsAccess, FilesystemBrowseAccess,
    FontAccess, HistoryAccess, OperationalMetricsAccess, PageHashAccess, RemoteFeedAccess,
    ServerSettingsStore, SyncpointAccess, load_remember_me_runtime_settings,
};
use komga_infrastructure_search::SearchSyncAdapter;
use komga_infrastructure_tasks::TaskEnqueueAdapter;
use komga_interfaces::state::{
    AuthDatabaseState, DiscoveryAuthState, HttpAppState, HttpServices, IdentityState,
    OAuth2ClientConfig, OperationalBuildMetadata, OperationalState, ReadProgressState,
    RuntimeProfile, RuntimeState, ShutdownTrigger, SseConnectionState, WebUiDirState,
};
use sha2::Digest;
use tokio::sync::watch;

use crate::build_metadata::current_build_metadata;
use crate::runtime::HttpRuntimeParts;

pub(super) fn compose_http_runtime(
    config: &RuntimeConfig,
    runtime: HttpRuntimeParts,
    shutdown_trigger: Option<watch::Sender<bool>>,
    startup_timing: StartupTimingState,
) -> HttpAppState {
    let HttpRuntimeParts {
        main_db: db,
        tasks_db,
        task_engine,
        runtime_events,
        contribution_cleanup,
    } = runtime;
    let runtime_event_source: Arc<dyn RuntimeSseEventSource> = runtime_events.clone();
    let runtime_event_sink: Arc<dyn RuntimeSseEventSink> = runtime_events;
    let content_resolver_access = Arc::new(ContentResolver);
    let epub_navigation_content: Arc<
        dyn komga_application::media_assets::EpubNavigationContentPort,
    > = content_resolver_access.clone();
    let identity = IdentityState::new(Arc::new(IdentityAccess::with_kobo_proxy_base_url(
        db.clone(),
        kobo_proxy_base_url(),
        epub_navigation_content.clone(),
    )));
    let operational_runtime_service: Arc<dyn OperationalMetricsPort> =
        Arc::new(OperationalMetricsAccess::new(db.clone(), tasks_db));
    let owns_search_index = config
        .writer_decision(WriterKind::SearchIndex)
        .allows_write();
    let discovery_detail_access = Arc::new(DiscoveryDetailAccess::new(
        db.clone(),
        config.lucene_data_directory.clone(),
        owns_search_index,
        runtime_event_sink.clone(),
    ));
    let book_detail: Arc<dyn BookDetailPort> = discovery_detail_access.clone();
    let series_detail: Arc<dyn SeriesDetailPort> = discovery_detail_access.clone();
    let series_metadata: Arc<dyn SeriesMetadataWritePort> = discovery_detail_access.clone();
    let series_access: Arc<dyn CollectionSeriesPort> = discovery_detail_access.clone();
    let book_id_resolver: Arc<dyn PersistedBookIdResolverPort> = discovery_detail_access.clone();
    let series_id_resolver: Arc<dyn PersistedSeriesIdResolverPort> =
        discovery_detail_access.clone();
    let manifest_metadata: Arc<dyn komga_application::media_assets::ManifestMetadataPort> =
        discovery_detail_access.clone();
    let persisted_set_visibility: Arc<dyn PersistedSetVisibilityService> =
        discovery_detail_access.clone();
    let persisted_sets: Arc<dyn PersistedSetService> = discovery_detail_access;
    let discovery_query_support = Arc::new(DiscoveryQuerySupportAccess::new(
        db.clone(),
        config.lucene_data_directory.clone(),
    ));
    let author_facets: Arc<dyn AuthorFacetPort> = discovery_query_support.clone();
    let library_id_mapping: Arc<dyn LibraryIdMappingPort> = discovery_query_support.clone();
    let book_special_lists: Arc<dyn BookSpecialListPort> = discovery_query_support;
    let discovery_browse_service = Arc::new(SqliteDiscoveryBrowseService::new(
        db.clone(),
        config.lucene_data_directory.clone(),
    ));
    let discovery_browse: Arc<dyn DiscoveryBrowseService> = discovery_browse_service.clone();
    let discovery_facets: Arc<dyn DiscoveryFacetService> = discovery_browse_service;
    let opds_catalog_access = Arc::new(OpdsCatalogAccess::new(db.clone()));
    let opds_feed_catalog: Arc<dyn komga_application::opds::OpdsFeedCatalogPort> =
        opds_catalog_access.clone();
    let opds_browse_catalog: Arc<dyn komga_application::opds::OpdsBrowseCatalogPort> =
        opds_catalog_access;
    let opds_persisted_access = Arc::new(OpdsPersistedAccess::new(
        db.clone(),
        config.lucene_data_directory.clone(),
    ));
    let opds_feed_persisted: Arc<dyn komga_application::opds::OpdsFeedPersistedPort> =
        opds_persisted_access.clone();
    let opds_library_persisted: Arc<dyn komga_application::opds::OpdsLibraryPersistedPort> =
        opds_persisted_access.clone();
    let opds_publisher_persisted: Arc<dyn komga_application::opds::OpdsPublisherPersistedPort> =
        opds_persisted_access.clone();
    let opds_collection_detail_persisted: Arc<
        dyn komga_application::opds::OpdsCollectionDetailPersistedPort,
    > = opds_persisted_access.clone();
    let opds_readlist_detail_persisted: Arc<
        dyn komga_application::opds::OpdsReadlistDetailPersistedPort,
    > = opds_persisted_access.clone();
    let opds_series_persisted: Arc<dyn komga_application::opds::OpdsSeriesPersistedPort> =
        opds_persisted_access.clone();
    let opds_search_persisted: Arc<dyn komga_application::opds::OpdsSearchPersistedPort> =
        opds_persisted_access;
    let remember_me_runtime_key = runtime_identity_key(config.database_file.as_path());
    identity
        .session_lifecycle()
        .sync_remember_me_runtime_database_file(remember_me_runtime_key.as_str());
    preload_remember_me_runtime_settings(config, remember_me_runtime_key.as_str(), &identity);
    // The current registry still derives both token families from the same configured root,
    // but the HTTP state keeps separate runtime keys so session and remember-me semantics are explicit.
    let session_runtime_key = remember_me_runtime_key.clone();
    identity.session_lifecycle().sync_session_runtime_settings(
        session_runtime_key.as_str(),
        config.session_max_inactive_seconds,
    );

    let read_progress = ReadProgressState::new();
    let profile = runtime_profile(config);
    let discovery_auth = DiscoveryAuthState::default();
    let auth_db = AuthDatabaseState {
        database_file: db.database_file().to_path_buf(),
        demo_mode: config.demo_mode,
        session_runtime_key,
        remember_me_runtime_key: remember_me_runtime_key.clone(),
    };
    let task_engine_arc: Arc<dyn komga_application::task_processing::TaskQueueAdmin> =
        Arc::from(task_engine);
    let metadata_writer = Arc::new(MetadataWriter::new(
        Box::new(SqliteBookMetadataPort::new(
            db.read_pool().clone(),
            db.write_pool().clone(),
        )),
        Box::new(SearchSyncAdapter::new(
            db.write_pool().clone(),
            config.lucene_data_directory.clone(),
            owns_search_index,
        )),
        Box::new(TaskEnqueueAdapter::new(task_engine_arc.clone())),
        Box::new(SseBookEventEmitter::new(runtime_event_sink.clone())),
    ));
    let server_settings = Arc::new(ServerSettingsStore::new(config.database_file.clone()));
    let page_hashes = Arc::new(PageHashAccess::new(db.clone()));
    let announcement_access = Arc::new(AnnouncementAccess::new(db.clone()));
    let remote_feeds = Arc::new(RemoteFeedService::new(
        Arc::new(RemoteFeedAccess::new(
            announcements_feed_url(),
            releases_feed_url(),
        )),
        announcement_access,
    ));
    let media_reader = Arc::new(MediaReader::new(db.read_pool().clone()));
    let book_media_reader: Arc<dyn komga_application::media_assets::BookMediaReaderPort> =
        media_reader.clone();
    let manifest_reader: Arc<dyn komga_application::media_assets::ManifestReaderPort> =
        media_reader.clone();
    let archive_reader: Arc<dyn komga_application::media_assets::ArchiveReaderPort> =
        media_reader.clone();
    let archive_builder: Arc<dyn ArchiveBuilderPort> = Arc::new(ZipArchiveBuilder);
    let thumbnail_reader: Arc<dyn komga_application::media_assets::ThumbnailReaderPort> =
        media_reader.clone();
    let epub_navigation_reader: Arc<dyn komga_application::media_assets::EpubNavigationReaderPort> =
        media_reader.clone();
    let book_progression_reader: Arc<
        dyn komga_application::media_assets::BookProgressionSurfacePort,
    > = media_reader.clone();
    let read_progress_reader: Arc<dyn komga_application::media_assets::ReadProgressReaderPort> =
        media_reader.clone();
    let series_relation: Arc<dyn komga_application::media_assets::SeriesRelationPort> =
        media_reader.clone();
    let device_progress_reader: Arc<
        dyn komga_application::identity_access::DeviceProgressReaderPort,
    > = media_reader.clone();
    let read_progress_surface: Arc<dyn komga_application::media_assets::ReadProgressSurfacePort> =
        media_reader;
    let progress_writer_access = Arc::new(ProgressWriter::new(
        db.write_pool().clone(),
        runtime_event_sink.clone(),
    ));
    let progress_writer: Arc<dyn komga_application::media_assets::ProgressWriterPort> =
        progress_writer_access.clone();
    let series_read_progress_writer: Arc<
        dyn komga_application::media_assets::SeriesReadProgressWriterPort,
    > = progress_writer_access;
    let manifest_content: Arc<dyn komga_application::media_assets::ManifestContentPort> =
        content_resolver_access.clone();
    let book_media_content: Arc<dyn komga_application::media_assets::BookMediaContentPort> =
        content_resolver_access.clone();
    let content_resolver: Arc<dyn komga_application::media_assets::ContentResolverPort> =
        content_resolver_access;
    let http_server_requests = HttpServerRequestsState::default();
    let build_metadata = current_build_metadata();
    let actuator_snapshots: Arc<dyn ActuatorSnapshotPort> = Arc::new(ActuatorSnapshotAccess::new(
        ActuatorRuntimeMetadata {
            main_db_file: db.database_file().to_path_buf(),
            tasks_db_file: config.tasks_db_file.clone(),
            config_dir: config.config_dir.clone(),
            build_version: build_metadata.version.clone(),
            build_time: build_metadata.build_time.clone(),
            git_branch: build_metadata.git_branch.clone(),
            git_commit_id: build_metadata.git_commit_id.clone(),
            git_commit_time: build_metadata.git_commit_time.clone(),
        },
        startup_timing.clone(),
        http_server_requests.clone(),
    ));
    let services = HttpServices {
        library_catalog: Arc::new(LibraryCatalogAccess::new(
            db.read_pool().clone(),
            db.write_pool().clone(),
            runtime_event_sink.clone(),
            contribution_cleanup,
        )),
        task_queue: task_engine_arc.clone(),
        server_settings: server_settings.clone(),
        server_settings_control: Arc::new(ServerSettingsService::new(
            server_settings,
            task_engine_arc.clone(),
        )),
        runtime_events: runtime_event_source,
        identity,
        operational_runtime: operational_runtime_service,
        actuator_snapshots,
        remote_feeds,
        claim: Arc::new(ClaimAccess::new(db.clone())),
        client_settings: Arc::new(ClientSettingsAccess::new(db.clone())),
        filesystem_browse: Arc::new(FilesystemBrowseAccess),
        fonts: Arc::new(FontAccess),
        history: Arc::new(HistoryAccess::new(db.clone())),
        page_hash_control: Arc::new(PageHashService::new(page_hashes, task_engine_arc)),
        syncpoints: Arc::new(SyncpointAccess::new(db.clone())),
        transient_books: Arc::new(TransientBookService::new(Arc::new(
            TransientBookAccess::new(db.clone()),
        ))),
        opds_feed_catalog,
        opds_browse_catalog,
        opds_feed_persisted,
        opds_library_persisted,
        opds_publisher_persisted,
        opds_collection_detail_persisted,
        opds_readlist_detail_persisted,
        opds_series_persisted,
        opds_search_persisted,
        author_facets,
        library_id_mapping,
        book_special_lists,
        persisted_sets,
        persisted_set_visibility,
        book_detail,
        series_detail,
        series_metadata,
        series_access,
        discovery_browse,
        discovery_facets,
        book_id_resolver,
        series_id_resolver,
        book_media_reader,
        manifest_reader,
        manifest_content,
        manifest_metadata,
        archive_reader,
        archive_builder,
        thumbnail_reader,
        epub_navigation_reader,
        book_progression_reader,
        read_progress_reader,
        series_relation,
        device_progress_reader,
        epub_navigation_content,
        book_media_content,
        content_resolver,
        thumbnail_writer: Arc::new(ThumbnailWriter::new(
            db.write_pool().clone(),
            runtime_event_sink.clone(),
        )),
        progress_writer: progress_writer.clone(),
        read_progress_service: Arc::new(ReadProgressService::new(
            read_progress_surface,
            series_read_progress_writer,
        )),
        metadata_writer,
        import_service: Arc::new(BookImportService::new(
            Arc::new(FilesystemBookImport::new(
                db.read_pool().clone(),
                db.write_pool().clone(),
            )),
            runtime_event_sink,
        )),
    };
    let operational = compose_operational_state(
        config,
        startup_timing,
        http_server_requests,
        build_metadata,
        remember_me_runtime_key,
        shutdown_trigger,
    );

    HttpAppState {
        profile,
        read_progress,
        discovery_auth,
        auth_db,
        operational,
        services,
    }
}

fn runtime_profile(config: &RuntimeConfig) -> RuntimeProfile {
    match config.runtime_profile {
        ConfigRuntimeProfile::SnapshotAligned => RuntimeProfile::SnapshotAligned,
        ConfigRuntimeProfile::LiveLocaldb => RuntimeProfile::LiveLocaldb,
    }
}

fn runtime_identity_key(database_file: &Path) -> String {
    let canonical = database_file
        .canonicalize()
        .unwrap_or_else(|_| database_file.to_path_buf());
    let digest = sha2::Sha256::digest(canonical.to_string_lossy().as_bytes());
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("auth-runtime-{}", &encoded[..16])
}

fn kobo_proxy_base_url() -> String {
    std::env::var("KOMGA_RUST_KOBO_PROXY_URL")
        .unwrap_or_else(|_| IdentityAccess::default_kobo_proxy_base_url().to_string())
}

fn announcements_feed_url() -> String {
    std::env::var("KOMGA_RUST_ANNOUNCEMENTS_URL")
        .unwrap_or_else(|_| RemoteFeedAccess::default_announcements_url().to_string())
}

fn releases_feed_url() -> String {
    std::env::var("KOMGA_RUST_RELEASES_URL")
        .unwrap_or_else(|_| RemoteFeedAccess::default_releases_url().to_string())
}

fn actuator_enabled() -> bool {
    !std::env::var("KOMGA_RUST_DISABLE_ACTUATOR")
        .map(|value| value.eq_ignore_ascii_case("true") || value == "1")
        .unwrap_or(false)
}

fn spring_profile_enabled(expected: &str) -> bool {
    std::env::var("SPRING_PROFILES_ACTIVE")
        .ok()
        .map(|profiles| {
            profiles
                .split(',')
                .map(str::trim)
                .any(|profile| profile.eq_ignore_ascii_case(expected))
        })
        .unwrap_or(false)
}

fn preload_remember_me_runtime_settings(
    config: &RuntimeConfig,
    remember_me_runtime_key: &str,
    identity: &IdentityState,
) {
    let remember_me_settings = load_remember_me_runtime_settings(config.database_file.as_path())
        .expect("remember-me startup settings should load");
    identity
        .session_lifecycle()
        .sync_remember_me_runtime_settings(
            remember_me_runtime_key,
            remember_me_settings.key.as_str(),
            remember_me_settings.duration_days,
        );
}

fn compose_operational_state(
    config: &RuntimeConfig,
    startup_timing: StartupTimingState,
    http_server_requests: HttpServerRequestsState,
    build_metadata: crate::build_metadata::BuildMetadata,
    remember_me_runtime_key: String,
    shutdown_trigger: Option<watch::Sender<bool>>,
) -> OperationalState {
    let sse = shutdown_trigger
        .as_ref()
        .map(|shutdown_tx| SseConnectionState::accepting_with_shutdown(shutdown_tx.clone()))
        .unwrap_or_default();

    OperationalState {
        runtime: RuntimeState {
            tasks_db_file: config.tasks_db_file.clone(),
            lucene_data_directory: config.lucene_data_directory.clone(),
            fonts_data_directory: config.fonts_data_directory.clone(),
            log_file: config.log_file.clone(),
            config_dir: config.config_dir.clone(),
            bind_address: config.bind_address,
            configuration_bind_address: config.configuration_bind_address,
            server_context_path: config.server_context_path.clone(),
            configuration_server_context_path: config.configuration_server_context_path.clone(),
            actuator_enabled: actuator_enabled(),
            dev_cors_enabled: spring_profile_enabled("dev"),
        },
        startup_timing,
        http_server_requests,
        remember_me_runtime_key,
        build_metadata: OperationalBuildMetadata {
            version: build_metadata.version,
            build_time: build_metadata.build_time,
            git_branch: build_metadata.git_branch,
            git_commit_id: build_metadata.git_commit_id,
            git_commit_time: build_metadata.git_commit_time,
        },
        oauth2_clients: oauth2_clients(config),
        oauth2_account_creation: config.oauth2_account_creation,
        oidc_email_verification: config.oidc_email_verification,
        webui_dir: WebUiDirState::new(crate::webui_updater::initial_dir(config)),
        sse,
        shutdown_trigger: shutdown_trigger.map(ShutdownTrigger::new),
    }
}

fn oauth2_clients(config: &RuntimeConfig) -> Vec<OAuth2ClientConfig> {
    config
        .oauth2_clients
        .iter()
        .map(|client| OAuth2ClientConfig {
            registration_id: client.registration_id.clone(),
            client_name: client.client_name.clone(),
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            authorization_uri: client.authorization_uri.clone(),
            token_uri: client.token_uri.clone(),
            user_info_uri: client.user_info_uri.clone(),
            issuer_uri: client.issuer_uri.clone(),
            jwk_set_uri: client.jwk_set_uri.clone(),
            redirect_uri: client.redirect_uri.clone(),
            client_authentication_method: client.client_authentication_method.clone(),
            scopes: client.scopes.clone(),
        })
        .collect()
}
