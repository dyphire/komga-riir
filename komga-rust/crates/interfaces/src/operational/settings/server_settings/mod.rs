use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use komga_application::identity_access::user_is_admin;
use komga_application::operational::{
    PersistedServerSettings, ServerSettingPatch, ServerSettingsLoadError,
    ServerSettingsUpdateCommand, ServerSettingsUpdateError, ThumbnailSize,
};
use serde_json::Value;

use crate::contracts::common::MessageDto;
use crate::contracts::operational::{SettingMultiSourceDto, SettingsDto, ThumbnailSizeDto};
use crate::identity_access::auth::{Admin, Authenticated};
use crate::state::{RuntimeState, ServerSettingsState};

fn invalid_settings_payload(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json")],
        Json(MessageDto {
            message: message.to_string(),
        }),
    )
        .into_response()
}

pub(crate) async fn get_server_settings(
    State(app): State<ServerSettingsState>,
    Authenticated(current_user): Authenticated,
) -> Response {
    let settings = match app.server_settings.load().await {
        Ok(settings) => settings,
        Err(ServerSettingsLoadError::Load(error)) => return settings_load_error_response(error),
    };

    if !user_is_admin(&current_user) {
        // Non-admins only see upload limits (parity with Kotlin's public view).
        return Json(SettingsDto::public(super::super::MAX_UPLOAD_FILE_SIZE_BYTES))
            .into_response();
    }

    Json(settings_dto(&app.runtime, &settings)).into_response()
}

pub(crate) async fn update_server_settings(
    State(app): State<ServerSettingsState>,
    Admin(_admin): Admin,
    body: Bytes,
) -> Response {
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return invalid_settings_payload("invalid settings payload");
    };

    if !payload.is_object() {
        return invalid_settings_payload("invalid settings payload");
    }

    let command = match settings_update_command(&payload) {
        Ok(command) => command,
        Err(message) => return invalid_settings_payload(&message.to_string()),
    };

    match app.server_settings.update(command).await {
        Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(ServerSettingsUpdateError::InvalidPayload(message)) => {
            invalid_settings_payload(&message)
        }
        Err(ServerSettingsUpdateError::Load(error)) => settings_load_error_response(error),
        Err(ServerSettingsUpdateError::Persist(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(MessageDto {
                message: format!("failed to persist settings: {error:#}"),
            }),
        )
            .into_response(),
        Err(ServerSettingsUpdateError::ApplyTaskPool(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(MessageDto {
                message: format!("failed to process queued tasks: {error:#}"),
            }),
        )
            .into_response(),
    }
}

fn settings_load_error_response(error: impl std::fmt::Display + std::fmt::Debug) -> Response {
    tracing::error!(?error, "server settings load error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(MessageDto {
            message: format!("failed to load settings: {error:#}"),
        }),
    )
        .into_response()
}

fn settings_update_command(payload: &Value) -> anyhow::Result<ServerSettingsUpdateCommand> {
    let mut command = ServerSettingsUpdateCommand::default();

    if let Some(value) = payload.get("deleteEmptyCollections")
        && !value.is_null()
    {
        command.delete_empty_collections = Some(
            json_bool(value)
                .ok_or_else(|| anyhow::anyhow!("deleteEmptyCollections must be a boolean"))?,
        );
    }

    if let Some(value) = payload.get("deleteEmptyReadLists")
        && !value.is_null()
    {
        command.delete_empty_read_lists = Some(
            json_bool(value)
                .ok_or_else(|| anyhow::anyhow!("deleteEmptyReadLists must be a boolean"))?,
        );
    }

    if let Some(value) = payload.get("rememberMeDurationDays")
        && !value.is_null()
    {
        command.remember_me_duration_days =
            Some(json_u64(value).ok_or_else(|| {
                anyhow::anyhow!("rememberMeDurationDays must be a positive integer")
            })?);
    }

    if let Some(value) = payload.get("renewRememberMeKey")
        && !value.is_null()
    {
        command.renew_remember_me_key = Some(
            json_bool(value)
                .ok_or_else(|| anyhow::anyhow!("renewRememberMeKey must be a boolean"))?,
        );
    }

    if let Some(value) = payload.get("thumbnailSize")
        && !value.is_null()
    {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("thumbnailSize must be a string"))?;
        command.thumbnail_size = Some(
            ThumbnailSize::parse(value)
                .ok_or_else(|| anyhow::anyhow!("thumbnailSize is invalid"))?,
        );
    }

    if let Some(value) = payload.get("taskPoolSize")
        && !value.is_null()
    {
        command.task_pool_size = Some(
            json_u64(value)
                .ok_or_else(|| anyhow::anyhow!("taskPoolSize must be a positive integer"))?,
        );
    }

    command.server_port = optional_integer_patch(
        payload,
        "serverPort",
        "serverPort must be an integer between 1 and 65535",
    )?;
    command.server_context_path = optional_string_patch(
        payload,
        "serverContextPath",
        "serverContextPath must be a string or null",
    )?;

    if let Some(value) = payload.get("koboProxy")
        && !value.is_null()
    {
        command.kobo_proxy =
            Some(json_bool(value).ok_or_else(|| anyhow::anyhow!("koboProxy must be a boolean"))?);
    }

    command.kobo_port = optional_integer_patch(
        payload,
        "koboPort",
        "koboPort must be an integer between 1 and 65535",
    )?;

    command.kepubify_path = optional_string_patch(
        payload,
        "kepubifyPath",
        "kepubifyPath must be a string or null",
    )?;

    Ok(command)
}

fn optional_integer_patch(
    payload: &Value,
    field: &str,
    type_error: &str,
) -> anyhow::Result<ServerSettingPatch<u64>> {
    match payload.get(field) {
        Some(Value::Null) => Ok(ServerSettingPatch::Clear),
        Some(value) => json_u64(value)
            .map(ServerSettingPatch::Set)
            .ok_or_else(|| anyhow::anyhow!("{}", type_error)),
        None => Ok(ServerSettingPatch::Unchanged),
    }
}

/// Parse a JSON value as a non-negative integer, tolerating numeric strings.
///
/// The legacy webui serializes number inputs (`v-text-field type="number"`) as
/// JSON strings, which strict `serde_json::Value::as_u64` rejects. Kotlin's
/// Jackson backend coerces these automatically, so we do the same for parity.
fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(string) => string.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Parse a JSON value as a boolean, tolerating string representations.
fn json_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(boolean) => Some(*boolean),
        Value::String(string) => match string.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn optional_string_patch(
    payload: &Value,
    field: &str,
    type_error: &str,
) -> anyhow::Result<ServerSettingPatch<String>> {
    match payload.get(field) {
        Some(Value::Null) => Ok(ServerSettingPatch::Clear),
        Some(value) => value
            .as_str()
            .map(|value| ServerSettingPatch::Set(value.to_string()))
            .ok_or_else(|| anyhow::anyhow!("{}", type_error)),
        None => Ok(ServerSettingPatch::Unchanged),
    }
}

fn settings_dto(runtime: &RuntimeState, settings: &PersistedServerSettings) -> SettingsDto {
    SettingsDto {
        delete_empty_collections: Some(settings.delete_empty_collections),
        delete_empty_read_lists: Some(settings.delete_empty_read_lists),
        remember_me_duration_days: Some(settings.remember_me_duration_days),
        thumbnail_size: Some(thumbnail_size_dto(settings.thumbnail_size)),
        task_pool_size: Some(settings.task_pool_size),
        server_port: Some(SettingMultiSourceDto::new(
            Some(runtime.configuration_bind_address.port()),
            settings.server_port,
            Some(runtime.bind_address.port()),
        )),
        server_context_path: Some(SettingMultiSourceDto::new(
            runtime.configuration_server_context_path.clone(),
            settings.server_context_path.clone(),
            Some(runtime.server_context_path.clone().unwrap_or_default()),
        )),
        kobo_proxy: Some(settings.kobo_proxy),
        kobo_port: settings.kobo_port,
        kepubify_path: Some(SettingMultiSourceDto::new(
            None,
            settings.kepubify_path.clone(),
            None,
        )),
        max_upload_file_size_bytes: Some(super::super::MAX_UPLOAD_FILE_SIZE_BYTES),
    }
}

fn thumbnail_size_dto(size: ThumbnailSize) -> ThumbnailSizeDto {
    match size {
        ThumbnailSize::Default => ThumbnailSizeDto::Default,
        ThumbnailSize::Medium => ThumbnailSizeDto::Medium,
        ThumbnailSize::Large => ThumbnailSizeDto::Large,
        ThumbnailSize::XLarge => ThumbnailSizeDto::XLarge,
    }
}

#[cfg(test)]
mod tests;
