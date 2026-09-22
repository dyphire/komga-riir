use std::sync::Arc;

use axum::extract::FromRef;

use komga_application::operational::{
    ActuatorSnapshotPort, ClaimPort, ClientSettingsPort, FilesystemBrowsePort, FontPort,
    HistoryPort, OperationalMetricsPort, PageHashService, RemoteFeedService, ServerSettingsService,
    SyncpointPort, TransientBookService,
};
use komga_application::runtime_sse::RuntimeSseEventSource;

use super::app_state::{HttpAppState, OperationalState};
use super::core::RuntimeState;
use super::identity::IdentityState;
use super::task_queue::TaskQueueState;

#[derive(Clone)]
pub struct OperationalApiState {
    pub(crate) operational: OperationalState,
    pub(crate) identity: IdentityState,
    pub(crate) task_queue: TaskQueueState,
    pub(crate) runtime_events: Arc<dyn RuntimeSseEventSource>,
    pub(crate) operational_runtime: Arc<dyn OperationalMetricsPort>,
    pub(crate) actuator_snapshots: Arc<dyn ActuatorSnapshotPort>,
    pub(crate) remote_feeds: Arc<RemoteFeedService>,
    pub(crate) claim: Arc<dyn ClaimPort>,
    pub(crate) client_settings: Arc<dyn ClientSettingsPort>,
    pub(crate) filesystem_browse: Arc<dyn FilesystemBrowsePort>,
    pub(crate) fonts: Arc<dyn FontPort>,
    pub(crate) history: Arc<dyn HistoryPort>,
    pub(crate) page_hash_control: Arc<PageHashService>,
    pub(crate) syncpoints: Arc<dyn SyncpointPort>,
    pub(crate) transient_books: Arc<TransientBookService>,
    pub(crate) webui_dir: super::app_state::WebUiDirState,
}

impl FromRef<Arc<HttpAppState>> for OperationalApiState {
    fn from_ref(app: &Arc<HttpAppState>) -> Self {
        Self {
            operational: app.operational.clone(),
            identity: IdentityState::from_ref(app),
            task_queue: TaskQueueState::from_ref(app),
            runtime_events: app.services.runtime_events.clone(),
            operational_runtime: app.services.operational_runtime.clone(),
            actuator_snapshots: app.services.actuator_snapshots.clone(),
            remote_feeds: app.services.remote_feeds.clone(),
            claim: app.services.claim.clone(),
            client_settings: app.services.client_settings.clone(),
            filesystem_browse: app.services.filesystem_browse.clone(),
            fonts: app.services.fonts.clone(),
            history: app.services.history.clone(),
            page_hash_control: app.services.page_hash_control.clone(),
            syncpoints: app.services.syncpoints.clone(),
            transient_books: app.services.transient_books.clone(),
            webui_dir: app.operational.webui_dir.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ServerSettingsState {
    pub runtime: RuntimeState,
    pub(crate) server_settings: Arc<ServerSettingsService>,
}

impl FromRef<Arc<HttpAppState>> for ServerSettingsState {
    fn from_ref(app: &Arc<HttpAppState>) -> Self {
        Self {
            runtime: app.operational.runtime.clone(),
            server_settings: app.services.server_settings_control.clone(),
        }
    }
}
