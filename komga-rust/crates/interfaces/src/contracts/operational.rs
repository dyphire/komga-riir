use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingMultiSourceDto<T> {
    pub configuration_source: Option<T>,
    pub database_source: Option<T>,
    pub effective_value: Option<T>,
}

impl<T> SettingMultiSourceDto<T> {
    pub fn new(
        configuration_source: Option<T>,
        database_source: Option<T>,
        effective_value: Option<T>,
    ) -> Self {
        Self {
            configuration_source,
            database_source,
            effective_value,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_empty_collections: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_empty_read_lists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember_me_duration_days: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_size: Option<ThumbnailSizeDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_pool_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_port: Option<SettingMultiSourceDto<u16>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_context_path: Option<SettingMultiSourceDto<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kobo_proxy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kobo_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kepubify_path: Option<SettingMultiSourceDto<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_upload_file_size_bytes: Option<u64>,
}

impl SettingsDto {
    /// Non-admin view of server settings: only upload limits are exposed
    /// (parity with the Kotlin backend's `SettingsDto.public()`).
    pub fn public(max_upload_file_size_bytes: u64) -> Self {
        Self {
            delete_empty_collections: None,
            delete_empty_read_lists: None,
            remember_me_duration_days: None,
            thumbnail_size: None,
            task_pool_size: None,
            server_port: None,
            server_context_path: None,
            kobo_proxy: None,
            kobo_port: None,
            kepubify_path: None,
            max_upload_file_size_bytes: Some(max_upload_file_size_bytes),
        }
    }
}

#[derive(Debug, Serialize)]
pub enum ThumbnailSizeDto {
    #[serde(rename = "DEFAULT")]
    Default,
    #[serde(rename = "MEDIUM")]
    Medium,
    #[serde(rename = "LARGE")]
    Large,
    #[serde(rename = "XLARGE")]
    XLarge,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuth2ClientDto {
    pub name: String,
    pub registration_id: String,
}
