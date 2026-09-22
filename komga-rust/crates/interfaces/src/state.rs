mod app_state;
mod core;
mod discovery;
mod identity;
mod library_catalog;
mod media_assets;
mod opds;
mod operational;
mod runtime_events;
mod task_queue;

pub use crate::discovery_auth::state::DiscoveryAuthState;
pub use app_state::{
    HttpAppState, HttpServices, OperationalState, ReadProgressState, ShutdownTrigger,
    SseConnectionState, WebUiDirState,
};
pub use core::{
    AuthDatabaseState, OAuth2ClientConfig, OperationalBuildMetadata, RuntimeProfile, RuntimeState,
};
pub use discovery::DiscoveryState;
pub use identity::{AuthenticationActivityWriteInput, IdentityAccessState, IdentityState};
pub use library_catalog::LibraryCatalogState;
pub use media_assets::{MediaAssetsState, PersistedMediaFileRecord};
pub use opds::OpdsState;
pub use operational::{OperationalApiState, ServerSettingsState};
pub use runtime_events::RuntimeSseEventHub;
pub use task_queue::TaskQueueState;

#[cfg(test)]
pub(crate) mod tests;
