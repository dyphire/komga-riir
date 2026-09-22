use anyhow::Context;
use komga_application::task_processing::CleanupEmptySetsPolicy;
use komga_domain::discovery::compare_book_names;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

const EMPTY_TRASH_BOOK_DEPENDENCY_SQL: &[&str] = &[
    "DELETE FROM BOOK_METADATA WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_AUTHOR WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_LINK WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_TAG WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM MEDIA WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM MEDIA_FILE WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM MEDIA_PAGE WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM THUMBNAIL_BOOK WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM READ_PROGRESS WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM READLIST_BOOK WHERE BOOK_ID IN (SELECT ID FROM BOOK WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
];

const EMPTY_TRASH_SERIES_DEPENDENCY_SQL: &[&str] = &[
    "DELETE FROM READ_PROGRESS_SERIES WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM COLLECTION_SERIES WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM THUMBNAIL_SERIES WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA_ALTERNATE_TITLE WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA_GENRE WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA_LINK WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA_SHARING WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM SERIES_METADATA_TAG WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_AGGREGATION_AUTHOR WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_AGGREGATION_TAG WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
    "DELETE FROM BOOK_METADATA_AGGREGATION WHERE SERIES_ID IN (SELECT ID FROM SERIES WHERE LIBRARY_ID = ? AND DELETED_DATE IS NOT NULL)",
];

const BOOK_DEPENDENCY_SQL: &[&str] = &[
    "DELETE FROM BOOK_METADATA WHERE BOOK_ID = ?",
    "DELETE FROM BOOK_METADATA_AUTHOR WHERE BOOK_ID = ?",
    "DELETE FROM BOOK_METADATA_LINK WHERE BOOK_ID = ?",
    "DELETE FROM BOOK_METADATA_TAG WHERE BOOK_ID = ?",
    "DELETE FROM MEDIA WHERE BOOK_ID = ?",
    "DELETE FROM MEDIA_FILE WHERE BOOK_ID = ?",
    "DELETE FROM MEDIA_PAGE WHERE BOOK_ID = ?",
    "DELETE FROM THUMBNAIL_BOOK WHERE BOOK_ID = ?",
    "DELETE FROM READ_PROGRESS WHERE BOOK_ID = ?",
    "DELETE FROM READLIST_BOOK WHERE BOOK_ID = ?",
];

const SERIES_DEPENDENCY_SQL: &[&str] = &[
    "DELETE FROM READ_PROGRESS_SERIES WHERE SERIES_ID = ?",
    "DELETE FROM COLLECTION_SERIES WHERE SERIES_ID = ?",
    "DELETE FROM THUMBNAIL_SERIES WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA_ALTERNATE_TITLE WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA_GENRE WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA_LINK WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA_SHARING WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA_TAG WHERE SERIES_ID = ?",
    "DELETE FROM SERIES_METADATA WHERE SERIES_ID = ?",
    "DELETE FROM BOOK_METADATA_AGGREGATION_AUTHOR WHERE SERIES_ID = ?",
    "DELETE FROM BOOK_METADATA_AGGREGATION_TAG WHERE SERIES_ID = ?",
    "DELETE FROM BOOK_METADATA_AGGREGATION WHERE SERIES_ID = ?",
];

pub async fn delete_book_dependency_rows(pool: &SqlitePool, book_id: &str) -> anyhow::Result<()> {
    for sql in BOOK_DEPENDENCY_SQL {
        sqlx::query(*sql)
            .bind(book_id)
            .execute(pool)
            .await
            .with_context(|| format!("delete book dependency rows for '{book_id}'"))?;
    }
    Ok(())
}

pub async fn delete_series_dependency_rows(
    pool: &SqlitePool,
    series_id: &str,
) -> anyhow::Result<()> {
    for sql in SERIES_DEPENDENCY_SQL {
        sqlx::query(*sql)
            .bind(series_id)
            .execute(pool)
            .await
            .with_context(|| format!("delete series dependency rows for '{series_id}'"))?;
    }
    Ok(())
}

pub async fn empty_trash_rows(pool: &SqlitePool, library_id: &str) -> anyhow::Result<Vec<String>> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start empty-trash transaction")?;

    let affected_book_ids = load_empty_trash_book_ids(&mut tx, library_id)
        .await
        .with_context(|| {
            format!("failed to load deleted books for empty-trash library '{library_id}'")
        })?;
    let affected_series_ids = load_empty_trash_affected_series_ids(&mut tx, library_id)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to load affected series for empty-trash library '{library_id}': "
            ))
        })?;

    for sql in EMPTY_TRASH_BOOK_DEPENDENCY_SQL {
        sqlx::query(*sql)
            .bind(library_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to delete empty-trash dependent rows for library '{library_id}': "
                ))
            })?;
    }

    sqlx::query(
        r#"
        DELETE FROM BOOK
        WHERE LIBRARY_ID = ?
        AND DELETED_DATE IS NOT NULL
        "#,
    )
    .bind(library_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to delete trashed BOOK rows for library '{library_id}': "
        ))
    })?;

    sqlx::query(
        r#"
        UPDATE SERIES
        SET BOOK_COUNT = (
        SELECT COUNT(*)
        FROM BOOK
        WHERE BOOK.SERIES_ID = SERIES.ID
        )
        WHERE LIBRARY_ID = ?
        "#,
    )
    .bind(library_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to refresh SERIES book counts for library '{library_id}': "
        ))
    })?;

    for sql in EMPTY_TRASH_SERIES_DEPENDENCY_SQL {
        sqlx::query(*sql)
            .bind(library_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to delete empty-trash SERIES dependents for library '{library_id}': "
                ))
            })?;
    }

    sqlx::query(
        r#"
        DELETE FROM SERIES
        WHERE LIBRARY_ID = ?
        AND DELETED_DATE IS NOT NULL
        "#,
    )
    .bind(library_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to delete trashed SERIES rows for library '{library_id}': "
        ))
    })?;

    resort_empty_trash_affected_series(&mut tx, &affected_series_ids)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to resort affected series after empty-trash for library '{library_id}': "
            ))
        })?;

    tx.commit().await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to commit empty-trash transaction for library '{library_id}': "
        ))
    })?;

    Ok(affected_book_ids)
}

async fn load_empty_trash_book_ids(
    tx: &mut Transaction<'_, Sqlite>,
    library_id: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
        SELECT ID
        FROM BOOK
        WHERE LIBRARY_ID = ?
          AND DELETED_DATE IS NOT NULL
        ORDER BY ID ASC
        "#,
    )
    .bind(library_id)
    .fetch_all(&mut **tx)
    .await
    .context("query deleted book ids")?;

    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("ID"))
        .collect())
}

async fn load_empty_trash_affected_series_ids(
    tx: &mut Transaction<'_, Sqlite>,
    library_id: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT SERIES_ID
        FROM BOOK
        WHERE LIBRARY_ID = ?
          AND DELETED_DATE IS NOT NULL
        ORDER BY SERIES_ID ASC
        "#,
    )
    .bind(library_id)
    .fetch_all(&mut **tx)
    .await
    .context("query affected series ids")?;

    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("SERIES_ID"))
        .collect())
}

async fn resort_empty_trash_affected_series(
    tx: &mut Transaction<'_, Sqlite>,
    series_ids: &[String],
) -> anyhow::Result<()> {
    for series_id in series_ids {
        let exists = sqlx::query(
            r#"
            SELECT 1 AS FOUND
            FROM SERIES
            WHERE ID = ?
              AND DELETED_DATE IS NULL
            LIMIT 1
            "#,
        )
        .bind(series_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!("query affected series existence '{series_id}'"))
        })?
        .is_some();
        if !exists {
            continue;
        }

        let book_rows = sqlx::query(
            r#"
            SELECT b.ID AS BOOK_ID,
                   b.NAME AS BOOK_NAME,
                   b.NUMBER AS BOOK_NUMBER,
                   bm.BOOK_ID AS BOOK_METADATA_BOOK_ID,
                   bm.NUMBER AS METADATA_NUMBER,
                   bm.NUMBER_SORT AS METADATA_NUMBER_SORT,
                   bm.NUMBER_LOCK AS METADATA_NUMBER_LOCK,
                   bm.NUMBER_SORT_LOCK AS METADATA_NUMBER_SORT_LOCK
            FROM BOOK b
            LEFT JOIN BOOK_METADATA bm ON bm.BOOK_ID = b.ID
            WHERE b.SERIES_ID = ?
              AND b.DELETED_DATE IS NULL
            ORDER BY b.ID ASC
            "#,
        )
        .bind(series_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!("query affected series books '{series_id}'"))
        })?;

        let mut books = book_rows
            .into_iter()
            .map(|row| {
                let id = row.get::<String, _>("BOOK_ID");
                Ok(EmptyTrashSortableBook {
                    metadata: empty_trash_book_metadata(&row, &id)?,
                    id,
                    name: row.get::<String, _>("BOOK_NAME"),
                    number: row.get::<i64, _>("BOOK_NUMBER"),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        books.sort_by(|left, right| {
            compare_book_names(&left.name, &right.name).then_with(|| left.id.cmp(&right.id))
        });

        for (index, book) in books.iter().enumerate() {
            let new_number = index as i64 + 1;

            if book.number != new_number {
                sqlx::query(
                    r#"
                    UPDATE BOOK
                    SET NUMBER = ?,
                        LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
                    WHERE ID = ?
                    "#,
                )
                .bind(new_number)
                .bind(&book.id)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!("update book order '{}'", book.id))
                })?;
            }

            if let Some(metadata) = &book.metadata {
                let metadata_number = if metadata.number_lock {
                    metadata.number.clone()
                } else {
                    Some(new_number.to_string())
                };
                let metadata_number_sort = if metadata.number_sort_lock {
                    metadata.number_sort
                } else {
                    Some(new_number as f64)
                };

                sqlx::query(
                    r#"
                    UPDATE BOOK_METADATA
                    SET NUMBER = ?,
                        NUMBER_SORT = ?,
                        LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
                    WHERE BOOK_ID = ?
                    "#,
                )
                .bind(metadata_number)
                .bind(metadata_number_sort)
                .bind(&book.id)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context(format!("update book metadata order '{}'", book.id))
                })?;
            }
        }
    }

    Ok(())
}

struct EmptyTrashSortableBook {
    id: String,
    name: String,
    number: i64,
    metadata: Option<EmptyTrashBookMetadata>,
}

struct EmptyTrashBookMetadata {
    number: Option<String>,
    number_sort: Option<f64>,
    number_lock: bool,
    number_sort_lock: bool,
}

fn empty_trash_book_metadata(
    row: &SqliteRow,
    book_id: &str,
) -> anyhow::Result<Option<EmptyTrashBookMetadata>> {
    let metadata_exists = row
        .try_get::<Option<String>, _>("BOOK_METADATA_BOOK_ID")
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "persisted BOOK_METADATA row marker could not be read for '{book_id}': "
            ))
        })?
        .is_some();
    if !metadata_exists {
        return Ok(None);
    }

    Ok(Some(EmptyTrashBookMetadata {
        number: row
            .try_get::<Option<String>, _>("METADATA_NUMBER")
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "persisted BOOK_METADATA.NUMBER could not be read for '{book_id}': "
                ))
            })?,
        number_sort: row
            .try_get::<Option<f64>, _>("METADATA_NUMBER_SORT")
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "persisted BOOK_METADATA.NUMBER_SORT could not be read for '{book_id}': "
                ))
            })?,
        number_lock: row.get::<bool, _>("METADATA_NUMBER_LOCK"),
        number_sort_lock: row.get::<bool, _>("METADATA_NUMBER_SORT_LOCK"),
    }))
}

pub async fn cleanup_empty_sets_rows(
    pool: &SqlitePool,
    policy: CleanupEmptySetsPolicy,
) -> anyhow::Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("failed to start cleanup-empty-sets transaction")?;

    let mut deletes = Vec::<&str>::new();
    if policy.delete_empty_collections {
        deletes.push(
            "DELETE FROM THUMBNAIL_COLLECTION WHERE COLLECTION_ID IN (SELECT ID FROM COLLECTION WHERE ID NOT IN (SELECT COLLECTION_ID FROM COLLECTION_SERIES))",
        );
        deletes.push(
            "DELETE FROM COLLECTION WHERE ID NOT IN (SELECT COLLECTION_ID FROM COLLECTION_SERIES)",
        );
    }
    if policy.delete_empty_read_lists {
        deletes.push(
            "DELETE FROM THUMBNAIL_READLIST WHERE READLIST_ID IN (SELECT ID FROM READLIST WHERE ID NOT IN (SELECT READLIST_ID FROM READLIST_BOOK))",
        );
        deletes
            .push("DELETE FROM READLIST WHERE ID NOT IN (SELECT READLIST_ID FROM READLIST_BOOK)");
    }

    for sql in deletes {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .context("failed to cleanup empty sets rows")?;
    }

    tx.commit()
        .await
        .context("failed to commit cleanup-empty-sets transaction")?;

    Ok(())
}
