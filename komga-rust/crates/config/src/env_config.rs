use std::collections::BTreeMap;
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::cli_args::{ADDR_ENV, DEFAULT_BIND_ADDRESS, SERVER_CONTEXT_PATH_ENV, SERVER_PORT_ENV};
use super::error::ConfigError;
use super::path_resolution::{
    default_log_file_for_config_dir, is_valid_startup_context_path, preferred_string,
    resolve_admin_action_config_with_env, resolve_runtime_config_with_env,
};
use super::profile::{DEFAULT_CONFIG_DIR, PlatformProfile, RuntimeMode, RuntimeProfile};
use super::startup_policy::{
    ensure_startup_runtime_layout, validate_single_writer_storage_ownership,
};
use super::writer_ownership::WriterOwnershipPolicy;

pub const DEFAULT_SESSION_MAX_INACTIVE_SECONDS: u64 = 30 * 24 * 60 * 60;
pub const DEFAULT_SORT_LOCALE: Option<String> = None;
pub const DEFAULT_WEBUI_UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuth2ClientConfig {
    pub registration_id: String,
    pub client_name: String,
    pub client_id: String,
    pub client_secret: String,
    pub authorization_uri: Option<String>,
    pub token_uri: Option<String>,
    pub user_info_uri: Option<String>,
    pub issuer_uri: Option<String>,
    pub jwk_set_uri: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_authentication_method: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub bind_address: SocketAddr,
    pub configuration_bind_address: SocketAddr,
    pub mode: RuntimeMode,
    pub demo_mode: bool,
    pub oauth2_account_creation: bool,
    pub oidc_email_verification: bool,
    pub runtime_profile: RuntimeProfile,
    pub platform_profile: PlatformProfile,
    pub config_dir: Option<PathBuf>,
    pub server_context_path: Option<String>,
    pub configuration_server_context_path: Option<String>,
    pub database_server_port: Option<u16>,
    pub database_server_context_path: Option<String>,
    pub log_file: PathBuf,
    pub database_file: PathBuf,
    pub riir_db_file: PathBuf,
    pub tasks_db_file: PathBuf,
    pub lucene_data_directory: PathBuf,
    pub fonts_data_directory: PathBuf,
    pub oauth2_clients: Vec<OAuth2ClientConfig>,
    pub writer_ownership_policy: WriterOwnershipPolicy,
    pub session_max_inactive_seconds: u64,
    pub task_pool_size: usize,
    pub sort_locale: Option<String>,
    pub webui_dir: Option<PathBuf>,
    pub webui_auto_update: bool,
    pub webui_update_interval: Duration,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeDatabaseSettings {
    pub server_port: Option<u16>,
    pub server_context_path: Option<String>,
    pub task_pool_size: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminActionConfig {
    pub(crate) database_file: PathBuf,
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let cli = super::cli_args::RuntimeCli::default();
        let env = env::vars().collect::<BTreeMap<_, _>>();
        let config = Self::resolve_with_env(&cli, &env)?;
        ensure_startup_runtime_layout(&config)?;
        Ok(config)
    }

    pub fn resolve_with_env(
        cli: &super::cli_args::RuntimeCli,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        resolve_runtime_config_with_env(cli, env)
    }

    pub fn resolve_with_env_and_database(
        cli: &super::cli_args::RuntimeCli,
        env: &BTreeMap<String, String>,
        database_settings: RuntimeDatabaseSettings,
    ) -> Result<Self, ConfigError> {
        let mut config = resolve_runtime_config_with_env(cli, env)?;
        config.database_server_port = database_settings.server_port;
        config.database_server_context_path = database_settings.server_context_path.clone();
        config.task_pool_size = database_settings.task_pool_size.unwrap_or(1).max(1);

        if !has_explicit_bind_address(cli, env)
            && !has_explicit_server_port(env)
            && let Some(server_port) = database_settings.server_port
        {
            config.bind_address.set_port(server_port);
        }

        if !has_explicit_server_context_path(env)
            && let Some(server_context_path) = database_settings.server_context_path
            && is_valid_startup_context_path(server_context_path.as_str())
        {
            config.server_context_path = Some(server_context_path);
        }

        config.validate_single_writer_storage_ownership(env)?;

        Ok(config)
    }

    pub fn for_runtime_profile(runtime_profile: RuntimeProfile) -> Self {
        let config_dir = PathBuf::from(DEFAULT_CONFIG_DIR);
        let bind_address = DEFAULT_BIND_ADDRESS
            .parse()
            .expect("default bind address should parse");
        let server_context_path = Some(String::new());
        let database_file = config_dir.join("database.sqlite");
        Self {
            bind_address,
            configuration_bind_address: bind_address,
            mode: match runtime_profile {
                RuntimeProfile::SnapshotAligned => RuntimeMode::Snapshot,
                RuntimeProfile::LiveLocaldb => RuntimeMode::Localdb,
            },
            demo_mode: false,
            oauth2_account_creation: false,
            oidc_email_verification: true,
            runtime_profile,
            platform_profile: PlatformProfile::Default,
            config_dir: Some(config_dir.clone()),
            server_context_path: server_context_path.clone(),
            configuration_server_context_path: server_context_path,
            database_server_port: None,
            database_server_context_path: None,
            log_file: default_log_file_for_config_dir(&config_dir),
            riir_db_file: super::path_resolution::riir_db_file_for(&database_file),
            database_file,
            tasks_db_file: config_dir.join("tasks.sqlite"),
            lucene_data_directory: config_dir.join("lucene"),
            fonts_data_directory: config_dir.join("fonts"),
            oauth2_clients: vec![],
            writer_ownership_policy: WriterOwnershipPolicy {
                isolation_root: None,
                allow_isolated_writes: false,
            },
            session_max_inactive_seconds: DEFAULT_SESSION_MAX_INACTIVE_SECONDS,
            task_pool_size: 1,
            sort_locale: DEFAULT_SORT_LOCALE,
            webui_dir: None,
            webui_auto_update: false,
            webui_update_interval: DEFAULT_WEBUI_UPDATE_INTERVAL,
        }
    }

    pub(crate) fn validate_single_writer_storage_ownership(
        &self,
        env: &BTreeMap<String, String>,
    ) -> Result<(), ConfigError> {
        validate_single_writer_storage_ownership(self, env)
    }
}

fn has_explicit_bind_address(
    cli: &super::cli_args::RuntimeCli,
    env: &BTreeMap<String, String>,
) -> bool {
    preferred_string(
        cli.address.as_deref(),
        env.get(ADDR_ENV).map(String::as_str),
    )
    .is_some()
}

fn has_explicit_server_port(env: &BTreeMap<String, String>) -> bool {
    preferred_string(None, env.get(SERVER_PORT_ENV).map(String::as_str)).is_some()
}

fn has_explicit_server_context_path(env: &BTreeMap<String, String>) -> bool {
    preferred_string(None, env.get(SERVER_CONTEXT_PATH_ENV).map(String::as_str)).is_some()
}

impl AdminActionConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let cli = super::cli_args::RuntimeCli::default();
        let env = env::vars().collect::<BTreeMap<_, _>>();
        Self::resolve_with_env(&cli, &env)
    }

    pub fn resolve_with_env(
        cli: &super::cli_args::RuntimeCli,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        resolve_admin_action_config_with_env(cli, env)
    }

    pub fn database_file(&self) -> &Path {
        self.database_file.as_path()
    }
}
