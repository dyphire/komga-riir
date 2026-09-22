use anyhow::Context;
use komga_domain::discovery::MediaStatus;
use sqlx::SqlitePool;

#[derive(Clone, Debug)]
pub(super) struct BookAnalysisInput {
    pub(super) url: String,
    pub(super) root: String,
    pub(super) analyze_dimensions: bool,
    pub(super) series_id: String,
    pub(super) library_id: String,
    pub(super) previous_media_status: Option<MediaStatus>,
    pub(super) previous_page_count: i64,
}

#[derive(Clone, Debug)]
pub(super) struct AnalyzedBookPage {
    pub(super) file_name: String,
    pub(super) media_type: String,
    pub(super) width: Option<i64>,
    pub(super) height: Option<i64>,
    pub(super) file_size: i64,
}

#[derive(Clone, Debug)]
pub(super) struct AnalyzedBookMedia {
    pub(super) status: MediaStatus,
    pub(super) media_type: String,
    pub(super) comment: Option<String>,
    pub(super) page_count: u64,
    pub(super) epub_divina_compatible: bool,
    pub(super) epub_is_kepub: bool,
    pub(super) pages: Vec<AnalyzedBookPage>,
    pub(super) media_files: Vec<AnalyzedBookMediaFile>,
    pub(super) epub_extension_blob: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub(super) struct AnalyzedBookMediaFile {
    pub(super) file_name: String,
    pub(super) media_type: Option<String>,
    pub(super) sub_type: Option<String>,
    pub(super) file_size: Option<i64>,
}

pub(super) async fn analyze_book_input(
    pool: &SqlitePool,
    book_id: &str,
) -> anyhow::Result<Option<BookAnalysisInput>> {
    let row = sqlx::query(
        r#"SELECT
             b.URL AS URL,
             b.SERIES_ID AS SERIES_ID,
             l.ANALYZE_DIMENSIONS AS ANALYZE_DIMENSIONS,
             COALESCE(m.STATUS, '') AS PREVIOUS_MEDIA_STATUS,
             COALESCE(m.PAGE_COUNT, 0) AS PREVIOUS_PAGE_COUNT,
             l.ID AS LIBRARY_ID,
             l.ROOT AS ROOT
            FROM BOOK b
            JOIN LIBRARY l ON l.ID = b.LIBRARY_ID
           LEFT JOIN MEDIA m ON m.BOOK_ID = b.ID
              WHERE b.ID = ?
              LIMIT 1
             "#,
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await
    .context("failed to load BOOK row for analyze")?;

    Ok(row.map(|row| BookAnalysisInput {
        url: sqlx::Row::get::<String, _>(&row, "URL"),
        root: sqlx::Row::get::<String, _>(&row, "ROOT"),
        analyze_dimensions: sqlx::Row::get::<bool, _>(&row, "ANALYZE_DIMENSIONS"),
        series_id: sqlx::Row::get::<String, _>(&row, "SERIES_ID"),
        library_id: sqlx::Row::get::<String, _>(&row, "LIBRARY_ID"),
        previous_media_status: MediaStatus::parse(
            sqlx::Row::get::<String, _>(&row, "PREVIOUS_MEDIA_STATUS").as_str(),
        ),
        previous_page_count: sqlx::Row::get::<i64, _>(&row, "PREVIOUS_PAGE_COUNT"),
    }))
}

pub(super) async fn persist_book_analysis(
    pool: &SqlitePool,
    book_id: &str,
    analysis: &AnalyzedBookMedia,
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to start analyze-book transaction for '{book_id}': "
        ))
    })?;

    sqlx::query("DELETE FROM MEDIA_PAGE WHERE BOOK_ID = ?")
        .bind(book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to clear MEDIA_PAGE rows for '{book_id}'"))
        })?;

    for (index, page) in analysis.pages.iter().enumerate() {
        sqlx::query(
            r#"INSERT INTO MEDIA_PAGE (
            FILE_NAME,
            MEDIA_TYPE,
            NUMBER,
            BOOK_ID,
            width,
            height,
            FILE_HASH,
            FILE_SIZE
        ) VALUES (?, ?, ?, ?, ?, ?, '', ?)"#,
        )
        .bind(&page.file_name)
        .bind(&page.media_type)
        .bind(index as i64)
        .bind(book_id)
        .bind(page.width)
        .bind(page.height)
        .bind(page.file_size)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to insert MEDIA_PAGE row for '{book_id}'"))
        })?;
    }

    sqlx::query("DELETE FROM MEDIA_FILE WHERE BOOK_ID = ?")
        .bind(book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to clear MEDIA_FILE rows for '{book_id}'"))
        })?;

    for file in &analysis.media_files {
        sqlx::query(
            r#"INSERT INTO MEDIA_FILE (
                FILE_NAME,
                BOOK_ID,
                MEDIA_TYPE,
                SUB_TYPE,
                FILE_SIZE
            ) VALUES (?, ?, ?, ?, ?)
            ON CONFLICT DO NOTHING"#,
        )
        .bind(&file.file_name)
        .bind(book_id)
        .bind(&file.media_type)
        .bind(&file.sub_type)
        .bind(file.file_size)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to insert derived MEDIA_FILE row for '{book_id}'"
            ))
        })?;
    }

    sqlx::query(
        r#"INSERT INTO MEDIA (
            BOOK_ID,
            STATUS,
            MEDIA_TYPE,
            COMMENT,
            PAGE_COUNT,
            EPUB_DIVINA_COMPATIBLE,
            EPUB_IS_KEPUB
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(BOOK_ID) DO UPDATE
        SET STATUS = excluded.STATUS,
            MEDIA_TYPE = excluded.MEDIA_TYPE,
            COMMENT = excluded.COMMENT,
            PAGE_COUNT = excluded.PAGE_COUNT,
            EPUB_DIVINA_COMPATIBLE = excluded.EPUB_DIVINA_COMPATIBLE,
            EPUB_IS_KEPUB = excluded.EPUB_IS_KEPUB,
            LAST_MODIFIED_DATE = CURRENT_TIMESTAMP"#,
    )
    .bind(book_id)
    .bind(analysis.status.persisted_name())
    .bind(&analysis.media_type)
    .bind(&analysis.comment)
    .bind(analysis.page_count.min(i32::MAX as u64) as i32)
    .bind(analysis.epub_divina_compatible)
    .bind(analysis.epub_is_kepub)
    .execute(&mut *tx)
    .await
    .context("failed to persist MEDIA analyze state")?;

    if let Some(blob) = &analysis.epub_extension_blob {
        sqlx::query(
            r#"UPDATE MEDIA
               SET EXTENSION_CLASS = ?,
                   EXTENSION_VALUE_BLOB = ?
             WHERE BOOK_ID = ?"#,
        )
        .bind("org.gotson.komga.domain.model.MediaExtensionEpub")
        .bind(blob)
        .bind(book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to persist EPUB extension for '{book_id}'"))
        })?;
    } else {
        sqlx::query(
            r#"UPDATE MEDIA
               SET EXTENSION_CLASS = NULL,
                   EXTENSION_VALUE_BLOB = NULL
             WHERE BOOK_ID = ?"#,
        )
        .bind(book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("failed to clear EPUB extension for '{book_id}'"))
        })?;
    }

    tx.commit().await.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to commit analyze-book transaction for '{book_id}': "
        ))
    })?;

    Ok(())
}
