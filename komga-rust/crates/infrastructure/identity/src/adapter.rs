use std::sync::Arc;

use komga_application::identity_access::{
    AuthActivityPort, AuthOutcome, AuthUser, AuthenticationActivityApiKey, AuthenticationPort,
    CreateAuthUserInput, DeviceSyncPort, DeviceThumbnailBinary, KoboMetadataRecord, KoboProxyPort,
    KoboProxyRequest, KoboProxyResponse, KoboSyncBookState, KoboSyncPage, KoboSyncPageRequest,
    KoboSyncPointBook, KoboSyncStatePort, KoreaderBookLookupError, KoreaderBookTarget,
    PersistedApiKey, PersistedApiKeyMetadata, PersistedAuthenticationActivity,
    PersistedReadProgressRecord, ResolvedAuthToken, SessionLifecyclePort, SessionResolverPort,
    UpdateAuthUserInput, UpdateAuthUserResult, UserAdminPort,
    invalidate_remember_me_token as invalidate_remember_me_runtime_token,
    invalidate_session_token as invalidate_runtime_session_token,
    invalidate_user_sessions as invalidate_all_runtime_user_sessions, issue_remember_me_token,
    issue_session_token, kobo_metadata_pre_paginated, resolve_authenticated_token,
    resolve_authenticated_user,
};
use komga_application::media_assets::EpubNavigationContentPort;

use komga_infrastructure_base::DatabaseHandle;

use super::session_store::session_token_store;
use super::users::{authentication as auth_identity, mutation as user_mutation};
use super::{device_auth, kobo};

#[derive(Clone)]
pub struct IdentityAccess {
    db: DatabaseHandle,
    kobo_proxy_base_url: String,
    epub_navigation_content: Arc<dyn EpubNavigationContentPort>,
}

impl IdentityAccess {
    pub fn default_kobo_proxy_base_url() -> &'static str {
        kobo::DEFAULT_KOBO_PROXY_BASE_URL
    }

    pub fn new(
        db: DatabaseHandle,
        epub_navigation_content: Arc<dyn EpubNavigationContentPort>,
    ) -> Self {
        Self::with_kobo_proxy_base_url(
            db,
            Self::default_kobo_proxy_base_url(),
            epub_navigation_content,
        )
    }

    pub fn with_kobo_proxy_base_url(
        db: DatabaseHandle,
        kobo_proxy_base_url: impl Into<String>,
        epub_navigation_content: Arc<dyn EpubNavigationContentPort>,
    ) -> Self {
        Self {
            db,
            kobo_proxy_base_url: kobo_proxy_base_url.into(),
            epub_navigation_content,
        }
    }
}

impl SessionResolverPort for IdentityAccess {
    fn resolve_session_user(
        &self,
        session_token: Option<&str>,
        remember_me_token: Option<&str>,
    ) -> anyhow::Result<Option<AuthUser>> {
        resolve_authenticated_user(
            session_token_store(),
            session_token_store(),
            session_token,
            remember_me_token,
        )
    }

    fn resolve_auth_token(
        &self,
        session_token: Option<&str>,
        remember_me_token: Option<&str>,
    ) -> anyhow::Result<Option<ResolvedAuthToken>> {
        resolve_authenticated_token(
            session_token_store(),
            session_token_store(),
            session_token,
            remember_me_token,
        )
    }
}

#[async_trait::async_trait]
impl AuthenticationPort for IdentityAccess {
    async fn authenticate_basic(
        &self,
        username: &str,
        password: &str,
    ) -> anyhow::Result<AuthOutcome> {
        auth_identity::authenticate_basic_credentials(self.db.read_pool(), username, password).await
    }

    async fn authenticate_api_key(&self, api_key: &str) -> anyhow::Result<AuthOutcome> {
        auth_identity::persisted_api_key_user_by_token(api_key, self.db.read_pool()).await
    }

    async fn api_key_metadata_by_token(
        &self,
        api_key: &str,
    ) -> anyhow::Result<Option<PersistedApiKeyMetadata>> {
        auth_identity::persisted_api_key_metadata_by_token(api_key, self.db.read_pool()).await
    }
}

impl SessionLifecyclePort for IdentityAccess {
    fn session_token_for_user(&self, user: &AuthUser, runtime_key: &str) -> String {
        issue_session_token(session_token_store(), user, runtime_key)
    }

    fn remember_me_token_for_user(&self, user: &AuthUser, runtime_key: &str) -> Option<String> {
        issue_remember_me_token(session_token_store(), user, runtime_key)
    }

    fn sync_session_runtime_settings(&self, runtime_key: &str, max_inactive_seconds: u64) {
        session_token_store().sync_session_settings(runtime_key, max_inactive_seconds)
    }

    fn sync_remember_me_runtime_database_file(&self, runtime_key: &str) {
        session_token_store().sync_remember_me_database_path(runtime_key, self.db.database_file())
    }

    fn sync_remember_me_runtime_settings(&self, runtime_key: &str, key: &str, duration_days: u64) {
        session_token_store().sync_remember_me_settings(runtime_key, key, duration_days);
    }

    fn remember_me_max_age_seconds(&self, runtime_key: &str) -> u64 {
        session_token_store().remember_me_max_age_seconds(runtime_key)
    }

    fn invalidate_user_sessions(&self, user_id: &str) {
        invalidate_all_runtime_user_sessions(session_token_store(), user_id)
    }

    fn invalidate_user_sessions_with_runtime_key(&self, user_id: &str, runtime_key: &str) {
        session_token_store().invalidate_user_sessions_for_runtime_key(runtime_key, user_id)
    }

    fn invalidate_session_token(&self, token: &str) {
        invalidate_runtime_session_token(session_token_store(), token)
    }

    fn invalidate_remember_me_token(&self, token: &str) {
        invalidate_remember_me_runtime_token(session_token_store(), token)
    }

    fn store_oauth2_authorization_state(
        &self,
        runtime_key: &str,
        session_token: &str,
        registration_id: &str,
        state: &str,
    ) {
        session_token_store().store_oauth2_authorization_state(
            runtime_key,
            session_token,
            registration_id,
            state,
        )
    }

    fn take_oauth2_authorization_state(
        &self,
        runtime_key: &str,
        session_token: &str,
        registration_id: &str,
    ) -> Option<String> {
        session_token_store().take_oauth2_authorization_state(
            runtime_key,
            session_token,
            registration_id,
        )
    }
}

#[async_trait::async_trait]
impl UserAdminPort for IdentityAccess {
    async fn persisted_users(&self) -> anyhow::Result<Vec<AuthUser>> {
        auth_identity::persisted_users(self.db.read_pool()).await
    }

    async fn create_auth_user(
        &self,
        input: CreateAuthUserInput,
    ) -> anyhow::Result<Option<AuthUser>> {
        user_mutation::create_auth_user(self.db.write_pool(), input)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn delete_auth_user(&self, target_user_id: &str) -> anyhow::Result<bool> {
        user_mutation::delete_auth_user(self.db.write_pool(), target_user_id)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn update_auth_user(
        &self,
        target_user_id: &str,
        patch: UpdateAuthUserInput,
    ) -> anyhow::Result<UpdateAuthUserResult> {
        user_mutation::update_auth_user(self.db.write_pool(), target_user_id, patch)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn persisted_update_password_by_user_id(
        &self,
        user_id: &str,
        password: &str,
    ) -> anyhow::Result<bool> {
        auth_identity::persisted_update_password_by_user_id(self.db.write_pool(), user_id, password)
            .await
    }

    async fn ensure_oauth_user(
        &self,
        email: &str,
        allow_create: bool,
    ) -> anyhow::Result<Option<AuthUser>> {
        auth_identity::ensure_oauth_user(self.db.write_pool(), email, allow_create)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn persisted_create_api_key(
        &self,
        user_id: &str,
        comment: &str,
    ) -> anyhow::Result<PersistedApiKey> {
        auth_identity::persisted_create_api_key(self.db.write_pool(), user_id, comment).await
    }

    async fn persisted_api_key_comment_exists(
        &self,
        user_id: &str,
        comment: &str,
    ) -> anyhow::Result<bool> {
        auth_identity::persisted_api_key_comment_exists(self.db.read_pool(), user_id, comment).await
    }

    async fn persisted_list_api_keys(&self, user_id: &str) -> anyhow::Result<Vec<PersistedApiKey>> {
        auth_identity::persisted_list_api_keys(self.db.read_pool(), user_id).await
    }

    async fn persisted_delete_api_key_by_id(
        &self,
        user_id: &str,
        api_key_id: &str,
    ) -> anyhow::Result<bool> {
        auth_identity::persisted_delete_api_key_by_id(self.db.write_pool(), user_id, api_key_id)
            .await
    }
}

#[async_trait::async_trait]
impl AuthActivityPort for IdentityAccess {
    async fn persisted_list_authentication_activity(
        &self,
        user_id: Option<&str>,
    ) -> anyhow::Result<Vec<PersistedAuthenticationActivity>> {
        auth_identity::persisted_list_authentication_activity(self.db.read_pool(), user_id).await
    }

    async fn persisted_cleanup_authentication_activity(&self) -> anyhow::Result<u64> {
        auth_identity::persisted_cleanup_authentication_activity(self.db.write_pool()).await
    }

    async fn persisted_record_failed_authentication_activity(
        &self,
        email: Option<&str>,
        source: &str,
        error: &str,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Option<()> {
        auth_identity::persisted_record_failed_authentication_activity(
            self.db.write_pool(),
            email,
            source,
            error,
            ip,
            user_agent,
        )
        .await
    }

    async fn persisted_record_successful_authentication_activity(
        &self,
        user: &AuthUser,
        source: &str,
        api_key: AuthenticationActivityApiKey<'_>,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Option<()> {
        auth_identity::persisted_record_successful_authentication_activity(
            self.db.write_pool(),
            user,
            source,
            api_key,
            ip,
            user_agent,
        )
        .await
    }
}

#[async_trait::async_trait]
impl DeviceSyncPort for IdentityAccess {
    async fn load_book_created_timestamp(&self, book_id: &str) -> anyhow::Result<Option<String>> {
        device_auth::load_book_created_timestamp(self.db.read_pool(), book_id)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn load_kobo_metadata_record(
        &self,
        book_id: &str,
    ) -> anyhow::Result<Option<KoboMetadataRecord>> {
        let Some(record) = device_auth::load_kobo_metadata_record(self.db.read_pool(), book_id)
            .await
            .map_err(anyhow::Error::from)?
        else {
            return Ok(None);
        };

        Ok(Some(self.kobo_metadata_record(record)?))
    }

    async fn save_book_projection_file_size(
        &self,
        book_id: &str,
        profile: &str,
        file_size: u64,
    ) -> anyhow::Result<()> {
        device_auth::save_book_projection_file_size(
            self.db.write_pool(),
            book_id,
            profile,
            file_size,
        )
        .await
        .map_err(anyhow::Error::from)
    }

    async fn load_koreader_book_target(
        &self,
        book_hash: &str,
    ) -> Result<Option<KoreaderBookTarget>, KoreaderBookLookupError> {
        device_auth::load_koreader_book_target(self.db.read_pool(), book_hash).await
    }

    async fn load_read_progress(
        &self,
        book_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Option<PersistedReadProgressRecord>> {
        device_auth::load_read_progress(self.db.read_pool(), book_id, user_id)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn load_thumbnail_by_id(
        &self,
        thumbnail_id: &str,
    ) -> anyhow::Result<Option<DeviceThumbnailBinary>> {
        device_auth::load_thumbnail_by_id(self.db.read_pool(), thumbnail_id).await
    }

    async fn persisted_book_exists(&self, book_id: &str) -> anyhow::Result<bool> {
        device_auth::persisted_book_exists(self.db.read_pool(), book_id)
            .await
            .map_err(anyhow::Error::from)
    }
}

impl IdentityAccess {
    fn kobo_metadata_record(
        &self,
        record: device_auth::PersistedKoboMetadataRecord,
    ) -> anyhow::Result<KoboMetadataRecord> {
        Ok(KoboMetadataRecord {
            title: record.title,
            summary: record.summary,
            release_date: record.release_date,
            created_date: record.created_date,
            language: record.language,
            file_size: record.file_size,
            kepub_file_size: record.kepub_file_size,
            file_name: record.file_name,
            media_type: record.media_type,
            contributor_names: record.contributor_names,
            isbn: record.isbn,
            publisher_name: record.publisher_name,
            cover_image_id: record.cover_image_id,
            series_id: record.series_id,
            series_name: record.series_name,
            series_number: record.series_number,
            series_number_float: record.series_number_float,
            oneshot: record.oneshot,
            is_kepub: record.is_kepub,
            is_pre_paginated: kobo_metadata_pre_paginated(
                self.epub_navigation_content.as_ref(),
                record.epub_extension_blob.as_deref(),
            )?,
        })
    }
}

#[async_trait::async_trait]
impl KoboSyncStatePort for IdentityAccess {
    async fn load_sync_page(&self, request: KoboSyncPageRequest) -> anyhow::Result<KoboSyncPage> {
        kobo::load_kobo_sync_page(self.db.write_pool(), request)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn load_sync_book_states(
        &self,
        books: &[KoboSyncPointBook],
        user_id: &str,
    ) -> anyhow::Result<Vec<KoboSyncBookState>> {
        kobo::load_sync_book_states(self.db.read_pool(), books, user_id)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn remove_sync_point(&self, sync_point_id: &str) -> anyhow::Result<()> {
        kobo::remove_sync_point(self.db.write_pool(), sync_point_id)
            .await
            .map_err(anyhow::Error::from)
    }
}

#[async_trait::async_trait]
impl KoboProxyPort for IdentityAccess {
    async fn proxy_kobo_request(
        &self,
        request: KoboProxyRequest,
    ) -> anyhow::Result<KoboProxyResponse> {
        kobo::execute_kobo_proxy_request(self.kobo_proxy_base_url.as_str(), request).await
    }
}
