use std::collections::HashMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite};

use komga_application::discovery::SeriesReadingDirection;
use komga_application::media_assets::SeriesMetadataContributionCleanupPort;
use komga_infrastructure_base::RiirDatabase;

use super::SeriesMetadataImportPatch;

const PAYLOAD_FORMAT_VERSION: i64 = 1;
const CONTRIBUTION_LOOKUP_BATCH_SIZE: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SeriesMetadataProvider {
    ComicInfo,
    Epub,
}

impl SeriesMetadataProvider {
    fn persisted_name(self) -> &'static str {
        match self {
            Self::ComicInfo => "COMICINFO",
            Self::Epub => "EPUB",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SeriesMetadataContributionSource {
    pub(super) book_id: String,
    pub(super) file_last_modified_seconds: i64,
    pub(super) file_size: i64,
    pub(super) media_type: String,
    pub(super) media_modified_seconds: i64,
}

pub(super) enum SeriesMetadataContribution {
    ComicInfo {
        plain: Box<SeriesMetadataImportPatch>,
        append_volume: Box<SeriesMetadataImportPatch>,
    },
    Epub {
        patch: Box<SeriesMetadataImportPatch>,
    },
}

pub(super) enum SeriesMetadataContributionOutcome {
    Present(Box<SeriesMetadataContribution>),
    Absent,
}

pub(super) enum ContributionSnapshot {
    Complete(Vec<SeriesMetadataContribution>),
    Incomplete,
}

#[derive(Clone)]
pub struct RiirSeriesMetadataContributionCleanup {
    database: RiirDatabase,
}

impl RiirSeriesMetadataContributionCleanup {
    pub fn new(database: RiirDatabase) -> Self {
        Self { database }
    }
}

#[async_trait::async_trait]
impl SeriesMetadataContributionCleanupPort for RiirSeriesMetadataContributionCleanup {
    async fn delete_book_contributions(&self, book_ids: &[String]) -> anyhow::Result<()> {
        delete_book_contributions(&self.database, book_ids).await
    }
}

struct PersistedContributionRow {
    file_last_modified_seconds: i64,
    file_size: i64,
    media_type: String,
    media_modified_seconds: i64,
    payload_format_version: i64,
    outcome: String,
    payload: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "provider", rename_all = "SCREAMING_SNAKE_CASE")]
enum PersistedContribution {
    ComicInfo {
        plain: Box<PersistedSeriesMetadataPatch>,
        append_volume: Box<PersistedSeriesMetadataPatch>,
    },
    Epub {
        patch: Box<PersistedSeriesMetadataPatch>,
    },
}

impl From<&SeriesMetadataContribution> for PersistedContribution {
    fn from(value: &SeriesMetadataContribution) -> Self {
        match value {
            SeriesMetadataContribution::ComicInfo {
                plain,
                append_volume,
            } => Self::ComicInfo {
                plain: Box::new(plain.as_ref().into()),
                append_volume: Box::new(append_volume.as_ref().into()),
            },
            SeriesMetadataContribution::Epub { patch } => Self::Epub {
                patch: Box::new(patch.as_ref().into()),
            },
        }
    }
}

impl PersistedContribution {
    fn into_contribution(
        self,
        provider: SeriesMetadataProvider,
    ) -> Option<SeriesMetadataContribution> {
        match (provider, self) {
            (
                SeriesMetadataProvider::ComicInfo,
                Self::ComicInfo {
                    plain,
                    append_volume,
                },
            ) => Some(SeriesMetadataContribution::ComicInfo {
                plain: Box::new((*plain).into_patch()?),
                append_volume: Box::new((*append_volume).into_patch()?),
            }),
            (SeriesMetadataProvider::Epub, Self::Epub { patch }) => {
                Some(SeriesMetadataContribution::Epub {
                    patch: Box::new((*patch).into_patch()?),
                })
            }
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct PersistedSeriesMetadataPatch {
    title: Option<String>,
    title_sort: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    reading_direction: Option<String>,
    publisher: Option<String>,
    age_rating: Option<u32>,
    language: Option<String>,
    genres: Option<Vec<String>>,
    total_book_count: Option<u32>,
    collections: Vec<String>,
}

impl From<&SeriesMetadataImportPatch> for PersistedSeriesMetadataPatch {
    fn from(value: &SeriesMetadataImportPatch) -> Self {
        Self {
            title: value.title.clone(),
            title_sort: value.title_sort.clone(),
            status: value.status.clone(),
            summary: value.summary.clone(),
            reading_direction: value
                .reading_direction
                .map(|direction| direction.persisted_name().to_string()),
            publisher: value.publisher.clone(),
            age_rating: value.age_rating,
            language: value.language.clone(),
            genres: value.genres.clone(),
            total_book_count: value.total_book_count,
            collections: value.collections.clone(),
        }
    }
}

impl PersistedSeriesMetadataPatch {
    fn into_patch(self) -> Option<SeriesMetadataImportPatch> {
        let reading_direction = match self.reading_direction.as_deref() {
            Some(value) => Some(SeriesReadingDirection::parse(value)?),
            None => None,
        };
        Some(SeriesMetadataImportPatch {
            title: self.title,
            title_sort: self.title_sort,
            status: self.status,
            summary: self.summary,
            reading_direction,
            publisher: self.publisher,
            age_rating: self.age_rating,
            language: self.language,
            genres: self.genres,
            total_book_count: self.total_book_count,
            collections: self.collections,
        })
    }
}

pub(super) async fn upsert(
    database: &RiirDatabase,
    provider: SeriesMetadataProvider,
    source: &SeriesMetadataContributionSource,
    outcome: SeriesMetadataContributionOutcome,
) -> anyhow::Result<()> {
    let payload;
    let (outcome_name, payload_value): (&str, Option<&str>) = match outcome {
        SeriesMetadataContributionOutcome::Present(contribution) => {
            payload = serde_json::to_string(&PersistedContribution::from(contribution.as_ref()))
                .context("failed to encode series metadata contribution")?;
            ("PRESENT", Some(payload.as_str()))
        }
        SeriesMetadataContributionOutcome::Absent => ("ABSENT", None),
    };
    sqlx::query(
        r#"
            INSERT INTO SERIES_METADATA_CONTRIBUTION (
                BOOK_ID,
                PROVIDER,
                SOURCE_FILE_LAST_MODIFIED_SECONDS,
                SOURCE_FILE_SIZE,
                SOURCE_MEDIA_TYPE,
                SOURCE_MEDIA_MODIFIED_SECONDS,
                PAYLOAD_FORMAT_VERSION,
                OUTCOME,
                PAYLOAD
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (BOOK_ID, PROVIDER) DO UPDATE SET
                SOURCE_FILE_LAST_MODIFIED_SECONDS = excluded.SOURCE_FILE_LAST_MODIFIED_SECONDS,
                SOURCE_FILE_SIZE = excluded.SOURCE_FILE_SIZE,
                SOURCE_MEDIA_TYPE = excluded.SOURCE_MEDIA_TYPE,
                SOURCE_MEDIA_MODIFIED_SECONDS = excluded.SOURCE_MEDIA_MODIFIED_SECONDS,
                PAYLOAD_FORMAT_VERSION = excluded.PAYLOAD_FORMAT_VERSION,
                OUTCOME = excluded.OUTCOME,
                PAYLOAD = excluded.PAYLOAD,
                UPDATED_AT = CURRENT_TIMESTAMP
            "#,
    )
    .bind(&source.book_id)
    .bind(provider.persisted_name())
    .bind(source.file_last_modified_seconds)
    .bind(source.file_size)
    .bind(&source.media_type)
    .bind(source.media_modified_seconds)
    .bind(PAYLOAD_FORMAT_VERSION)
    .bind(outcome_name)
    .bind(payload_value)
    .execute(database.write_pool())
    .await
    .with_context(|| {
        format!(
            "failed to persist {} series metadata contribution for '{}'",
            provider.persisted_name(),
            source.book_id
        )
    })?;
    Ok(())
}

async fn delete_book_contributions(
    database: &RiirDatabase,
    book_ids: &[String],
) -> anyhow::Result<()> {
    for book_ids in book_ids.chunks(CONTRIBUTION_LOOKUP_BATCH_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "DELETE FROM SERIES_METADATA_CONTRIBUTION WHERE BOOK_ID IN (",
        );
        let mut separated = query.separated(", ");
        for book_id in book_ids {
            separated.push_bind(book_id);
        }
        separated.push_unseparated(")");
        query
            .build()
            .execute(database.write_pool())
            .await
            .context("failed to delete series metadata contributions")?;
    }

    Ok(())
}

pub(super) async fn load_complete_snapshot(
    database: &RiirDatabase,
    provider: SeriesMetadataProvider,
    sources: &[SeriesMetadataContributionSource],
) -> anyhow::Result<ContributionSnapshot> {
    let rows = load_rows(database, provider, sources).await?;
    let mut contributions = Vec::new();

    for source in sources {
        let Some(row) = rows.get(&source.book_id) else {
            return Ok(ContributionSnapshot::Incomplete);
        };
        let fresh = row.file_last_modified_seconds == source.file_last_modified_seconds
            && row.file_size == source.file_size
            && row.media_type == source.media_type
            && row.media_modified_seconds == source.media_modified_seconds
            && row.payload_format_version == PAYLOAD_FORMAT_VERSION;
        if !fresh {
            return Ok(ContributionSnapshot::Incomplete);
        }

        match row.outcome.as_str() {
            "ABSENT" => {}
            "PRESENT" => {
                let contribution = row
                    .payload
                    .as_deref()
                    .and_then(|payload| serde_json::from_str(payload).ok())
                    .and_then(|payload: PersistedContribution| payload.into_contribution(provider));
                if let Some(contribution) = contribution {
                    contributions.push(contribution);
                } else {
                    return Ok(ContributionSnapshot::Incomplete);
                }
            }
            _ => return Ok(ContributionSnapshot::Incomplete),
        }
    }

    Ok(ContributionSnapshot::Complete(contributions))
}

async fn load_rows(
    database: &RiirDatabase,
    provider: SeriesMetadataProvider,
    sources: &[SeriesMetadataContributionSource],
) -> anyhow::Result<HashMap<String, PersistedContributionRow>> {
    let mut result = HashMap::with_capacity(sources.len());
    for sources in sources.chunks(CONTRIBUTION_LOOKUP_BATCH_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"
                SELECT BOOK_ID,
                       SOURCE_FILE_LAST_MODIFIED_SECONDS,
                       SOURCE_FILE_SIZE,
                       SOURCE_MEDIA_TYPE,
                       SOURCE_MEDIA_MODIFIED_SECONDS,
                       PAYLOAD_FORMAT_VERSION,
                       OUTCOME,
                       PAYLOAD
                FROM SERIES_METADATA_CONTRIBUTION
                WHERE PROVIDER =
                "#,
        );
        query.push_bind(provider.persisted_name());
        query.push(" AND BOOK_ID IN (");
        let mut separated = query.separated(", ");
        for source in sources {
            separated.push_bind(&source.book_id);
        }
        separated.push_unseparated(")");

        let rows = query
            .build()
            .fetch_all(database.read_pool())
            .await
            .with_context(|| {
                format!(
                    "failed to load {} series metadata contribution batch",
                    provider.persisted_name()
                )
            })?;
        for row in rows {
            result.insert(
                row.get("BOOK_ID"),
                PersistedContributionRow {
                    file_last_modified_seconds: row.get("SOURCE_FILE_LAST_MODIFIED_SECONDS"),
                    file_size: row.get("SOURCE_FILE_SIZE"),
                    media_type: row.get("SOURCE_MEDIA_TYPE"),
                    media_modified_seconds: row.get("SOURCE_MEDIA_MODIFIED_SECONDS"),
                    payload_format_version: row.get("PAYLOAD_FORMAT_VERSION"),
                    outcome: row.get("OUTCOME"),
                    payload: row.get("PAYLOAD"),
                },
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::super::SeriesMetadataImportPatch;
    use super::{
        ContributionSnapshot, RiirDatabase, SeriesMetadataContribution,
        SeriesMetadataContributionOutcome, SeriesMetadataContributionSource,
        SeriesMetadataProvider, delete_book_contributions, load_complete_snapshot, upsert,
    };

    fn riir_db_path(case_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("{case_name}-{}-{nonce}", std::process::id()))
            .join("riir.sqlite")
    }

    fn source(book_id: &str) -> SeriesMetadataContributionSource {
        SeriesMetadataContributionSource {
            book_id: book_id.to_string(),
            file_last_modified_seconds: 10,
            file_size: 20,
            media_type: "application/zip".to_string(),
            media_modified_seconds: 30,
        }
    }

    #[tokio::test]
    async fn absent_contribution_satisfies_complete_snapshot() {
        let path = riir_db_path("absent");
        let store = RiirDatabase::file_backed(&path)
            .await
            .expect("RIIR database should open");
        let source = source("book-1");

        upsert(
            &store,
            SeriesMetadataProvider::ComicInfo,
            &source,
            SeriesMetadataContributionOutcome::Absent,
        )
        .await
        .expect("absent contribution should be persisted");
        let snapshot = load_complete_snapshot(&store, SeriesMetadataProvider::ComicInfo, &[source])
            .await
            .expect("snapshot should load");

        assert!(matches!(snapshot, ContributionSnapshot::Complete(values) if values.is_empty()));
        store.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn changed_source_fingerprint_makes_snapshot_incomplete() {
        let path = riir_db_path("stale");
        let store = RiirDatabase::file_backed(&path)
            .await
            .expect("RIIR database should open");
        let source = source("book-1");
        upsert(
            &store,
            SeriesMetadataProvider::ComicInfo,
            &source,
            SeriesMetadataContributionOutcome::Absent,
        )
        .await
        .expect("absent contribution should be persisted");
        let mut changed_source = source;
        changed_source.file_size += 1;

        let snapshot =
            load_complete_snapshot(&store, SeriesMetadataProvider::ComicInfo, &[changed_source])
                .await
                .expect("snapshot should load");

        assert!(matches!(snapshot, ContributionSnapshot::Incomplete));
        store.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn comicinfo_present_contribution_round_trips_both_title_policies() {
        let path = riir_db_path("comicinfo-present");
        let store = RiirDatabase::file_backed(&path)
            .await
            .expect("RIIR database should open");
        let source = source("book-1");
        let plain = SeriesMetadataImportPatch {
            title: Some("Series".to_string()),
            publisher: Some("Publisher".to_string()),
            ..Default::default()
        };
        let append_volume = SeriesMetadataImportPatch {
            title: Some("Series v01".to_string()),
            publisher: Some("Publisher".to_string()),
            ..Default::default()
        };

        upsert(
            &store,
            SeriesMetadataProvider::ComicInfo,
            &source,
            SeriesMetadataContributionOutcome::Present(Box::new(
                SeriesMetadataContribution::ComicInfo {
                    plain: Box::new(plain),
                    append_volume: Box::new(append_volume),
                },
            )),
        )
        .await
        .expect("ComicInfo contribution should be persisted");
        let snapshot = load_complete_snapshot(&store, SeriesMetadataProvider::ComicInfo, &[source])
            .await
            .expect("snapshot should load");

        let ContributionSnapshot::Complete(values) = snapshot else {
            panic!("fresh present row should produce a complete snapshot");
        };
        let SeriesMetadataContribution::ComicInfo {
            plain,
            append_volume,
        } = &values[0]
        else {
            panic!("ComicInfo provider should return ComicInfo contribution");
        };
        assert_eq!(plain.title.as_deref(), Some("Series"));
        assert_eq!(plain.publisher.as_deref(), Some("Publisher"));
        assert_eq!(append_volume.title.as_deref(), Some("Series v01"));
        store.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn epub_present_contribution_round_trips() {
        let path = riir_db_path("epub-present");
        let store = RiirDatabase::file_backed(&path)
            .await
            .expect("RIIR database should open");
        let source = SeriesMetadataContributionSource {
            media_type: "application/epub+zip".to_string(),
            ..source("book-1")
        };
        let patch = SeriesMetadataImportPatch {
            title: Some("EPUB Series".to_string()),
            language: Some("en".to_string()),
            ..Default::default()
        };

        upsert(
            &store,
            SeriesMetadataProvider::Epub,
            &source,
            SeriesMetadataContributionOutcome::Present(Box::new(
                SeriesMetadataContribution::Epub {
                    patch: Box::new(patch),
                },
            )),
        )
        .await
        .expect("EPUB contribution should be persisted");
        let snapshot = load_complete_snapshot(&store, SeriesMetadataProvider::Epub, &[source])
            .await
            .expect("snapshot should load");

        let ContributionSnapshot::Complete(values) = snapshot else {
            panic!("fresh present row should produce a complete snapshot");
        };
        let SeriesMetadataContribution::Epub { patch } = &values[0] else {
            panic!("EPUB provider should return EPUB contribution");
        };
        assert_eq!(patch.title.as_deref(), Some("EPUB Series"));
        assert_eq!(patch.language.as_deref(), Some("en"));
        store.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn deletes_contributions_in_batches_and_treats_empty_input_as_noop() {
        let path = riir_db_path("delete-batches");
        let store = RiirDatabase::file_backed(&path)
            .await
            .expect("RIIR database should open");

        delete_book_contributions(&store, &[])
            .await
            .expect("empty contribution cleanup should be a no-op");

        let mut transaction = store
            .write_pool()
            .begin()
            .await
            .expect("RIIR seed transaction should begin");
        for index in 0..501 {
            sqlx::query(
                "INSERT INTO SERIES_METADATA_CONTRIBUTION (BOOK_ID, PROVIDER, SOURCE_FILE_LAST_MODIFIED_SECONDS, SOURCE_FILE_SIZE, SOURCE_MEDIA_TYPE, SOURCE_MEDIA_MODIFIED_SECONDS, PAYLOAD_FORMAT_VERSION, OUTCOME) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("book-{index}"))
            .bind("COMICINFO")
            .bind(1_i64)
            .bind(2_i64)
            .bind("application/zip")
            .bind(3_i64)
            .bind(1_i64)
            .bind("ABSENT")
            .execute(&mut *transaction)
            .await
            .expect("RIIR contribution should be seeded");
        }
        transaction
            .commit()
            .await
            .expect("RIIR seed transaction should commit");

        let book_ids = (0..501)
            .map(|index| format!("book-{index}"))
            .collect::<Vec<_>>();
        delete_book_contributions(&store, &book_ids)
            .await
            .expect("batched contribution cleanup should succeed");
        let remaining =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM SERIES_METADATA_CONTRIBUTION")
                .fetch_one(store.read_pool())
                .await
                .expect("remaining contribution count should be queryable");
        assert_eq!(remaining, 0);

        store.close().await;
        let _ = std::fs::remove_file(path);
    }
}
