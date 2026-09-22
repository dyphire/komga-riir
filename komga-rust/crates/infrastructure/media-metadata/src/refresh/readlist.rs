use anyhow::Context;
use komga_application::runtime_sse::RuntimeSseEventSink;
use sqlx::{Row, SqlitePool};

use super::events;
use super::support::generated_readlist_id;

pub(super) struct ComicInfoReadListEntry {
    pub(super) name: String,
    pub(super) number: Option<i64>,
}

struct ComicInfoReadListTarget {
    id: String,
    created: bool,
}

pub(super) async fn upsert_comicinfo_readlist(
    pool: &SqlitePool,
    runtime_events: &dyn RuntimeSseEventSink,
    book_id: &str,
    readlist: ComicInfoReadListEntry,
) -> anyhow::Result<Option<String>> {
    let readlist_id = sqlx::query("SELECT ID FROM READLIST WHERE NAME = ? LIMIT 1")
        .bind(&readlist.name)
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to load readlist '{}' for '{}'",
                readlist.name, book_id
            ))
        })?
        .map(|row| row.get::<String, _>("ID"));

    let target = match readlist_id {
        Some(readlist_id) => ComicInfoReadListTarget {
            id: readlist_id,
            created: false,
        },
        None => {
            let generated_id = generated_readlist_id(&readlist.name);
            sqlx::query(
                "INSERT INTO READLIST (ID, NAME, BOOK_COUNT, SUMMARY, ORDERED) VALUES (?, ?, 0, '', 1)",
            )
            .bind(&generated_id)
            .bind(&readlist.name)
            .execute(pool)
            .await
            .map_err(|error| { anyhow::anyhow!(error).context( format!(
                    "failed to create ComicInfo readlist '{}' for '{}'",
                    readlist.name, book_id,
                ))
            })?;
            ComicInfoReadListTarget {
                id: generated_id,
                created: true,
            }
        }
    };

    let book_already_in_readlist =
        sqlx::query("SELECT 1 FROM READLIST_BOOK WHERE READLIST_ID = ? AND BOOK_ID = ? LIMIT 1")
            .bind(&target.id)
            .bind(book_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "failed to check ComicInfo readlist membership '{}' for '{}'",
                    readlist.name, book_id,
                ))
            })?
            .is_some();
    if book_already_in_readlist {
        return Ok(None);
    }

    let assigned_number = assign_comicinfo_readlist_number(pool, &target.id, readlist.number)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to assign ComicInfo readlist number '{}' for '{}'",
                readlist.name, book_id,
            ))
        })?;

    sqlx::query("INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) VALUES (?, ?, ?)")
        .bind(&target.id)
        .bind(book_id)
        .bind(assigned_number)
        .execute(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to insert ComicInfo readlist membership '{}' for '{}'",
                readlist.name, book_id,
            ))
        })?;

    sqlx::query(
        r#"
        UPDATE READLIST
        SET BOOK_COUNT = (SELECT COUNT(*) FROM READLIST_BOOK WHERE READLIST_ID = ?),
            LAST_MODIFIED_DATE = CURRENT_TIMESTAMP
        WHERE ID = ?
        "#,
    )
    .bind(&target.id)
    .bind(&target.id)
    .execute(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to update ComicInfo readlist counters '{}' for '{}'",
            readlist.name, book_id,
        ))
    })?;

    let readlist_book_ids = load_readlist_book_ids(pool, &target.id).await?;
    events::emit_readlist(
        runtime_events,
        &target.id,
        &readlist_book_ids,
        target.created,
    );

    Ok(Some(target.id))
}

async fn load_readlist_book_ids(
    pool: &SqlitePool,
    readlist_id: &str,
) -> anyhow::Result<Vec<String>> {
    sqlx::query("SELECT BOOK_ID FROM READLIST_BOOK WHERE READLIST_ID = ? ORDER BY NUMBER ASC")
        .bind(readlist_id)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to load readlist books for '{readlist_id}': "
            ))
        })
        .map(|rows| {
            rows.into_iter()
                .map(|row| row.get::<String, _>("BOOK_ID"))
                .collect()
        })
}

async fn assign_comicinfo_readlist_number(
    pool: &SqlitePool,
    readlist_id: &str,
    requested_number: Option<i64>,
) -> anyhow::Result<i64> {
    let max_number = sqlx::query(
        "SELECT COALESCE(MAX(NUMBER), -1) AS MAX_NUMBER FROM READLIST_BOOK WHERE READLIST_ID = ?",
    )
    .bind(readlist_id)
    .fetch_one(pool)
    .await
    .context("query ComicInfo readlist max position")?
    .get::<i64, _>("MAX_NUMBER");

    let Some(requested_number) = requested_number else {
        return Ok(max_number + 1);
    };

    let number_taken =
        sqlx::query("SELECT 1 FROM READLIST_BOOK WHERE READLIST_ID = ? AND NUMBER = ? LIMIT 1")
            .bind(readlist_id)
            .bind(requested_number)
            .fetch_optional(pool)
            .await
            .context("query ComicInfo readlist position collision")?
            .is_some();

    if number_taken {
        Ok(max_number + 1)
    } else {
        Ok(requested_number)
    }
}
