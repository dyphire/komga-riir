use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, Method, Uri};
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get, patch, post, put};
use komga_application::operational::HttpServerRequestsState;
use std::sync::Arc;
use tower_http::csrf::CsrfLayer;
use tower_http::trace::TraceLayer;

use crate::access_log;
use crate::cache;
use crate::{discovery, identity_access, library_catalog, media_assets, opds, operational};

use crate::identity_access::device_auth;
use crate::state::HttpAppState;

pub fn build_router(app: HttpAppState) -> Router {
    let runtime_context_path =
        mounted_runtime_context_path(app.operational.runtime.server_context_path.as_deref());
    let dev_cors_enabled = app.operational.runtime.dev_cors_enabled;
    let actuator_enabled = app.operational.runtime.actuator_enabled;
    let app = Arc::new(app);
    let router = Router::new()
        .route(
            "/api/v1/settings",
            get(operational::get_server_settings).patch(operational::update_server_settings),
        )
        .route(
            "/api/v1/announcements",
            get(operational::get_announcements).put(operational::put_announcements),
        )
        .route("/api/v1/releases", get(operational::get_releases))
        .route("/api/v1/filesystem", post(operational::post_filesystem))
        .route(
            "/api/v1/fonts/families",
            get(operational::get_fonts_families),
        )
        .route(
            "/api/v1/fonts/resource/{font_family}/{font_file}",
            get(operational::get_font_file),
        )
        .route(
            "/api/v1/fonts/resource/{font_family}/css",
            get(operational::get_font_family_css),
        )
        .route("/api/v1/history", get(operational::get_history))
        .route(
            "/api/v1/page-hashes",
            get(operational::get_page_hashes).put(operational::put_page_hash),
        )
        .route(
            "/api/v1/page-hashes/unknown",
            get(operational::get_page_hashes_unknown),
        )
        .route(
            "/api/v1/page-hashes/unknown/{page_hash}/thumbnail",
            get(operational::get_page_hash_unknown_thumbnail),
        )
        .route(
            "/api/v1/page-hashes/{page_hash}",
            get(operational::get_page_hash_matches),
        )
        .route(
            "/api/v1/page-hashes/{page_hash}/delete-all",
            post(operational::post_page_hash_delete_all),
        )
        .route(
            "/api/v1/page-hashes/{page_hash}/delete-match",
            post(operational::post_page_hash_delete_match),
        )
        .route(
            "/api/v1/page-hashes/{page_hash}/thumbnail",
            get(operational::get_page_hash_thumbnail),
        )
        .route(
            "/api/v1/transient-books",
            post(operational::post_transient_books),
        )
        .route(
            "/api/v1/transient-books/{transient_book_id}/analyze",
            post(operational::post_transient_book_analyze),
        )
        .route(
            "/api/v1/transient-books/{transient_book_id}/pages/{page_number}",
            get(operational::get_transient_book_page),
        )
        .route(
            "/api/v1/claim",
            get(operational::get_claim_status).post(operational::post_claim),
        )
        .route(
            "/api/v1/syncpoints/me",
            delete(operational::delete_syncpoints_me),
        )
        .route(
            "/api/v1/client-settings/global/list",
            get(operational::get_client_settings_global),
        )
        .route(
            "/api/v1/client-settings/global",
            patch(operational::patch_client_settings_global)
                .delete(operational::delete_client_settings_global),
        )
        .route(
            "/api/v1/client-settings/user/list",
            get(operational::get_client_settings_user),
        )
        .route(
            "/api/v1/client-settings/user",
            patch(operational::patch_client_settings_user)
                .delete(operational::delete_client_settings_user),
        )
        .route(
            "/api/v1/oauth2/providers",
            get(operational::get_oauth2_providers),
        )
        .route(
            "/oauth2/authorization/{registration_id}",
            get(device_auth::oauth2_authorization),
        )
        .route(
            "/login/oauth2/code/{registration_id}",
            get(device_auth::oauth2_login_code),
        )
        .route("/kobo/{auth_token}/ping", get(device_auth::kobo_ping))
        .route(
            "/kobo/{auth_token}/v1/initialization",
            get(device_auth::kobo_initialization),
        )
        .route(
            "/kobo/{auth_token}/v1/auth/device",
            post(device_auth::kobo_auth_device),
        )
        .route(
            "/kobo/{auth_token}/v1/library/sync",
            get(device_auth::kobo_library_sync),
        )
        .route(
            "/kobo/{auth_token}/v1/library/{book_id}/metadata",
            get(device_auth::kobo_library_book_metadata),
        )
        .route(
            "/kobo/{auth_token}/v1/library/{book_id}/state",
            get(device_auth::kobo_library_book_state).put(device_auth::kobo_library_book_state_update),
        )
        .route(
            "/kobo/{auth_token}/v1/books/{book_id}/file/epub",
            get(device_auth::kobo_book_file_epub),
        )
        .route(
            "/kobo/{auth_token}/v1/books/{thumbnail_id}/thumbnail/{width}/{height}/{is_greyscale}/image.jpg",
            get(device_auth::kobo_book_thumbnail),
        )
        .route(
            "/kobo/{auth_token}/v1/books/{thumbnail_id}/thumbnail/{width}/{height}/{quality}/{is_greyscale}/image.jpg",
            get(device_auth::kobo_book_thumbnail_with_quality),
        )
        .route(
            "/kobo/{auth_token}/{*path}",
            get(device_auth::kobo_catch_all)
                .put(device_auth::kobo_catch_all)
                .post(device_auth::kobo_catch_all)
                .patch(device_auth::kobo_catch_all)
                .delete(device_auth::kobo_catch_all),
        )
        .route(
            "/koreader/users/create",
            post(device_auth::koreader_user_create),
        )
        .route("/koreader/users/auth", get(device_auth::koreader_user_auth))
        .route(
            "/koreader/syncs/progress/{book_hash}",
            get(device_auth::koreader_get_progress),
        )
        .route(
            "/koreader/syncs/progress",
            put(device_auth::koreader_put_progress),
        )
        .route("/api/v1/tasks", delete(operational::delete_tasks))
        .route(
            "/api/v1/libraries",
            get(library_catalog::handlers::libraries_route)
                .post(library_catalog::handlers::library_create_route),
        )
        .route(
            "/api/v1/libraries/{library_id}",
            get(library_catalog::handlers::library_detail_route)
                // Deprecated since 1.3.0: use PATCH /api/v1/libraries/{library_id} instead.
                .put(library_catalog::handlers::library_update_route)
                .patch(library_catalog::handlers::library_update_route)
                .delete(library_catalog::handlers::library_delete_route),
        )
        .route(
            "/api/v1/libraries/{library_id}/scan",
            post(library_catalog::handlers::library_scan_route),
        )
        .route(
            "/api/v1/libraries/{library_id}/analyze",
            post(library_catalog::handlers::library_analyze_route),
        )
        .route(
            "/api/v1/libraries/{library_id}/metadata/refresh",
            post(library_catalog::handlers::library_metadata_refresh_route),
        )
        .route(
            "/api/v1/libraries/{library_id}/empty-trash",
            post(library_catalog::handlers::library_empty_trash_route),
        )
        // Deprecated since 1.20.0: use GET /api/v2/authors instead.
        .route("/api/v1/authors", get(discovery::facets::authors_deprecated_get))
        // Deprecated since 1.26.0: use GET /api/v2/authors/names instead.
        .route("/api/v1/authors/names", get(discovery::facets::authors_names))
        // Deprecated since 1.26.0: use GET /api/v2/authors/roles instead.
        .route("/api/v1/authors/roles", get(discovery::facets::authors_roles))
        // Deprecated since 1.26.0: use GET /api/v2/genres instead.
        .route("/api/v1/genres", get(discovery::facets::genres))
        // Deprecated since 1.26.0: use GET /api/v2/tags instead.
        .route("/api/v1/tags", get(discovery::facets::tags))
        // Deprecated since 1.26.0: use GET /api/v2/tags instead.
        .route("/api/v1/tags/series", get(discovery::facets::series_tags))
        // Deprecated since 1.26.0: use GET /api/v2/languages instead.
        .route("/api/v1/languages", get(discovery::facets::languages))
        // Deprecated since 1.26.0: use GET /api/v2/publishers instead.
        .route("/api/v1/publishers", get(discovery::facets::publishers))
        // Deprecated since 1.26.0: use GET /api/v2/age-ratings instead.
        .route("/api/v1/age-ratings", get(discovery::facets::age_ratings))
        // Deprecated since 1.26.0: use GET /api/v2/sharing-labels instead.
        .route("/api/v1/sharing-labels", get(discovery::facets::sharing_labels))
        // Deprecated since 1.19.0: use POST /api/v1/series/list instead.
        .route("/api/v1/series", get(discovery::series::series_deprecated_get))
        .route("/api/v1/series/new", get(discovery::series::series_new))
        .route("/api/v1/series/updated", get(discovery::series::series_updated))
        // Deprecated since 1.26.0: use GET /api/v2/series/release-years instead.
        .route(
            "/api/v1/series/release-dates",
            get(discovery::facets::series_release_dates),
        )
        .route("/api/v1/series/latest", get(discovery::series::series_latest))
        // Deprecated since 1.26.0: use GET /api/v2/tags instead.
        .route("/api/v1/tags/book", get(discovery::books::book_tags))
        .route("/api/v1/series/{series_id}", get(discovery::detail::series_detail))
        .route("/api/v1/series/{series_id}/", get(discovery::detail::series_detail))
        .route(
            "/api/v1/series/{series_id}/collections",
            get(discovery::detail::series_collections),
        )
        // Deprecated since 1.19.0: use POST /api/v1/books/list instead.
        .route(
            "/api/v1/series/{series_id}/books",
            get(discovery::books::series_books_deprecated),
        )
        .route(
            "/api/v1/series/{series_id}/thumbnail",
            get(media_assets::handlers::series_thumbnail),
        )
        .route(
            "/api/v1/series/{series_id}/thumbnails",
            get(media_assets::handlers::series_thumbnails)
                .post(media_assets::handlers::series_thumbnail_upload)
                .layer(DefaultBodyLimit::max(
                    crate::operational::MAX_UPLOAD_REQUEST_SIZE_BYTES as usize,
                )),
        )
        .route(
            "/api/v1/series/{series_id}/thumbnails/{thumbnail_id}",
            get(media_assets::handlers::series_thumbnail_by_id).delete(media_assets::handlers::series_thumbnail_delete),
        )
        .route(
            "/api/v1/series/{series_id}/thumbnails/{thumbnail_id}/selected",
            put(media_assets::handlers::series_thumbnail_select),
        )
        .route(
            "/api/v1/series/{series_id}/metadata",
            patch(discovery::detail::series_metadata_update),
        )
        .route(
            "/api/v1/series/{series_id}/metadata/refresh",
            post(media_assets::handlers::series_metadata_refresh),
        )
        .route(
            "/api/v1/series/{series_id}/analyze",
            post(media_assets::handlers::series_analyze),
        )
        .route(
            "/api/v1/series/{series_id}/read-progress",
            post(media_assets::handlers::series_read_progress_post)
                .delete(media_assets::handlers::series_read_progress_delete),
        )
        .route(
            "/api/v1/series/{series_id}/file",
            get(media_assets::handlers::series_file).delete(media_assets::handlers::series_file_delete),
        )
        .route("/api/v1/series/list", post(discovery::series::series_list))
        // Deprecated since 1.19.0: use POST /api/v1/series/list/alphabetical-groups instead.
        .route(
            "/api/v1/series/alphabetical-groups",
            get(discovery::series::series_alphabetical_groups_deprecated_get),
        )
        .route(
            "/api/v1/series/list/alphabetical-groups",
            post(discovery::series::series_alphabetical_groups),
        )
        // Deprecated since 1.19.0: use POST /api/v1/books/list instead.
        .route("/api/v1/books", get(discovery::books::books_deprecated_get))
        .route("/api/v1/books/list", post(discovery::books::books_list))
        .route("/api/v1/books/latest", get(discovery::books::books_latest))
        .route("/api/v1/books/ondeck", get(discovery::books::books_ondeck))
        .route("/api/v1/books/duplicates", get(discovery::books::books_duplicates))
        .route("/api/v1/books/{book_id}", get(discovery::detail::book_detail))
        .route(
            "/api/v1/books/{book_id}/previous",
            get(discovery::detail::book_sibling_previous),
        )
        .route(
            "/api/v1/books/{book_id}/next",
            get(discovery::detail::book_sibling_next),
        )
        .route(
            "/api/v1/books/{book_id}/readlists",
            get(discovery::detail::book_readlists),
        )
        .route(
            "/api/v1/readlists",
            get(discovery::detail::readlists).post(discovery::detail::readlist_create),
        )
        .route(
            "/api/v1/readlists/match/comicrack",
            post(discovery::detail::readlist_match_comicrack)
                .layer(DefaultBodyLimit::max(
                    crate::operational::MAX_UPLOAD_REQUEST_SIZE_BYTES as usize,
                )),
        )
        .route(
            "/api/v1/readlists/{readlist_id}",
            get(discovery::detail::readlist_detail)
                .patch(discovery::detail::readlist_update)
                .delete(discovery::detail::readlist_delete),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/thumbnail",
            get(media_assets::handlers::readlist_thumbnail),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/thumbnails",
            get(media_assets::handlers::readlist_thumbnails)
                .post(media_assets::handlers::readlist_thumbnail_upload)
                .layer(DefaultBodyLimit::max(
                    crate::operational::MAX_UPLOAD_REQUEST_SIZE_BYTES as usize,
                )),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/thumbnails/{thumbnail_id}",
            get(media_assets::handlers::readlist_thumbnail_by_id).delete(media_assets::handlers::readlist_thumbnail_delete),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/thumbnails/{thumbnail_id}/selected",
            put(media_assets::handlers::readlist_thumbnail_select),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/books",
            get(discovery::detail::readlist_books),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/books/{book_id}/previous",
            get(discovery::detail::readlist_book_sibling_previous),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/books/{book_id}/next",
            get(discovery::detail::readlist_book_sibling_next),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/read-progress/tachiyomi",
            get(media_assets::handlers::readlist_tachiyomi_read_progress_get)
                .put(media_assets::handlers::readlist_tachiyomi_read_progress_put),
        )
        .route(
            "/api/v1/readlists/{readlist_id}/file",
            get(media_assets::handlers::readlist_file),
        )
        .route(
            "/api/v1/collections",
            get(discovery::detail::collections).post(discovery::detail::collection_create),
        )
        .route(
            "/api/v1/collections/{collection_id}/series",
            get(discovery::detail::collection_series),
        )
        .route(
            "/api/v1/collections/{collection_id}",
            get(discovery::detail::collection_detail)
                .patch(discovery::detail::collection_update)
                .delete(discovery::detail::collection_delete),
        )
        .route(
            "/api/v1/collections/{collection_id}/thumbnail",
            get(media_assets::handlers::collection_thumbnail),
        )
        .route(
            "/api/v1/collections/{collection_id}/thumbnails",
            get(media_assets::handlers::collection_thumbnails)
                .post(media_assets::handlers::collection_thumbnail_upload)
                .layer(DefaultBodyLimit::max(
                    crate::operational::MAX_UPLOAD_REQUEST_SIZE_BYTES as usize,
                )),
        )
        .route(
            "/api/v1/collections/{collection_id}/thumbnails/{thumbnail_id}",
            get(media_assets::handlers::collection_thumbnail_by_id).delete(media_assets::handlers::collection_thumbnail_delete),
        )
        .route(
            "/api/v1/collections/{collection_id}/thumbnails/{thumbnail_id}/selected",
            put(media_assets::handlers::collection_thumbnail_select),
        )
        .route("/api/v1/books/{book_id}/pages", get(media_assets::handlers::book_pages))
        .route(
            "/api/v1/books/{book_id}/positions",
            get(media_assets::handlers::book_positions),
        )
        .route(
            "/api/v1/books/{book_id}/pages/{page_number}",
            get(media_assets::handlers::book_page),
        )
        .route(
            "/api/v1/books/{book_id}/pages/{page_number}/raw",
            get(media_assets::handlers::book_page_raw),
        )
        .route(
            "/api/v1/books/{book_id}/pages/{page_number}/thumbnail",
            get(media_assets::handlers::book_page_thumbnail),
        )
        .route(
            "/api/v1/books/{book_id}/thumbnail",
            get(media_assets::handlers::book_thumbnail),
        )
        .route(
            "/api/v1/books/{book_id}/thumbnails",
            get(media_assets::handlers::book_thumbnails)
                .post(media_assets::handlers::book_thumbnail_upload)
                .layer(DefaultBodyLimit::max(
                    crate::operational::MAX_UPLOAD_REQUEST_SIZE_BYTES as usize,
                )),
        )
        .route(
            "/api/v1/books/{book_id}/thumbnails/{thumbnail_id}",
            get(media_assets::handlers::book_thumbnail_by_id).delete(media_assets::handlers::book_thumbnail_delete),
        )
        .route(
            "/api/v1/books/{book_id}/thumbnails/{thumbnail_id}/selected",
            put(media_assets::handlers::book_thumbnail_select),
        )
        .route(
            "/api/v1/books/{book_id}/manifest",
            get(media_assets::handlers::book_manifest),
        )
        .route(
            "/api/v1/books/{book_id}/manifest/epub",
            get(media_assets::handlers::book_manifest_epub),
        )
        .route(
            "/api/v1/books/{book_id}/manifest/pdf",
            get(media_assets::handlers::book_manifest_pdf),
        )
        .route(
            "/api/v1/books/{book_id}/manifest/divina",
            get(media_assets::handlers::book_manifest_divina),
        )
        .route(
            "/api/v1/books/{book_id}/file",
            get(media_assets::handlers::book_file).delete(media_assets::handlers::book_file_delete),
        )
        .route(
            "/api/v1/books/{book_id}/file/{*file_name}",
            get(media_assets::handlers::book_file_with_suffix),
        )
        .route(
            "/api/v1/books/{book_id}/resource/{*resource_path}",
            get(media_assets::handlers::book_resource),
        )
        .route(
            "/opds/v2/books/{book_id}/resource/{*resource_path}",
            get(media_assets::handlers::book_resource_opds_v2),
        )
        .route(
            "/api/v1/books/thumbnails",
            put(media_assets::handlers::books_thumbnails_regenerate),
        )
        .route(
            "/api/v1/books/{book_id}/analyze",
            post(media_assets::handlers::book_analyze),
        )
        .route(
            "/api/v1/books/{book_id}/metadata/refresh",
            post(media_assets::handlers::book_metadata_refresh),
        )
        .route(
            "/api/v1/books/{book_id}/metadata",
            axum::routing::patch(media_assets::handlers::book_metadata_update),
        )
        .route(
            "/api/v1/books/metadata",
            axum::routing::patch(media_assets::handlers::book_metadata_batch_update),
        )
        .route("/api/v1/books/import", post(media_assets::handlers::books_import))
        .route(
            "/api/v1/books/{book_id}/read-progress",
            patch(media_assets::handlers::book_read_progress)
                .delete(media_assets::handlers::book_read_progress_delete),
        )
        .route(
            "/api/v1/books/{book_id}/progression",
            get(media_assets::handlers::book_progression_get)
                .put(media_assets::handlers::book_progression),
        )
        .route(
            "/api/v2/users",
            get(identity_access::content_auth::users_list_route)
                .post(identity_access::content_auth::users_create_route),
        )
        .route("/api/v2/users/me", get(identity_access::content_auth::users_me_route))
        .route(
            "/api/v2/users/{id}",
            patch(identity_access::content_auth::users_update_route)
                .delete(identity_access::content_auth::users_delete_route),
        )
        .route(
            "/api/v2/users/me/password",
            patch(identity_access::content_auth::users_me_password_route),
        )
        .route(
            "/api/v2/users/me/api-keys",
            get(identity_access::content_auth::users_me_api_keys_list_route)
                .post(identity_access::content_auth::users_me_api_keys_create_route),
        )
        .route(
            "/api/v2/users/me/api-keys/{key_id}",
            delete(identity_access::content_auth::users_me_api_keys_delete_route),
        )
        .route(
            "/api/v2/users/me/authentication-activity",
            get(identity_access::content_auth::users_me_authentication_activity_route),
        )
        .route(
            "/api/v2/users/authentication-activity",
            get(identity_access::content_auth::users_authentication_activity_route),
        )
        .route(
            "/api/v2/users/{id}/authentication-activity/latest",
            get(identity_access::content_auth::users_by_id_authentication_activity_latest_route),
        )
        .route(
            "/api/v2/series/{series_id}/read-progress/tachiyomi",
            get(media_assets::handlers::series_tachiyomi_read_progress_get)
                .put(media_assets::handlers::series_tachiyomi_read_progress_put),
        )
        .route(
            "/api/v2/users/{id}/password",
            patch(identity_access::content_auth::users_by_id_password_route),
        )
        .route("/api/v2/authors", get(discovery::facets::authors_v2))
        .route(
            "/api/v2/authors/names",
            get(discovery::facets::authors_names_v2),
        )
        .route(
            "/api/v2/authors/roles",
            get(discovery::facets::authors_roles_v2),
        )
        .route("/api/v2/genres", get(discovery::facets::genres_v2))
        .route(
            "/api/v2/sharing-labels",
            get(discovery::facets::sharing_labels_v2),
        )
        .route("/api/v2/languages", get(discovery::facets::languages_v2))
        .route("/api/v2/publishers", get(discovery::facets::publishers_v2))
        .route("/api/v2/tags", get(discovery::facets::tags_v2))
        .route(
            "/api/v2/series/release-years",
            get(discovery::facets::series_release_years_v2),
        )
        .route("/api/v2/age-ratings", get(discovery::facets::age_ratings_v2))
        .route("/opds/v1.2/catalog", get(opds::opds_v1_catalog_route))
        .route("/opds/v1.2/search", get(opds::opds_v1_search_route))
        .route("/opds/v1.2/ondeck", get(opds::opds_v1_on_deck_route))
        .route("/opds/v1.2/keep-reading", get(opds::opds_v1_keep_reading_route))
        .route("/opds/v1.2/series", get(opds::opds_v1_series_route))
        .route("/opds/v1.2/series/latest", get(opds::opds_v1_series_latest_route))
        .route("/opds/v1.2/books/latest", get(opds::opds_v1_books_latest_route))
        .route("/opds/v1.2/libraries", get(opds::opds_v1_libraries_route))
        .route("/opds/v1.2/collections", get(opds::opds_v1_collections_route))
        .route("/opds/v1.2/readlists", get(opds::opds_v1_readlists_route))
        .route("/opds/v1.2/publishers", get(opds::opds_v1_publishers_route))
        .route("/opds/v1.2/series/{series_id}", get(opds::opds_v1_series_detail_route))
        .route(
            "/opds/v1.2/libraries/{library_id}",
            get(opds::opds_v1_library_detail_route),
        )
        .route(
            "/opds/v1.2/collections/{collection_id}",
            get(opds::opds_v1_collection_detail_route),
        )
        .route(
            "/opds/v1.2/readlists/{readlist_id}",
            get(opds::opds_v1_readlist_detail_route),
        )
        .route(
            "/opds/v1.2/books/{book_id}/file/{file_name}",
            get(opds::opds_v1_book_file_route),
        )
        .route(
            "/opds/v1.2/books/{book_id}/thumbnail",
            get(opds::opds_v1_book_thumbnail_route),
        )
        .route(
            "/opds/v1.2/books/{book_id}/thumbnail/small",
            get(opds::opds_v1_book_thumbnail_small_route),
        )
        .route(
            "/opds/v1.2/books/{book_id}/pages/{page_number}",
            get(media_assets::handlers::book_page_opds_v1),
        )
        .route("/opds/v2/auth", get(opds::opds_auth_route))
        .route("/opds/v2/catalog", get(opds::opds_catalog))
        .route("/opds/v2/libraries", get(opds::opds_v2_libraries))
        .route(
            "/opds/v2/libraries/keep-reading",
            get(opds::opds_v2_libraries_keep_reading_route),
        )
        .route(
            "/opds/v2/libraries/on-deck",
            get(opds::opds_v2_libraries_on_deck_route),
        )
        .route(
            "/opds/v2/libraries/books/latest",
            get(opds::opds_v2_libraries_latest_books_route),
        )
        .route(
            "/opds/v2/libraries/series/latest",
            get(opds::opds_v2_libraries_latest_series_route),
        )
        .route(
            "/opds/v2/libraries/browse",
            get(opds::opds_v2_libraries_browse_route),
        )
        .route(
            "/opds/v2/libraries/collections",
            get(opds::opds_v2_libraries_collections_route),
        )
        .route(
            "/opds/v2/libraries/readlists",
            get(opds::opds_v2_libraries_readlists_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}",
            get(opds::opds_v2_library_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/keep-reading",
            get(opds::opds_v2_library_keep_reading_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/on-deck",
            get(opds::opds_v2_library_on_deck_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/books/latest",
            get(opds::opds_v2_library_latest_books_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/series/latest",
            get(opds::opds_v2_library_latest_series_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/browse",
            get(opds::opds_v2_library_browse_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/collections",
            get(opds::opds_v2_library_collections_route),
        )
        .route(
            "/opds/v2/libraries/{library_id}/readlists",
            get(opds::opds_v2_library_readlists_route),
        )
        .route(
            "/opds/v2/collections/{collection_id}",
            get(opds::opds_v2_collection_route),
        )
        .route("/opds/v2/series/{series_id}", get(opds::opds_v2_series_route))
        .route(
            "/opds/v2/readlists/{readlist_id}",
            get(opds::opds_v2_readlist_route),
        )
        .route("/opds/v2/search", get(opds::opds_v2_search_route))
        .route(
            "/opds/v2/books/{book_id}/manifest",
            get(opds::opds_manifest_route),
        )
        .route(
            "/opds/v2/books/{book_id}/manifest/{manifest_profile}",
            get(opds::opds_manifest_profile_route),
        )
        .route("/opds/v2/books/{book_id}/file", get(opds::opds_v2_book_file_route))
        .route(
            "/opds/v2/books/{book_id}/file/{*file_name}",
            get(opds::opds_v2_book_file_with_suffix_route),
        )
        .route(
            "/opds/v2/books/{book_id}/thumbnail",
            get(opds::opds_v2_book_thumbnail_route),
        )
        .route(
            "/opds/v2/books/{book_id}/pages/{page_number}",
            get(opds::opds_v2_book_page_route),
        )
        .route(
            "/opds/v2/books/{book_id}/pages/{page_number}/raw",
            get(opds::opds_v2_book_page_raw_route),
        )
        .route(
            "/opds/v2/books/{book_id}/progression",
            get(media_assets::handlers::opds_v2_book_progression_get)
                .put(media_assets::handlers::opds_v2_book_progression),
        )
        .route(
            "/api/v1/login/set-cookie",
            get(identity_access::content_auth::login_set_cookie_route),
        )
        .route(
            "/api/logout",
            post(identity_access::content_auth::logout_route),
        )
        .route("/sse/v1/events", get(operational::sse_events))
        .route("/", get(operational::webui_entrypoint))
        .route("/next", get(operational::nextui_entrypoint))
        .route("/{*webui_path}", get(operational::webui_asset));

    let router = if dev_cors_enabled {
        router.layer(operational::dev_cors_layer())
    } else {
        router
    };

    let router = if actuator_enabled {
        router
            .route("/actuator", get(operational::actuator_root))
            .route("/actuator/health", get(operational::actuator_health))
            .route("/actuator/info", get(operational::actuator_info))
            .route("/actuator/logfile", get(operational::actuator_logfile))
            .route("/actuator/shutdown", post(operational::actuator_shutdown))
            .route(
                "/actuator/metrics",
                get(operational::actuator_metrics_index),
            )
            .route(
                "/actuator/metrics/{metric_name}",
                get(operational::actuator_metric_detail),
            )
    } else {
        router
    };

    let router = with_access_logging(
        router
            .layer(csrf_layer(dev_cors_enabled))
            .route_layer(middleware::from_fn(cache::cache_workflow_middleware)),
        app.operational.http_server_requests.clone(),
    )
    .with_state(app);

    if let Some(runtime_context_path) = runtime_context_path {
        Router::new().nest(
            runtime_context_path.as_str(),
            router.layer(middleware::from_fn_with_state(
                runtime_context_path.clone(),
                inject_runtime_context_path,
            )),
        )
    } else {
        router
    }
}

pub fn with_access_logging<S>(
    router: Router<S>,
    http_server_requests: HttpServerRequestsState,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .route_layer(middleware::from_fn_with_state(
            http_server_requests,
            access_log::prepare_access_log_middleware,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(access_log::make_request_span)
                .on_request(access_log::on_request)
                .on_response(access_log::on_response)
                .on_failure(access_log::on_failure),
        )
}

fn mounted_runtime_context_path(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "/")
        .map(str::to_string)
}

fn csrf_layer(dev_cors_enabled: bool) -> CsrfLayer {
    let layer = CsrfLayer::new().with_insecure_bypass(non_browser_client_protocol_path);

    if dev_cors_enabled {
        layer
            .add_trusted_origin(operational::DEV_FRONTEND_ORIGIN)
            .expect("dev frontend origin should be a valid CSRF trusted origin")
    } else {
        layer
    }
}

fn non_browser_client_protocol_path(_method: &Method, uri: &Uri) -> bool {
    let path = uri.path();
    matches!(path, "/kobo" | "/koreader" | "/opds")
        || path.starts_with("/kobo/")
        || path.starts_with("/koreader/")
        || path.starts_with("/opds/")
}

async fn inject_runtime_context_path(
    State(runtime_context_path): State<String>,
    mut request: Request,
    next: Next,
) -> Response {
    if !request.headers().contains_key("x-forwarded-prefix") {
        let header_value = HeaderValue::from_str(runtime_context_path.as_str())
            .expect("validated runtime context path should serialize as a header");
        request
            .headers_mut()
            .insert("x-forwarded-prefix", header_value);
    }

    next.run(request).await
}
