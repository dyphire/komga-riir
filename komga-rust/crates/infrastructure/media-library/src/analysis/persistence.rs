use anyhow::Context;
use komga_domain::discovery::MediaStatus;
use komga_infrastructure_media_core::content::metadata_sources::MetadataSourceRequest;
use sqlx::SqlitePool;

#[derive(Clone, Debug)]
pub(super) struct BookAnalysisInput {
    pub(super) url: String,
    pub(super) root: String,
    pub(super) analyze_dimensions: bool,
    pub(super) hash_pages: bool,
    pub(super) series_id: String,
    pub(super) library_id: String,
    pub(super) previous_media_status: Option<MediaStatus>,
    pub(super) previous_page_count: i64,
    pub(super) metadata_sources: MetadataSourceRequest,
}

#[derive(Clone, Debug)]
pub(super) struct AnalyzedBookPage {
    pub(super) file_name: String,
    pub(super) media_type: String,
    pub(super) width: Option<i64>,
    pub(super) height: Option<i64>,
    pub(super) file_size: i64,
    pub(super) file_hash: Option<String>,
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
             b.LIBRARY_ID AS LIBRARY_ID,
             l.ANALYZE_DIMENSIONS AS ANALYZE_DIMENSIONS,
             l.HASH_PAGES AS HASH_PAGES,
             l.IMPORT_COMICINFO_BOOK AS IMPORT_COMICINFO_BOOK,
             l.IMPORT_COMICINFO_READLIST AS IMPORT_COMICINFO_READLIST,
             l.IMPORT_COMICINFO_SERIES AS IMPORT_COMICINFO_SERIES,
             l.IMPORT_COMICINFO_COLLECTION AS IMPORT_COMICINFO_COLLECTION,
             l.IMPORT_EPUB_BOOK AS IMPORT_EPUB_BOOK,
             l.IMPORT_EPUB_SERIES AS IMPORT_EPUB_SERIES,
             COALESCE(m.STATUS, '') AS PREVIOUS_MEDIA_STATUS,
             COALESCE(m.PAGE_COUNT, 0) AS PREVIOUS_PAGE_COUNT,
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

    Ok(row.map(|row| {
        let comicinfo = sqlx::Row::get::<bool, _>(&row, "IMPORT_COMICINFO_BOOK")
            || sqlx::Row::get::<bool, _>(&row, "IMPORT_COMICINFO_READLIST")
            || sqlx::Row::get::<bool, _>(&row, "IMPORT_COMICINFO_SERIES")
            || sqlx::Row::get::<bool, _>(&row, "IMPORT_COMICINFO_COLLECTION");
        let epub = sqlx::Row::get::<bool, _>(&row, "IMPORT_EPUB_BOOK")
            || sqlx::Row::get::<bool, _>(&row, "IMPORT_EPUB_SERIES");
        BookAnalysisInput {
            url: sqlx::Row::get::<String, _>(&row, "URL"),
            root: sqlx::Row::get::<String, _>(&row, "ROOT"),
            analyze_dimensions: sqlx::Row::get::<bool, _>(&row, "ANALYZE_DIMENSIONS"),
            hash_pages: sqlx::Row::get::<bool, _>(&row, "HASH_PAGES"),
            series_id: sqlx::Row::get::<String, _>(&row, "SERIES_ID"),
            library_id: sqlx::Row::get::<String, _>(&row, "LIBRARY_ID"),
            previous_media_status: MediaStatus::parse(
                sqlx::Row::get::<String, _>(&row, "PREVIOUS_MEDIA_STATUS").as_str(),
            ),
            previous_page_count: sqlx::Row::get::<i64, _>(&row, "PREVIOUS_PAGE_COUNT"),
            metadata_sources: MetadataSourceRequest { comicinfo, epub },
        }
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

    if !analysis.pages.is_empty() {
        // Batch multi-row INSERT: up to 2000 pages per statement instead of one
        // statement per page (a 1000-page book: 1000 statements -> 1). 2000 x 8
        // bound variables stays well below SQLite's 32766 limit (3.32+).
        const MEDIA_PAGE_INSERT_BATCH: usize = 2000;
        for (chunk_index, chunk) in analysis
            .pages
            .chunks(MEDIA_PAGE_INSERT_BATCH)
            .enumerate()
        {
            let chunk_start = chunk_index * MEDIA_PAGE_INSERT_BATCH;
            let mut builder = sqlx::QueryBuilder::new(
                r#"INSERT INTO MEDIA_PAGE (
                FILE_NAME,
                MEDIA_TYPE,
                NUMBER,
                BOOK_ID,
                width,
                height,
                FILE_HASH,
                FILE_SIZE
            )"#,
            );
            builder.push_values(chunk.iter().enumerate(), |mut b, (offset, page)| {
                b.push_bind(&page.file_name)
                    .push_bind(&page.media_type)
                    .push_bind((chunk_start + offset) as i64)
                    .push_bind(book_id)
                    .push_bind(page.width)
                    .push_bind(page.height)
                    .push_bind(page.file_hash.clone().unwrap_or_default())
                    .push_bind(page.file_size);
            });
            builder
                .build()
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(error)
                        .context(format!("failed to insert MEDIA_PAGE rows for '{book_id}'"))
                })?;
        }
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
