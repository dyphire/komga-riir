use std::collections::BTreeSet;

use anyhow::Context;
use komga_application::media_assets::{
    book_media_is_epub, book_media_is_rar_archive, book_media_is_zip_archive,
};
use komga_application::runtime_sse::RuntimeSseEventSink;
use sqlx::{Row, SqlitePool};

use crate::{load_comicinfo_bytes_for_media, parse_comicinfo_xml};
use komga_infrastructure_base::{RiirDatabase, resolve_stored_path};
use komga_infrastructure_media_core::content::epub_resources::load_epub_package_document;
use komga_infrastructure_media_core::content::metadata_sources::{
    CapturedMetadataDocument, CapturedMetadataSources,
};

mod artwork_refresh;
mod artwork_support;
mod barcode;
mod epub;
mod events;
mod patch;
mod queries;
mod readlist;
mod series_aggregation;
mod series_metadata;
mod series_metadata_contribution;
mod sources;
mod support;

pub use artwork_refresh::generate_book_thumbnail;
pub use artwork_refresh::{refresh_book_local_artwork, refresh_series_local_artwork};
use epub::{extract_epub_book_patch, extract_epub_series_patch};
pub use series_aggregation::aggregate_series_metadata;
use series_metadata::{
    apply_mylar_series_import, apply_oneshot_series_metadata_import,
    apply_series_metadata_from_book_imports,
};
pub use series_metadata_contribution::RiirSeriesMetadataContributionCleanup;
use series_metadata_contribution::{
    SeriesMetadataContribution, SeriesMetadataContributionOutcome,
    SeriesMetadataContributionSource, SeriesMetadataProvider, upsert,
};
use sources::{
    extract_comicinfo_book_patch, extract_comicinfo_readlists, extract_comicinfo_series_patch,
};

fn supplied_document<'a>(
    source: &'a CapturedMetadataDocument,
    name: &str,
) -> anyhow::Result<Option<&'a [u8]>> {
    match source {
        CapturedMetadataDocument::Present(bytes) => Ok(Some(bytes)),
        CapturedMetadataDocument::Absent => Ok(None),
        CapturedMetadataDocument::NotRequested | CapturedMetadataDocument::NotApplicable => {
            anyhow::bail!("{name} is required but was not captured during analysis")
        }
        CapturedMetadataDocument::Failed(error) => {
            anyhow::bail!("{name} capture failed during analysis: {error}")
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefreshBookMetadataOutcome {
    pub series_id: Option<String>,
    pub library_id: Option<String>,
    pub changed_readlist_ids: Vec<String>,
    pub book_changed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransientMetadataProviderInference {
    pub series_titles: Vec<String>,
    pub number: Option<f64>,
}

struct SeriesMetadataRefreshContext {
    library_id: Option<String>,
    should_emit_series_changed: bool,
}

fn push_transient_series_title(series_titles: &mut Vec<String>, title: Option<String>) {
    let Some(title) = title.map(|value| value.trim().to_string()) else {
        return;
    };
    if title.is_empty() || series_titles.iter().any(|existing| existing == &title) {
        return;
    }
    series_titles.push(title);
}

pub fn infer_transient_epub_provider_metadata(
    package_document: &[u8],
) -> anyhow::Result<TransientMetadataProviderInference> {
    let book_patch = extract_epub_book_patch(package_document)?;
    let series_patch = extract_epub_series_patch(package_document)?;
    let mut series_titles = Vec::new();
    push_transient_series_title(&mut series_titles, series_patch.title);

    Ok(TransientMetadataProviderInference {
        series_titles,
        number: book_patch.number_sort,
    })
}

pub fn infer_transient_comicinfo_provider_metadata(
    xml: &[u8],
) -> anyhow::Result<TransientMetadataProviderInference> {
    let document = parse_comicinfo_xml(xml)?;
    let book_patch = extract_comicinfo_book_patch(&document);
    let append_volume_patch = extract_comicinfo_series_patch(&document, true);
    let plain_patch = extract_comicinfo_series_patch(&document, false);
    let mut series_titles = Vec::new();
    push_transient_series_title(&mut series_titles, append_volume_patch.title);
    push_transient_series_title(&mut series_titles, plain_patch.title);

    Ok(TransientMetadataProviderInference {
        series_titles,
        number: book_patch.number_sort,
    })
}

pub async fn refresh_book_metadata(
    pool: &SqlitePool,
    riir_db: Option<&RiirDatabase>,
    runtime_events: &dyn RuntimeSseEventSink,
    book_id: &str,
    capabilities: &BTreeSet<String>,
) -> anyhow::Result<RefreshBookMetadataOutcome> {
    refresh_book_metadata_internal(pool, riir_db, runtime_events, book_id, capabilities, None).await
}

pub async fn refresh_book_metadata_with_sources(
    pool: &SqlitePool,
    riir_db: Option<&RiirDatabase>,
    runtime_events: &dyn RuntimeSseEventSink,
    book_id: &str,
    capabilities: &BTreeSet<String>,
    sources: &CapturedMetadataSources,
) -> anyhow::Result<RefreshBookMetadataOutcome> {
    refresh_book_metadata_internal(
        pool,
        riir_db,
        runtime_events,
        book_id,
        capabilities,
        Some(sources),
    )
    .await
}

async fn refresh_book_metadata_internal(
    pool: &SqlitePool,
    riir_db: Option<&RiirDatabase>,
    runtime_events: &dyn RuntimeSseEventSink,
    book_id: &str,
    capabilities: &BTreeSet<String>,
    supplied_sources: Option<&CapturedMetadataSources>,
) -> anyhow::Result<RefreshBookMetadataOutcome> {
    let book_id = book_id.to_string();
    let book_id_for_events = book_id.clone();
    let outcome = {
        let mut changed_readlist_ids = BTreeSet::new();
        let mut should_emit_book_changed = false;
        let book_row = sqlx::query(
            r#"
            SELECT b.URL AS BOOK_URL,
                   l.IMPORT_COMICINFO_BOOK AS IMPORT_COMICINFO_BOOK,
                   l.IMPORT_COMICINFO_READLIST AS IMPORT_COMICINFO_READLIST,
                   l.IMPORT_COMICINFO_SERIES AS IMPORT_COMICINFO_SERIES,
                   l.IMPORT_COMICINFO_COLLECTION AS IMPORT_COMICINFO_COLLECTION,
                   l.IMPORT_EPUB_BOOK AS IMPORT_EPUB_BOOK,
                   l.IMPORT_EPUB_SERIES AS IMPORT_EPUB_SERIES,
                   unixepoch(b.FILE_LAST_MODIFIED) AS FILE_LAST_MODIFIED,
                   b.FILE_SIZE AS FILE_SIZE,
                   COALESCE(m.MEDIA_TYPE, 'application/octet-stream') AS MEDIA_TYPE,
                   unixepoch(m.LAST_MODIFIED_DATE) AS MEDIA_LAST_MODIFIED,
                   m.STATUS AS MEDIA_STATUS,
                   l.IMPORT_BARCODE_ISBN AS IMPORT_BARCODE_ISBN
            FROM BOOK b
            JOIN LIBRARY l ON l.ID = b.LIBRARY_ID
            LEFT JOIN MEDIA m ON m.BOOK_ID = b.ID
            WHERE b.ID = ?
            LIMIT 1
            "#,
        )
        .bind(&book_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to resolve book path for metadata refresh '{book_id}'"
            ))
        })?;

        if let Some(book_row) = &book_row {
            let import_comicinfo_book = book_row.get::<bool, _>("IMPORT_COMICINFO_BOOK");
            let import_comicinfo_readlist = book_row.get::<bool, _>("IMPORT_COMICINFO_READLIST");
            let import_comicinfo_series = book_row.get::<bool, _>("IMPORT_COMICINFO_SERIES");
            let import_comicinfo_collection =
                book_row.get::<bool, _>("IMPORT_COMICINFO_COLLECTION");
            let import_epub_book = book_row.get::<bool, _>("IMPORT_EPUB_BOOK");
            let import_epub_series = book_row.get::<bool, _>("IMPORT_EPUB_SERIES");
            let import_barcode_isbn = book_row.get::<bool, _>("IMPORT_BARCODE_ISBN");
            should_emit_book_changed |=
                import_comicinfo_book && comicinfo_provider_matches_capabilities(capabilities);
            should_emit_book_changed |=
                import_epub_book && epub_provider_matches_capabilities(capabilities);
            should_emit_book_changed |=
                import_barcode_isbn && barcode_provider_matches_capabilities(capabilities);
            let should_persist_comicinfo = import_comicinfo_series || import_comicinfo_collection;
            let should_read_comicinfo =
                (import_comicinfo_book || import_comicinfo_readlist || should_persist_comicinfo)
                    && comicinfo_provider_matches_capabilities(capabilities);
            let should_read_epub = (import_epub_book || import_epub_series)
                && epub_provider_matches_capabilities(capabilities);
            let media = if should_read_comicinfo || should_read_epub {
                load_book_media_for_refresh(pool, &book_id).await?
            } else {
                None
            };
            let comicinfo_media = media.as_ref().filter(|media| {
                should_read_comicinfo
                    && (book_media_is_zip_archive(media) || book_media_is_rar_archive(media))
            });
            let epub_media = media
                .as_ref()
                .filter(|media| should_read_epub && book_media_is_epub(media));
            // Validate both handoff sources before either provider writes metadata.
            if let Some(sources) = supplied_sources {
                if comicinfo_media.is_some() {
                    supplied_document(&sources.comicinfo, "ComicInfo.xml")?;
                }
                if epub_media.is_some() {
                    supplied_document(&sources.epub, "EPUB package document")?;
                }
            }
            if let Some(media) = comicinfo_media {
                let document = match supplied_sources {
                    Some(sources) => supplied_document(&sources.comicinfo, "ComicInfo.xml")?
                        .map(parse_comicinfo_xml)
                        .transpose()
                        .map_err(|error| {
                            anyhow::anyhow!(error).context(format!(
                                "failed to use supplied ComicInfo.xml for '{}'",
                                media.file_path.display()
                            ))
                        })?,
                    None => load_comicinfo_bytes_for_media(media)?
                        .as_deref()
                        .map(parse_comicinfo_xml)
                        .transpose()
                        .map_err(|error| {
                            anyhow::anyhow!(error).context(format!(
                                "failed to parse ComicInfo.xml from '{}'",
                                media.file_path.display()
                            ))
                        })?,
                };

                if should_persist_comicinfo
                    && book_row.get::<Option<String>, _>("MEDIA_STATUS").as_deref() == Some("READY")
                    && let Some(media_modified_seconds) =
                        book_row.get::<Option<i64>, _>("MEDIA_LAST_MODIFIED")
                {
                    let riir_db =
                        riir_db.context("book metadata refresh requires the RIIR database")?;
                    let source = SeriesMetadataContributionSource {
                        book_id: book_id.clone(),
                        file_last_modified_seconds: book_row.get("FILE_LAST_MODIFIED"),
                        file_size: book_row.get("FILE_SIZE"),
                        media_type: book_row.get("MEDIA_TYPE"),
                        media_modified_seconds,
                    };
                    let outcome = document.as_ref().map_or(
                        SeriesMetadataContributionOutcome::Absent,
                        |document| {
                            SeriesMetadataContributionOutcome::Present(Box::new(
                                SeriesMetadataContribution::ComicInfo {
                                    plain: Box::new(extract_comicinfo_series_patch(
                                        document, false,
                                    )),
                                    append_volume: Box::new(extract_comicinfo_series_patch(
                                        document, true,
                                    )),
                                },
                            ))
                        },
                    );
                    upsert(riir_db, SeriesMetadataProvider::ComicInfo, &source, outcome).await?;
                }

                if let Some(document) = document {
                    if import_comicinfo_book {
                        let patch = extract_comicinfo_book_patch(&document);
                        apply_book_metadata_import_patch(pool, &book_id, patch).await?;
                    }

                    if import_comicinfo_readlist {
                        for readlist in extract_comicinfo_readlists(&document) {
                            if let Some(readlist_id) = readlist::upsert_comicinfo_readlist(
                                pool,
                                runtime_events,
                                &book_id,
                                readlist,
                            )
                            .await?
                            {
                                changed_readlist_ids.insert(readlist_id);
                            }
                        }
                    }
                }
            }

            if let Some(media) = epub_media {
                let package_document = match supplied_sources {
                    Some(sources) => supplied_document(&sources.epub, "EPUB package document")?
                        .map(std::borrow::Cow::Borrowed),
                    None => load_epub_package_document(media)
                        .await?
                        .map(std::borrow::Cow::Owned),
                };

                if import_epub_series
                    && book_row.get::<Option<String>, _>("MEDIA_STATUS").as_deref() == Some("READY")
                    && let Some(media_modified_seconds) =
                        book_row.get::<Option<i64>, _>("MEDIA_LAST_MODIFIED")
                {
                    let riir_db =
                        riir_db.context("book metadata refresh requires the RIIR database")?;
                    let source = SeriesMetadataContributionSource {
                        book_id: book_id.clone(),
                        file_last_modified_seconds: book_row.get("FILE_LAST_MODIFIED"),
                        file_size: book_row.get("FILE_SIZE"),
                        media_type: book_row.get("MEDIA_TYPE"),
                        media_modified_seconds,
                    };
                    let outcome = package_document.as_deref().map_or(
                        Ok(SeriesMetadataContributionOutcome::Absent),
                        |package_document| {
                            extract_epub_series_patch(package_document).map(|patch| {
                                SeriesMetadataContributionOutcome::Present(Box::new(
                                    SeriesMetadataContribution::Epub {
                                        patch: Box::new(patch),
                                    },
                                ))
                            })
                        },
                    )?;
                    upsert(riir_db, SeriesMetadataProvider::Epub, &source, outcome).await?;
                }

                if import_epub_book && let Some(package_document) = package_document {
                    let patch = extract_epub_book_patch(&package_document)?;
                    apply_book_metadata_import_patch(pool, &book_id, patch).await?;
                }
            }

            if import_barcode_isbn && barcode_provider_matches_capabilities(capabilities) {
                barcode::refresh_barcode_isbn(pool, &book_id).await?;
            }
        }

        sqlx::query(
            r#"
            UPDATE BOOK_METADATA
            SET LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
            WHERE BOOK_ID = ?
            "#,
        )
        .bind(&book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to refresh BOOK_METADATA for '{book_id}'"))
        })?;

        let book_context = sqlx::query(
            r#"
            SELECT SERIES_ID, LIBRARY_ID
            FROM BOOK
            WHERE ID = ?
            LIMIT 1
            "#,
        )
        .bind(&book_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to resolve book SSE context for '{book_id}'"
            ))
        })?;
        let series_id = book_context
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("SERIES_ID"));
        let library_id = book_context
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("LIBRARY_ID"));

        RefreshBookMetadataOutcome {
            series_id,
            library_id,
            changed_readlist_ids: changed_readlist_ids.into_iter().collect(),
            book_changed: should_emit_book_changed,
        }
    };

    if outcome.book_changed
        && let (Some(series_id), Some(library_id)) =
            (outcome.series_id.as_deref(), outcome.library_id.as_deref())
    {
        events::emit_book_changed(runtime_events, &book_id_for_events, series_id, library_id);
    }

    Ok(outcome)
}

use patch::{BookMetadataImportPatch, SeriesMetadataImportPatch};

fn comicinfo_provider_matches_capabilities(capabilities: &BTreeSet<String>) -> bool {
    capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "TITLE"
                | "SUMMARY"
                | "NUMBER"
                | "NUMBER_SORT"
                | "RELEASE_DATE"
                | "AUTHORS"
                | "READ_LISTS"
                | "LINKS"
        )
    })
}

fn epub_provider_matches_capabilities(capabilities: &BTreeSet<String>) -> bool {
    capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "TITLE" | "SUMMARY" | "RELEASE_DATE" | "AUTHORS" | "ISBN"
        )
    })
}

fn barcode_provider_matches_capabilities(capabilities: &BTreeSet<String>) -> bool {
    capabilities.contains("ISBN")
}

async fn apply_book_metadata_import_patch(
    pool: &SqlitePool,
    book_id: &str,
    patch: BookMetadataImportPatch,
) -> anyhow::Result<()> {
    let Some(mut metadata) = load_book_metadata_for_refresh(pool, book_id).await? else {
        return Ok(());
    };

    if patch::apply_patch_to_metadata(&mut metadata, patch) {
        let persisted = persist_book_metadata_for_refresh(pool, book_id, &metadata).await?;
        if !persisted {
            return Err(anyhow::anyhow!(format!(
                "book metadata row disappeared before metadata refresh for '{book_id}'"
            )));
        }
    }

    Ok(())
}

use queries::{
    load_book_media_for_refresh, load_book_metadata_for_refresh, load_book_page_row_for_refresh,
    persist_book_metadata_for_refresh,
};

pub async fn refresh_series_metadata(
    pool: &SqlitePool,
    riir_db: Option<&RiirDatabase>,
    runtime_events: &dyn RuntimeSseEventSink,
    series_id: &str,
) -> anyhow::Result<()> {
    let series_id = series_id.to_string();
    let series_id_for_events = series_id.clone();

    let refresh_context = {
        let mut should_emit_series_changed = false;
        let series_row = sqlx::query(
                r#"
                SELECT s.URL AS SERIES_URL,
                       l.ROOT AS LIBRARY_ROOT,
                       s.ONESHOT AS ONESHOT,
                       l.IMPORT_COMICINFO_SERIES AS IMPORT_COMICINFO_SERIES,
                       l.IMPORT_COMICINFO_COLLECTION AS IMPORT_COMICINFO_COLLECTION,
                       l.IMPORT_COMICINFO_SERIES_APPEND_VOLUME AS IMPORT_COMICINFO_SERIES_APPEND_VOLUME,
                       l.IMPORT_EPUB_SERIES AS IMPORT_EPUB_SERIES,
                       l.IMPORT_MYLAR_SERIES AS IMPORT_MYLAR_SERIES
                FROM SERIES s
                JOIN LIBRARY l ON l.ID = s.LIBRARY_ID
                WHERE s.ID = ?
                LIMIT 1
                "#,
        )
        .bind(&series_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| { anyhow::anyhow!(error).context( format!("failed to resolve series path for metadata refresh '{series_id}'"))
        })?;

        if let Some(series_row) = &series_row {
            let series_url = series_row.get::<String, _>("SERIES_URL");
            let library_root = series_row.get::<String, _>("LIBRARY_ROOT");
            let resolved_library_root = resolve_stored_path(&library_root);
            let oneshot = series_row.get::<i64, _>("ONESHOT") != 0;
            let import_comicinfo_series = series_row.get::<bool, _>("IMPORT_COMICINFO_SERIES");
            let import_comicinfo_collection =
                series_row.get::<bool, _>("IMPORT_COMICINFO_COLLECTION");
            let import_comicinfo_series_append_volume =
                series_row.get::<bool, _>("IMPORT_COMICINFO_SERIES_APPEND_VOLUME");
            let import_epub_series = series_row.get::<bool, _>("IMPORT_EPUB_SERIES");
            let import_mylar_series = series_row.get::<bool, _>("IMPORT_MYLAR_SERIES");
            should_emit_series_changed =
                import_comicinfo_series || import_epub_series || import_mylar_series || oneshot;

            if import_comicinfo_series || import_comicinfo_collection || import_epub_series {
                let riir_db =
                    riir_db.context("series metadata refresh requires the RIIR database")?;
                apply_series_metadata_from_book_imports(
                    pool,
                    runtime_events,
                    &series_id,
                    riir_db,
                    import_comicinfo_series,
                    import_comicinfo_collection,
                    import_comicinfo_series_append_volume,
                    import_epub_series,
                )
                .await?;
            }

            apply_mylar_series_import(
                pool,
                &series_id,
                resolved_library_root.as_path(),
                &series_url,
                import_mylar_series,
                oneshot,
            )
            .await?;

            if oneshot {
                apply_oneshot_series_metadata_import(pool, &series_id).await?;
            }
        }

        sqlx::query(
            r#"
                UPDATE SERIES_METADATA
                SET LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
                WHERE SERIES_ID = ?
                "#,
        )
        .bind(&series_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to refresh SERIES_METADATA for '{series_id}'"
            ))
        })?;

        sqlx::query(
            r#"
                SELECT LIBRARY_ID
                FROM SERIES
                WHERE ID = ?
                LIMIT 1
                "#,
        )
        .bind(&series_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to resolve LIBRARY_ID for refreshed series '{series_id}'"
            ))
        })
        .map(|row| SeriesMetadataRefreshContext {
            library_id: row.and_then(|row| row.get::<Option<String>, _>("LIBRARY_ID")),
            should_emit_series_changed,
        })
    }?;

    if refresh_context.should_emit_series_changed
        && let Some(library_id) = refresh_context.library_id.as_deref()
    {
        events::emit_series_changed(runtime_events, &series_id_for_events, library_id);
    }
    Ok(())
}
