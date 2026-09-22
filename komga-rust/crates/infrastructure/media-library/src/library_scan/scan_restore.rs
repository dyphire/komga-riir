use anyhow::Context;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use tokio::task;

use komga_domain::media_assets::ThumbnailType;

use komga_infrastructure_base::stored_paths::resolve_rooted_path;
use komga_infrastructure_discovery::delete_book_dependency_rows;

use super::scan_models::{
    BookMetadataRefreshRequest, InsertedBookCandidate, InsertedSeriesCandidate,
    RestoredBookMatches, RestoredSeriesMatch,
};

fn compute_file_sha256(path: PathBuf) -> anyhow::Result<String> {
    let file = std::fs::File::open(&path).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to open book file for restore '{}'",
            path.display()
        ))
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let bytes_read = reader.read(&mut buffer).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to read book file for restore '{}'",
                path.display()
            ))
        })?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect::<String>())
}

async fn compute_file_sha256_blocking(path: PathBuf) -> anyhow::Result<String> {
    task::spawn_blocking(move || compute_file_sha256(path))
        .await
        .context("file hashing task panicked")?
}
pub(super) async fn try_restore_deleted_books(
    pool: &SqlitePool,
    library_root: &Path,
    inserted_books: &[InsertedBookCandidate],
) -> anyhow::Result<RestoredBookMatches> {
    let mut restored_series_ids = HashSet::<String>::new();
    let mut book_metadata_refreshes = Vec::<BookMetadataRefreshRequest>::new();

    for inserted in inserted_books {
        let deleted_candidates = sqlx::query(
            r#"SELECT ID, FILE_HASH
FROM BOOK
WHERE DELETED_DATE IS NOT NULL
  AND FILE_SIZE = ?
  AND COALESCE(FILE_HASH, '') <> ''
ORDER BY ID ASC"#,
        )
        .bind(inserted.file_size)
        .fetch_all(pool)
        .await
        .context("failed to load deleted book restore candidates")?;
        if deleted_candidates.is_empty() {
            continue;
        }

        let inserted_hash = compute_file_sha256_blocking(resolve_rooted_path(
            library_root,
            &inserted.book_url,
        ))
        .await?;
        sqlx::query(
            r#"UPDATE BOOK
SET FILE_HASH = ?, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
WHERE ID = ?"#,
        )
        .bind(&inserted_hash)
        .bind(&inserted.book_id)
        .execute(pool)
        .await
        .context("failed to persist inserted book hash during restore: ")?;

        let Some(matched_deleted_book_id) = deleted_candidates.into_iter().find_map(|row| {
            let file_hash = row.get::<String, _>("FILE_HASH");
            (file_hash == inserted_hash).then(|| row.get::<String, _>("ID"))
        }) else {
            continue;
        };

        sqlx::query(
            r#"UPDATE MEDIA
SET BOOK_ID = ?
WHERE BOOK_ID = ?"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore MEDIA rows for '{}'",
                inserted.book_id
            ))
        })?;
        sqlx::query(
            r#"UPDATE MEDIA_FILE
SET BOOK_ID = ?
WHERE BOOK_ID = ?"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore MEDIA_FILE rows for '{}'",
                inserted.book_id
            ))
        })?;
        sqlx::query(
            r#"UPDATE MEDIA_PAGE
SET BOOK_ID = ?
WHERE BOOK_ID = ?"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore MEDIA_PAGE rows for '{}'",
                inserted.book_id
            ))
        })?;
        sqlx::query(
            r#"UPDATE THUMBNAIL_BOOK
SET BOOK_ID = ?
WHERE BOOK_ID = ?
  AND TYPE IN (?, ?)"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .bind(ThumbnailType::Generated.persisted_name())
        .bind(ThumbnailType::UserUploaded.persisted_name())
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore THUMBNAIL_BOOK rows for '{}'",
                inserted.book_id
            ))
        })?;
        sqlx::query(
            r#"UPDATE READ_PROGRESS
SET BOOK_ID = ?
WHERE BOOK_ID = ?"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore READ_PROGRESS rows for '{}'",
                inserted.book_id
            ))
        })?;
        sqlx::query(
            r#"UPDATE READLIST_BOOK
SET BOOK_ID = ?
WHERE BOOK_ID = ?"#,
        )
        .bind(&inserted.book_id)
        .bind(&matched_deleted_book_id)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to restore READLIST_BOOK rows for '{}'",
                inserted.book_id
            ))
        })?;

        let metadata_row = sqlx::query(
            r#"SELECT TITLE, TITLE_LOCK, SUMMARY, SUMMARY_LOCK, NUMBER, NUMBER_LOCK, NUMBER_SORT,
       NUMBER_SORT_LOCK, RELEASE_DATE, RELEASE_DATE_LOCK, AUTHORS_LOCK, TAGS_LOCK, ISBN,
       ISBN_LOCK, LINKS_LOCK
FROM BOOK_METADATA
WHERE BOOK_ID = ?
LIMIT 1"#,
        )
        .bind(&matched_deleted_book_id)
        .fetch_optional(pool)
        .await
        .context("failed to load deleted BOOK_METADATA for restore: ")?;
        let inserted_metadata_row =
            sqlx::query(r#"SELECT TITLE FROM BOOK_METADATA WHERE BOOK_ID = ? LIMIT 1"#)
                .bind(&inserted.book_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to load inserted BOOK_METADATA for restore: ")
                })?;
        if let (Some(deleted_metadata), Some(inserted_metadata)) =
            (metadata_row, inserted_metadata_row)
        {
            let deleted_title_locked = deleted_metadata.get::<bool, _>("TITLE_LOCK");
            sqlx::query(
                r#"UPDATE BOOK_METADATA
SET TITLE = ?, TITLE_LOCK = ?, SUMMARY = ?, SUMMARY_LOCK = ?, NUMBER = ?, NUMBER_LOCK = ?,
    NUMBER_SORT = ?, NUMBER_SORT_LOCK = ?, RELEASE_DATE = ?, RELEASE_DATE_LOCK = ?,
    AUTHORS_LOCK = ?, TAGS_LOCK = ?, ISBN = ?, ISBN_LOCK = ?, LINKS_LOCK = ?,
    LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
WHERE BOOK_ID = ?"#,
            )
            .bind(if deleted_title_locked {
                deleted_metadata.get::<String, _>("TITLE")
            } else {
                inserted_metadata.get::<String, _>("TITLE")
            })
            .bind(deleted_title_locked)
            .bind(deleted_metadata.get::<String, _>("SUMMARY"))
            .bind(deleted_metadata.get::<bool, _>("SUMMARY_LOCK"))
            .bind(deleted_metadata.get::<String, _>("NUMBER"))
            .bind(deleted_metadata.get::<bool, _>("NUMBER_LOCK"))
            .bind(deleted_metadata.get::<f64, _>("NUMBER_SORT"))
            .bind(deleted_metadata.get::<bool, _>("NUMBER_SORT_LOCK"))
            .bind(deleted_metadata.get::<Option<String>, _>("RELEASE_DATE"))
            .bind(deleted_metadata.get::<bool, _>("RELEASE_DATE_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("AUTHORS_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("TAGS_LOCK"))
            .bind(deleted_metadata.get::<String, _>("ISBN"))
            .bind(deleted_metadata.get::<bool, _>("ISBN_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("LINKS_LOCK"))
            .bind(&inserted.book_id)
            .execute(pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to restore BOOK_METADATA row for '{}'",
                    inserted.book_id
                ))
            })?;
            if !deleted_title_locked {
                book_metadata_refreshes.push(BookMetadataRefreshRequest {
                    book_id: inserted.book_id.clone(),
                    series_id: inserted.series_id.clone(),
                    capabilities: vec!["TITLE".to_string()],
                });
            }
            sqlx::query("DELETE FROM BOOK_METADATA_AUTHOR WHERE BOOK_ID = ?")
                .bind(&inserted.book_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to clear BOOK_METADATA_AUTHOR rows during restore: ")
                })?;
            sqlx::query(
                r#"INSERT INTO BOOK_METADATA_AUTHOR (BOOK_ID, NAME, ROLE)
SELECT ?, NAME, ROLE FROM BOOK_METADATA_AUTHOR WHERE BOOK_ID = ?"#,
            )
            .bind(&inserted.book_id)
            .bind(&matched_deleted_book_id)
            .execute(pool)
            .await
            .context("failed to restore BOOK_METADATA_AUTHOR rows")?;
            sqlx::query("DELETE FROM BOOK_METADATA_TAG WHERE BOOK_ID = ?")
                .bind(&inserted.book_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to clear BOOK_METADATA_TAG rows during restore: ")
                })?;
            sqlx::query(
                r#"INSERT INTO BOOK_METADATA_TAG (BOOK_ID, TAG)
SELECT ?, TAG FROM BOOK_METADATA_TAG WHERE BOOK_ID = ?"#,
            )
            .bind(&inserted.book_id)
            .bind(&matched_deleted_book_id)
            .execute(pool)
            .await
            .context("failed to restore BOOK_METADATA_TAG rows")?;
            sqlx::query("DELETE FROM BOOK_METADATA_LINK WHERE BOOK_ID = ?")
                .bind(&inserted.book_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to clear BOOK_METADATA_LINK rows during restore: ")
                })?;
            sqlx::query(
                r#"INSERT INTO BOOK_METADATA_LINK (BOOK_ID, LABEL, URL)
SELECT ?, LABEL, URL FROM BOOK_METADATA_LINK WHERE BOOK_ID = ?"#,
            )
            .bind(&inserted.book_id)
            .bind(&matched_deleted_book_id)
            .execute(pool)
            .await
            .context("failed to restore BOOK_METADATA_LINK rows")?;
        }

        delete_book_dependency_rows(pool, &matched_deleted_book_id)
            .await
            .context("failed to delete restored legacy book dependencies")?;
        sqlx::query("DELETE FROM BOOK WHERE ID = ?")
            .bind(&matched_deleted_book_id)
            .execute(pool)
            .await
            .context("failed to delete restored legacy BOOK row")?;

        let progress_user_rows = sqlx::query(
            "SELECT DISTINCT USER_ID FROM READ_PROGRESS WHERE BOOK_ID = ? ORDER BY USER_ID ASC",
        )
        .bind(&inserted.book_id)
        .fetch_all(pool)
        .await
        .context("failed to load restored READ_PROGRESS users")?;
        for row in progress_user_rows {
            let user_id = row.get::<String, _>("USER_ID");
            let aggregate = sqlx::query(
                r#"SELECT COUNT(rp.BOOK_ID) AS PROGRESS_COUNT,
       COALESCE(SUM(CASE WHEN rp.COMPLETED = 1 THEN 1 ELSE 0 END), 0) AS READ_COUNT,
       COALESCE(SUM(CASE WHEN rp.COMPLETED = 0 THEN 1 ELSE 0 END), 0) AS IN_PROGRESS_COUNT,
       MAX(rp.READ_DATE) AS MOST_RECENT_READ_DATE
FROM BOOK b
LEFT JOIN READ_PROGRESS rp ON rp.BOOK_ID = b.ID AND rp.USER_ID = ?
WHERE b.SERIES_ID = ? AND b.DELETED_DATE IS NULL"#,
            )
            .bind(&user_id)
            .bind(&inserted.series_id)
            .fetch_one(pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error)
                    .context("failed to aggregate restored READ_PROGRESS_SERIES rows: ")
            })?;
            let progress_count = aggregate.get::<i64, _>("PROGRESS_COUNT");
            if progress_count == 0 {
                sqlx::query("DELETE FROM READ_PROGRESS_SERIES WHERE SERIES_ID = ? AND USER_ID = ?")
                    .bind(&inserted.series_id)
                    .bind(&user_id)
                    .execute(pool)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(error).context(
                            "failed to delete empty READ_PROGRESS_SERIES row after restore: ",
                        )
                    })?;
            } else {
                sqlx::query(
                    r#"INSERT INTO READ_PROGRESS_SERIES (SERIES_ID, USER_ID, READ_COUNT, IN_PROGRESS_COUNT, MOST_RECENT_READ_DATE, LAST_MODIFIED_DATE)
VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP)
ON CONFLICT(SERIES_ID, USER_ID) DO UPDATE
SET READ_COUNT = excluded.READ_COUNT,
    IN_PROGRESS_COUNT = excluded.IN_PROGRESS_COUNT,
    MOST_RECENT_READ_DATE = excluded.MOST_RECENT_READ_DATE,
    LAST_MODIFIED_DATE = CURRENT_TIMESTAMP"#,
                )
                .bind(&inserted.series_id)
                .bind(&user_id)
                .bind(aggregate.get::<i64, _>("READ_COUNT"))
                .bind(aggregate.get::<i64, _>("IN_PROGRESS_COUNT"))
                .bind(aggregate.get::<Option<String>, _>("MOST_RECENT_READ_DATE"))
                .execute(pool)
                .await
                .context("failed to upsert READ_PROGRESS_SERIES row after restore")?;
            }
        }

        restored_series_ids.insert(inserted.series_id.clone());
    }

    Ok(RestoredBookMatches {
        series_ids: restored_series_ids.into_iter().collect(),
        book_metadata_refreshes,
    })
}

pub(super) async fn try_restore_deleted_series(
    pool: &SqlitePool,
    library_root: &Path,
    inserted_series: &[InsertedSeriesCandidate],
) -> anyhow::Result<Vec<RestoredSeriesMatch>> {
    let mut restored_series_ids = Vec::new();

    for inserted in inserted_series {
        if inserted.books.is_empty() {
            continue;
        }

        let deleted_series_rows = sqlx::query(
            r#"SELECT s.ID AS ID
FROM SERIES s
WHERE s.DELETED_DATE IS NOT NULL
ORDER BY s.ID ASC"#,
        )
        .fetch_all(pool)
        .await
        .context("failed to load deleted series restore candidates: ")?;
        if deleted_series_rows.is_empty() {
            continue;
        }

        let mut inserted_books_with_hash = Vec::<(InsertedBookCandidate, String)>::new();
        for book in &inserted.books {
            let book_hash = compute_file_sha256_blocking(resolve_rooted_path(
                library_root,
                &book.book_url,
            ))
            .await?;
            inserted_books_with_hash.push((book.clone(), book_hash));
        }

        let mut matched_deleted_series_id = None::<String>;
        for deleted_series_row in deleted_series_rows {
            let deleted_series_id = deleted_series_row.get::<String, _>("ID");
            let deleted_books = sqlx::query(
                r#"SELECT ID, FILE_SIZE, FILE_HASH
FROM BOOK
WHERE SERIES_ID = ?
ORDER BY ID ASC"#,
            )
            .bind(&deleted_series_id)
            .fetch_all(pool)
            .await
            .context("failed to load deleted series books for restore")?;
            if deleted_books.len() != inserted_books_with_hash.len() {
                continue;
            }

            let deleted_sizes = deleted_books
                .iter()
                .map(|row| row.get::<i64, _>("FILE_SIZE"))
                .collect::<Vec<_>>();
            let inserted_sizes = inserted_books_with_hash
                .iter()
                .map(|(book, _)| book.file_size)
                .collect::<Vec<_>>();
            let mut deleted_sizes_sorted = deleted_sizes.clone();
            let mut inserted_sizes_sorted = inserted_sizes.clone();
            deleted_sizes_sorted.sort();
            inserted_sizes_sorted.sort();
            if deleted_sizes_sorted != inserted_sizes_sorted {
                continue;
            }

            let deleted_hashes = deleted_books
                .iter()
                .map(|row| row.get::<String, _>("FILE_HASH"))
                .collect::<Vec<_>>();
            let inserted_hashes = inserted_books_with_hash
                .iter()
                .map(|(_, hash)| hash.clone())
                .collect::<Vec<_>>();
            let mut deleted_hashes_sorted = deleted_hashes.clone();
            let mut inserted_hashes_sorted = inserted_hashes.clone();
            deleted_hashes_sorted.sort();
            inserted_hashes_sorted.sort();
            if deleted_hashes_sorted != inserted_hashes_sorted {
                continue;
            }

            matched_deleted_series_id = Some(deleted_series_id);
            break;
        }

        let Some(deleted_series_id) = matched_deleted_series_id else {
            continue;
        };

        let deleted_series_metadata = sqlx::query(
            r#"SELECT STATUS, STATUS_LOCK, TITLE, TITLE_LOCK, TITLE_SORT, TITLE_SORT_LOCK, SUMMARY,
       SUMMARY_LOCK, READING_DIRECTION, READING_DIRECTION_LOCK, PUBLISHER, PUBLISHER_LOCK,
       AGE_RATING, AGE_RATING_LOCK, LANGUAGE, LANGUAGE_LOCK, GENRES_LOCK, TAGS_LOCK,
       TOTAL_BOOK_COUNT, TOTAL_BOOK_COUNT_LOCK, SHARING_LABELS_LOCK, LINKS_LOCK,
       ALTERNATE_TITLES_LOCK
FROM SERIES_METADATA
WHERE SERIES_ID = ?
LIMIT 1"#,
        )
        .bind(&deleted_series_id)
        .fetch_optional(pool)
        .await
        .context("failed to load deleted SERIES_METADATA for restore: ")?;
        let inserted_series_metadata = sqlx::query(
            r#"SELECT TITLE, TITLE_SORT FROM SERIES_METADATA WHERE SERIES_ID = ? LIMIT 1"#,
        )
        .bind(&inserted.series_id)
        .fetch_optional(pool)
        .await
        .context("failed to load inserted SERIES_METADATA for restore: ")?;
        if let (Some(deleted_metadata), Some(inserted_metadata)) =
            (deleted_series_metadata, inserted_series_metadata)
        {
            sqlx::query(
                r#"UPDATE SERIES
SET NAME = ?, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
WHERE ID = ?"#,
            )
            .bind(&inserted.series_title)
            .bind(&inserted.series_id)
            .execute(pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to touch restored SERIES row for '{}'",
                    inserted.series_id
                ))
            })?;
            sqlx::query(
                r#"UPDATE SERIES_METADATA
SET STATUS = ?, STATUS_LOCK = ?, TITLE = ?, TITLE_LOCK = ?, TITLE_SORT = ?, TITLE_SORT_LOCK = ?,
    SUMMARY = ?, SUMMARY_LOCK = ?, READING_DIRECTION = ?, READING_DIRECTION_LOCK = ?,
    PUBLISHER = ?, PUBLISHER_LOCK = ?, AGE_RATING = ?, AGE_RATING_LOCK = ?, LANGUAGE = ?,
    LANGUAGE_LOCK = ?, GENRES_LOCK = ?, TAGS_LOCK = ?, TOTAL_BOOK_COUNT = ?, TOTAL_BOOK_COUNT_LOCK = ?,
    SHARING_LABELS_LOCK = ?, LINKS_LOCK = ?, ALTERNATE_TITLES_LOCK = ?, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
WHERE SERIES_ID = ?"#,
            )
            .bind(deleted_metadata.get::<String, _>("STATUS"))
            .bind(deleted_metadata.get::<bool, _>("STATUS_LOCK"))
            .bind(if deleted_metadata.get::<bool, _>("TITLE_LOCK") {
                deleted_metadata.get::<String, _>("TITLE")
            } else {
                inserted_metadata.get::<String, _>("TITLE")
            })
            .bind(deleted_metadata.get::<bool, _>("TITLE_LOCK"))
            .bind(if deleted_metadata.get::<bool, _>("TITLE_SORT_LOCK") {
                deleted_metadata.get::<String, _>("TITLE_SORT")
            } else {
                inserted_metadata.get::<String, _>("TITLE_SORT")
            })
            .bind(deleted_metadata.get::<bool, _>("TITLE_SORT_LOCK"))
            .bind(deleted_metadata.get::<String, _>("SUMMARY"))
            .bind(deleted_metadata.get::<bool, _>("SUMMARY_LOCK"))
            .bind(deleted_metadata.get::<Option<String>, _>("READING_DIRECTION"))
            .bind(deleted_metadata.get::<bool, _>("READING_DIRECTION_LOCK"))
            .bind(deleted_metadata.get::<String, _>("PUBLISHER"))
            .bind(deleted_metadata.get::<bool, _>("PUBLISHER_LOCK"))
            .bind(deleted_metadata.get::<Option<i64>, _>("AGE_RATING"))
            .bind(deleted_metadata.get::<bool, _>("AGE_RATING_LOCK"))
            .bind(deleted_metadata.get::<String, _>("LANGUAGE"))
            .bind(deleted_metadata.get::<bool, _>("LANGUAGE_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("GENRES_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("TAGS_LOCK"))
            .bind(deleted_metadata.get::<Option<i64>, _>("TOTAL_BOOK_COUNT"))
            .bind(deleted_metadata.get::<bool, _>("TOTAL_BOOK_COUNT_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("SHARING_LABELS_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("LINKS_LOCK"))
            .bind(deleted_metadata.get::<bool, _>("ALTERNATE_TITLES_LOCK"))
            .bind(&inserted.series_id)
            .execute(pool)
            .await
            .map_err(|error| anyhow::anyhow!(error).context( format!("failed to restore SERIES_METADATA row for '{}'", inserted.series_id)))?;
            for table in [
                "SERIES_METADATA_GENRE",
                "SERIES_METADATA_TAG",
                "SERIES_METADATA_SHARING",
            ] {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE SERIES_ID = ?"
                )))
                .bind(&inserted.series_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to clear restored series metadata strings: ")
                })?;
            }
            sqlx::query(
                r#"INSERT INTO SERIES_METADATA_GENRE (SERIES_ID, GENRE)
SELECT ?, GENRE FROM SERIES_METADATA_GENRE WHERE SERIES_ID = ?"#,
            )
            .bind(&inserted.series_id)
            .bind(&deleted_series_id)
            .execute(pool)
            .await
            .context("failed to restore SERIES_METADATA_GENRE rows")?;
            sqlx::query(
                r#"INSERT INTO SERIES_METADATA_TAG (SERIES_ID, TAG)
SELECT ?, TAG FROM SERIES_METADATA_TAG WHERE SERIES_ID = ?"#,
            )
            .bind(&inserted.series_id)
            .bind(&deleted_series_id)
            .execute(pool)
            .await
            .context("failed to restore SERIES_METADATA_TAG rows")?;
            sqlx::query(
                r#"INSERT INTO SERIES_METADATA_SHARING (SERIES_ID, LABEL)
SELECT ?, LABEL FROM SERIES_METADATA_SHARING WHERE SERIES_ID = ?"#,
            )
            .bind(&inserted.series_id)
            .bind(&deleted_series_id)
            .execute(pool)
            .await
            .context("failed to restore SERIES_METADATA_SHARING rows")?;
            sqlx::query("DELETE FROM SERIES_METADATA_LINK WHERE SERIES_ID = ?")
                .bind(&inserted.series_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context("failed to clear SERIES_METADATA_LINK rows during restore: ")
                })?;
            sqlx::query(
                r#"INSERT INTO SERIES_METADATA_LINK (SERIES_ID, LABEL, URL)
SELECT ?, LABEL, URL FROM SERIES_METADATA_LINK WHERE SERIES_ID = ?"#,
            )
            .bind(&inserted.series_id)
            .bind(&deleted_series_id)
            .execute(pool)
            .await
            .context("failed to restore SERIES_METADATA_LINK rows")?;
            sqlx::query("DELETE FROM SERIES_METADATA_ALTERNATE_TITLE WHERE SERIES_ID = ?")
                .bind(&inserted.series_id)
                .execute(pool)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error).context(
                        "failed to clear SERIES_METADATA_ALTERNATE_TITLE rows during restore: ",
                    )
                })?;
            sqlx::query(
                r#"INSERT INTO SERIES_METADATA_ALTERNATE_TITLE (SERIES_ID, LABEL, TITLE)
SELECT ?, LABEL, TITLE FROM SERIES_METADATA_ALTERNATE_TITLE WHERE SERIES_ID = ?"#,
            )
            .bind(&inserted.series_id)
            .bind(&deleted_series_id)
            .execute(pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error)
                    .context("failed to restore SERIES_METADATA_ALTERNATE_TITLE rows: ")
            })?;
        }

        sqlx::query(
            r#"UPDATE THUMBNAIL_SERIES
SET SERIES_ID = ?
WHERE SERIES_ID = ?
  AND TYPE = ?"#,
        )
        .bind(&inserted.series_id)
        .bind(&deleted_series_id)
        .bind(ThumbnailType::UserUploaded.persisted_name())
        .execute(pool)
        .await
        .context("failed to restore THUMBNAIL_SERIES rows")?;
        sqlx::query(
            r#"UPDATE COLLECTION_SERIES
SET SERIES_ID = ?
WHERE SERIES_ID = ?"#,
        )
        .bind(&inserted.series_id)
        .bind(&deleted_series_id)
        .execute(pool)
        .await
        .context("failed to restore COLLECTION_SERIES rows")?;

        restored_series_ids.push(RestoredSeriesMatch {
            inserted_series_id: inserted.series_id.clone(),
            deleted_series_id: deleted_series_id.clone(),
        });
    }

    Ok(restored_series_ids)
}
