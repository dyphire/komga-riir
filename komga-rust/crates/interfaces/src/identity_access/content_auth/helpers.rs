use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use komga_application::identity_access::{
    AuthOutcome, AuthUser, AuthUserRole, PersistedAuthenticationActivity, random_uuid_like, user_id,
};
use serde_json::Value;

use crate::access_log::RequestConnectionInfo;
use crate::contracts::common::{
    MessageDto, PageDto, SpringErrorDto, ValidationErrorDto, ViolationDto,
};
use crate::contracts::identity_access::AuthenticationActivityDto;
use crate::discovery_auth::principal::principal_from_user;
use crate::identity_access::auth::{
    AuthenticationActivityApiKey, authentication_activity_headers_metadata_with_remote_addr,
    authentication_activity_write_input, persisted_api_key_metadata, persisted_api_key_user,
    persisted_basic_user, persisted_record_successful_authentication_activity, resolved_auth_user,
};
use crate::state::IdentityAccessState;

pub(super) fn register_discovery_principal(
    auth_state: &crate::discovery_auth::state::DiscoveryAuthState,
    user: &AuthUser,
    token: &str,
) {
    if let Some(principal) = principal_from_user(user) {
        auth_state.register_session_principal(token, principal);
    }
}

#[derive(Clone, Debug)]
pub(super) struct SharedLibrariesPatch {
    pub(super) all: bool,
    pub(super) library_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct AgeRestrictionPatch {
    pub(super) age: i64,
    pub(super) allow_only: bool,
}

pub(super) fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(MessageDto {
            message: message.to_string(),
        }),
    )
        .into_response()
}

pub(super) fn validation_error(field_name: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ValidationErrorDto {
            violations: vec![ViolationDto {
                field_name: Some(field_name.to_string()),
                message: Some(message.to_string()),
            }],
        }),
    )
        .into_response()
}

pub(super) fn spring_error(status: StatusCode, message: &str, path: &str) -> Response {
    let reason = status.canonical_reason().unwrap_or("Error");
    (
        status,
        Json(SpringErrorDto {
            error: reason.to_string(),
            message: message.to_string(),
            path: path.to_string(),
            status: status.as_u16(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }),
    )
        .into_response()
}

pub(super) fn generated_user_id() -> String {
    random_uuid_like()
}

pub(super) fn looks_like_kotlin_user_email(value: &str) -> bool {
    let mut parts = value.split('@');
    let Some(local) = parts.next() else {
        return false;
    };
    let Some(domain) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    if local.is_empty() || domain.is_empty() {
        return false;
    }

    let mut domain_segments = domain.split('.');
    let has_all_non_empty_segments = domain_segments.all(|segment| !segment.is_empty());
    has_all_non_empty_segments && domain.contains('.')
}

pub(super) fn parse_roles_array(value: Option<&Value>) -> Result<Vec<AuthUserRole>, &'static str> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }

    let Some(values) = value.as_array() else {
        return Err("roles must be an array of strings");
    };

    let mut roles = BTreeSet::new();
    for value in values {
        let Some(role) = value.as_str() else {
            return Err("roles must be an array of strings");
        };
        // Parity with Kotlin: UserRoles.valuesOf() silently drops names that
        // are not enum constants, so legacy webui payloads (e.g. "USER") must
        // not fail the request.
        let Some(role) = AuthUserRole::from_persisted_name(role) else {
            continue;
        };
        roles.insert(role);
    }
    Ok(roles.into_iter().collect())
}

pub(super) fn parse_string_set_optional(
    value: Option<&Value>,
) -> Result<Option<Vec<String>>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(Vec::new()));
    }

    let Some(values) = value.as_array() else {
        return Err("labels must be an array of strings");
    };

    let mut labels = BTreeSet::new();
    for value in values {
        let Some(label) = value.as_str() else {
            return Err("labels must be an array of strings");
        };
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        labels.insert(label.to_string());
    }

    Ok(Some(labels.into_iter().collect()))
}

pub(super) fn parse_age_restriction_optional(
    value: Option<&Value>,
) -> Result<Option<AgeRestrictionPatch>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }

    let Some(object) = value.as_object() else {
        return Err("ageRestriction must be an object");
    };

    let Some(age) = object.get("age").and_then(Value::as_i64) else {
        return Err("ageRestriction.age must be an integer");
    };
    if age < 0 {
        return Err("ageRestriction.age must be >= 0");
    }

    let Some(restriction) = object.get("restriction").and_then(Value::as_str) else {
        return Err("ageRestriction.restriction must be ALLOW_ONLY, EXCLUDE, or NONE");
    };

    match restriction {
        "ALLOW_ONLY" => Ok(Some(AgeRestrictionPatch {
            age,
            allow_only: true,
        })),
        "EXCLUDE" => Ok(Some(AgeRestrictionPatch {
            age,
            allow_only: false,
        })),
        "NONE" => Ok(None),
        _ => Err("ageRestriction.restriction must be ALLOW_ONLY, EXCLUDE, or NONE"),
    }
}

pub(super) fn parse_shared_libraries_patch(
    value: Option<&Value>,
) -> Result<SharedLibrariesPatch, &'static str> {
    let Some(value) = value else {
        return Err("sharedLibraries is required");
    };
    let Some(object) = value.as_object() else {
        return Err("sharedLibraries must be an object");
    };

    let Some(all) = object.get("all").and_then(Value::as_bool) else {
        return Err("sharedLibraries.all must be a boolean");
    };

    let library_ids = if all {
        Vec::new()
    } else {
        let Some(ids) = object.get("libraryIds").and_then(Value::as_array) else {
            return Err("sharedLibraries.libraryIds must be an array of strings");
        };

        let mut normalized = BTreeSet::new();
        for value in ids {
            let Some(library_id) = value.as_str() else {
                return Err("sharedLibraries.libraryIds must be an array of strings");
            };
            let library_id = library_id.trim();
            if library_id.is_empty() {
                continue;
            }
            normalized.insert(library_id.to_string());
        }
        normalized.into_iter().collect::<Vec<_>>()
    };

    Ok(SharedLibrariesPatch { all, library_ids })
}

pub(super) fn parse_shared_libraries_create(
    value: Option<&Value>,
) -> Result<SharedLibrariesPatch, &'static str> {
    let Some(value) = value else {
        return Ok(SharedLibrariesPatch {
            all: true,
            library_ids: Vec::new(),
        });
    };
    parse_shared_libraries_patch(Some(value))
}

pub(super) fn password_from_request(body: &Value) -> Option<&str> {
    body.get("password")?
        .as_str()
        .filter(|password| !password.trim().is_empty())
}

pub(super) fn api_key_comment_from_request(body: &Value) -> Option<String> {
    let comment = body.get("comment")?.as_str()?.trim();
    if comment.is_empty() {
        None
    } else {
        Some(comment.to_string())
    }
}

pub(super) async fn authenticated_user(
    headers: &HeaderMap,
    connection_info: RequestConnectionInfo,
    app: &IdentityAccessState,
) -> anyhow::Result<Option<AuthUser>> {
    let identity = &app.identity;
    let request_metadata = authentication_activity_headers_metadata_with_remote_addr(
        headers,
        connection_info.remote_addr(),
    );

    match persisted_api_key_user(identity, headers).await? {
        AuthOutcome::Valid(user) => {
            let api_key_metadata = persisted_api_key_metadata(identity, headers).await?;
            let _ = persisted_record_successful_authentication_activity(
                identity,
                &user,
                authentication_activity_write_input(
                    &request_metadata,
                    "ApiKey",
                    AuthenticationActivityApiKey::from_persisted(api_key_metadata.as_ref()),
                ),
            )
            .await;
            crate::access_log::record_resolved_auth_user_id(Some(user_id(&user)));
            return Ok(Some(*user));
        }
        AuthOutcome::Invalid => return Ok(None),
        AuthOutcome::Missing => {}
    }

    if let Some(user) = resolved_auth_user(identity, headers)? {
        return Ok(Some(user));
    }

    match persisted_basic_user(identity, headers).await? {
        AuthOutcome::Valid(user) => {
            let _ = persisted_record_successful_authentication_activity(
                identity,
                &user,
                authentication_activity_write_input(
                    &request_metadata,
                    "Password",
                    AuthenticationActivityApiKey::none(),
                ),
            )
            .await;
            crate::access_log::record_resolved_auth_user_id(Some(user_id(&user)));
            Ok(Some(*user))
        }
        AuthOutcome::Invalid | AuthOutcome::Missing => Ok(None),
    }
}

pub(super) async fn required_authenticated_user(
    headers: &HeaderMap,
    connection_info: RequestConnectionInfo,
    app: &IdentityAccessState,
) -> Result<AuthUser, StatusCode> {
    match authenticated_user(headers, connection_info, app).await {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Clone, Copy)]
enum AuthenticationActivitySortDirection {
    Asc,
    Desc,
}

struct AuthenticationActivitySortOrder<'a> {
    field: &'a str,
    direction: AuthenticationActivitySortDirection,
}

pub(super) fn authentication_activity_page_payload(
    mut rows: Vec<PersistedAuthenticationActivity>,
    query: &str,
) -> anyhow::Result<PageDto<AuthenticationActivityDto>> {
    let unpaged = query_bool(query, "unpaged");
    let page = query_value(query, "page")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let requested_size = query_value(query, "size")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20)
        .max(1);
    let sort_orders = authentication_activity_sort_orders(query);

    rows.sort_by(|left, right| compare_authentication_activity(left, right, &sort_orders));

    let total_elements = rows.len();
    let page_size = if unpaged {
        total_elements.max(20)
    } else {
        requested_size
    };
    let offset = if unpaged {
        0
    } else {
        page.saturating_mul(page_size)
    };
    let page_rows = if unpaged {
        rows
    } else if offset >= total_elements {
        vec![]
    } else {
        rows.into_iter()
            .skip(offset)
            .take(page_size)
            .collect::<Vec<_>>()
    };

    let content = page_rows
        .iter()
        .map(AuthenticationActivityDto::from_persisted)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let total_pages = if total_elements == 0 {
        0
    } else if unpaged {
        1
    } else {
        total_elements.div_ceil(page_size)
    };
    let page_number = if unpaged { 0 } else { page };

    Ok(PageDto::from_parts(
        content,
        page_number,
        page_size,
        total_elements,
        total_pages,
        !unpaged,
        true,
    ))
}

fn authentication_activity_sort_orders(query: &str) -> Vec<AuthenticationActivitySortOrder<'_>> {
    let sorts = crate::helpers::query_values(query, "sort");
    let mut orders = sorts
        .into_iter()
        .filter_map(|value| {
            let mut parts = value.split(',');
            let field = parts.next()?.trim();
            if field.is_empty() {
                return None;
            }
            let direction = match parts.next().map(str::trim) {
                Some(value) if value.eq_ignore_ascii_case("asc") => {
                    AuthenticationActivitySortDirection::Asc
                }
                _ => AuthenticationActivitySortDirection::Desc,
            };
            Some(AuthenticationActivitySortOrder { field, direction })
        })
        .collect::<Vec<_>>();
    if orders.is_empty() {
        orders.push(AuthenticationActivitySortOrder {
            field: "dateTime",
            direction: AuthenticationActivitySortDirection::Desc,
        });
    }
    orders
}

fn compare_authentication_activity(
    left: &PersistedAuthenticationActivity,
    right: &PersistedAuthenticationActivity,
    sort_orders: &[AuthenticationActivitySortOrder<'_>],
) -> Ordering {
    for sort in sort_orders {
        let ordering = match sort.field {
            "dateTime" => left.date_time().cmp(right.date_time()),
            "email" => left.email().cmp(&right.email()),
            "userId" => left.user_id().cmp(&right.user_id()),
            "ip" => left.ip().cmp(&right.ip()),
            "userAgent" => left.user_agent().cmp(&right.user_agent()),
            "success" => left.success().cmp(&right.success()),
            "error" => left.error().cmp(&right.error()),
            "source" => left.source().cmp(&right.source()),
            "apiKeyId" => left.api_key_id().cmp(&right.api_key_id()),
            "apiKeyComment" => left.api_key_comment().cmp(&right.api_key_comment()),
            _ => Ordering::Equal,
        };
        let ordering = match sort.direction {
            AuthenticationActivitySortDirection::Asc => ordering,
            AuthenticationActivitySortDirection::Desc => ordering.reverse(),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    Ordering::Equal
}

pub(super) fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let mut parts = pair.splitn(2, '=');
        let name = parts.next().unwrap_or_default();
        if name != key {
            return None;
        }
        Some(parts.next().unwrap_or_default())
    })
}

pub(super) fn query_bool(query: &str, key: &str) -> bool {
    query_value(query, key)
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_roles_array_ignores_unknown_roles_like_kotlin() {
        let roles = parse_roles_array(Some(&json!(["PAGE_STREAMING", "BROKEN", "USER"])))
            .expect("unknown roles should be ignored");
        assert_eq!(roles, vec![AuthUserRole::PageStreaming]);
    }

    #[test]
    fn parse_roles_array_rejects_non_string_entries() {
        let error = parse_roles_array(Some(&json!(["ADMIN", 42])))
            .expect_err("non-string entries should be rejected");
        assert_eq!(error, "roles must be an array of strings");
    }
}
