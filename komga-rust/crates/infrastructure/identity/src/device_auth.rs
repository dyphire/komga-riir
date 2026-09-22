use sqlx::{Row, SqlitePool};

use komga_infrastructure_base::resolve_optional_library_item_path;

use komga_application::identity_access::{
    DeviceThumbnailBinary, KoreaderBookLookupError, KoreaderBookTarget, PersistedReadProgressRecord,
};

pub(crate) struct PersistedKoboMetadataRecord {
    pub(crate) title: String,
    pub(crate) summary: String,
    pub(crate) release_date: Option<String>,
    pub(crate) created_date: Option<String>,
    pub(crate) language: String,
    pub(crate) file_size: u64,
    pub(crate) kepub_file_size: Option<u64>,
    pub(crate) file_name: String,
    pub(crate) media_type: String,
    pub(crate) contributor_names: Vec<String>,
    pub(crate) isbn: Option<String>,
    pub(crate) publisher_name: Option<String>,
    pub(crate) cover_image_id: Option<String>,
    pub(crate) series_id: Option<String>,
    pub(crate) series_name: Option<String>,
    pub(crate) series_number: Option<String>,
    pub(crate) series_number_float: Option<f64>,
    pub(crate) oneshot: bool,
    pub(crate) is_kepub: bool,
    pub(crate) epub_extension_blob: Option<Vec<u8>>,
}

pub(crate) async fn load_kobo_metadata_record(
    pool: &SqlitePool,
    book_id: &str,
) -> Result<Option<PersistedKoboMetadataRecord>, sqlx::Error> {
    let row = sqlx::query(
        r#"SELECT COALESCE(bm.TITLE, b.NAME) AS TITLE,
       COALESCE(bm.SUMMARY, '') AS SUMMARY,
       bm.RELEASE_DATE AS RELEASE_DATE,
       COALESCE(bm.CREATED_DATE, b.CREATED_DATE, '') AS CREATED_DATE,
       COALESCE(sm.LANGUAGE, 'en') AS LANGUAGE,
       b.FILE_SIZE AS FILE_SIZE,
       bp.FILE_SIZE AS KEPUB_FILE_SIZE,
       b.NAME AS FILE_NAME,
       COALESCE(m.MEDIA_TYPE, 'application/octet-stream') AS MEDIA_TYPE,
       NULLIF(TRIM(bm.ISBN), '') AS ISBN,
       NULLIF(TRIM(sm.PUBLISHER), '') AS PUBLISHER_NAME,
       tb.ID AS COVER_IMAGE_ID,
       sm.SERIES_ID AS SERIES_ID,
       sm.TITLE AS SERIES_NAME,
       NULLIF(TRIM(bm.NUMBER), '') AS SERIES_NUMBER,
       bm.NUMBER_SORT AS SERIES_NUMBER_FLOAT,
       b.ONESHOT AS ONESHOT,
       COALESCE(m.EPUB_IS_KEPUB, FALSE) AS EPUB_IS_KEPUB,
       m.EXTENSION_VALUE_BLOB AS EXTENSION_VALUE_BLOB
 FROM BOOK b
  LEFT JOIN BOOK_METADATA bm ON bm.BOOK_ID = b.ID
  LEFT JOIN SERIES_METADATA sm ON sm.SERIES_ID = b.SERIES_ID
  LEFT JOIN MEDIA m ON m.BOOK_ID = b.ID
  LEFT JOIN THUMBNAIL_BOOK tb ON tb.BOOK_ID = b.ID AND tb.SELECTED = TRUE
  LEFT JOIN BOOK_PROJECTION bp ON bp.BOOK_ID = b.ID AND bp.PROFILE = 'kepub'
 WHERE b.ID = ?
   AND b.DELETED_DATE IS NULL
   AND bm.BOOK_ID IS NOT NULL
 LIMIT 1"#,
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await?;

    let contributor_rows = sqlx::query(
        r#"SELECT NAME
 FROM BOOK_METADATA_AUTHOR
 WHERE BOOK_ID = ?
   AND NAME IS NOT NULL
   AND TRIM(NAME) <> ''
 ORDER BY NAME ASC"#,
    )
    .bind(book_id)
    .fetch_all(pool)
    .await?;

    let contributor_names = contributor_rows
        .into_iter()
        .map(|row| row.get::<String, _>("NAME"))
        .collect::<Vec<_>>();

    Ok(row.map(|row| PersistedKoboMetadataRecord {
        title: row.get::<String, _>("TITLE"),
        summary: row.get::<String, _>("SUMMARY"),
        release_date: row.get::<Option<String>, _>("RELEASE_DATE"),
        created_date: {
            let created_date = row.get::<String, _>("CREATED_DATE");
            let created_date = created_date.trim();
            if created_date.is_empty() {
                None
            } else {
                Some(created_date.to_string())
            }
        },
        language: row.get::<String, _>("LANGUAGE"),
        file_size: row.get::<i64, _>("FILE_SIZE").max(0) as u64,
        kepub_file_size: row.get::<Option<i64>, _>("KEPUB_FILE_SIZE").map(|value| value.max(0) as u64),
        file_name: row.get::<String, _>("FILE_NAME"),
        media_type: row.get::<String, _>("MEDIA_TYPE"),
        contributor_names,
        isbn: row.get::<Option<String>, _>("ISBN"),
        publisher_name: row.get::<Option<String>, _>("PUBLISHER_NAME"),
        cover_image_id: row.get::<Option<String>, _>("COVER_IMAGE_ID"),
        series_id: row.get::<Option<String>, _>("SERIES_ID"),
        series_name: row.get::<Option<String>, _>("SERIES_NAME"),
        series_number: row.get::<Option<String>, _>("SERIES_NUMBER"),
        series_number_float: row.get::<Option<f64>, _>("SERIES_NUMBER_FLOAT"),
        oneshot: row.get::<bool, _>("ONESHOT"),
        is_kepub: row.get::<bool, _>("EPUB_IS_KEPUB"),
        epub_extension_blob: row.get::<Option<Vec<u8>>, _>("EXTENSION_VALUE_BLOB"),
    }))
}

pub(crate) async fn load_thumbnail_by_id(
    pool: &SqlitePool,
    thumbnail_id: &str,
) -> anyhow::Result<Option<DeviceThumbnailBinary>> {
    let row = sqlx::query(
        r#"SELECT tb.BOOK_ID, tb.MEDIA_TYPE, tb.THUMBNAIL, tb.URL, l.ROOT AS LIBRARY_ROOT
 FROM THUMBNAIL_BOOK tb
 LEFT JOIN BOOK b ON b.ID = tb.BOOK_ID
 LEFT JOIN LIBRARY l ON l.ID = b.LIBRARY_ID
 WHERE tb.ID = ?
 LIMIT 1"#,
    )
    .bind(thumbnail_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!("load Kobo thumbnail '{thumbnail_id}'"))
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    let media_type = row.get::<String, _>("MEDIA_TYPE");
    let book_id = row.get::<String, _>("BOOK_ID");
    if let Some(thumbnail) = row.get::<Option<Vec<u8>>, _>("THUMBNAIL") {
        return Ok(Some(DeviceThumbnailBinary {
            book_id,
            media_type,
            bytes: thumbnail,
        }));
    }

    let Some(url) = row.get::<Option<String>, _>("URL") else {
        return Ok(None);
    };
    let library_root = row.get::<Option<String>, _>("LIBRARY_ROOT");
    let sidecar_path = resolve_optional_library_item_path(library_root.as_deref(), &url)
        .ok_or_else(|| {
            anyhow::anyhow!(format!(
                "persisted Kobo thumbnail sidecar URL requires a library root: {url}"
            ))
        })?;

    let bytes = std::fs::read(&sidecar_path).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "read Kobo thumbnail sidecar {}: ",
            sidecar_path.display()
        ))
    })?;
    Ok(Some(DeviceThumbnailBinary {
        book_id,
        media_type,
        bytes,
    }))
}

pub(crate) async fn save_book_projection_file_size(
    pool: &SqlitePool,
    book_id: &str,
    profile: &str,
    file_size: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO BOOK_PROJECTION (BOOK_ID, PROFILE, FILE_SIZE, CREATED_DATE, LAST_MODIFIED_DATE)
 VALUES (?, ?, ?, datetime('now'), datetime('now'))
 ON CONFLICT (BOOK_ID, PROFILE) DO UPDATE SET
   FILE_SIZE = excluded.FILE_SIZE,
   LAST_MODIFIED_DATE = excluded.LAST_MODIFIED_DATE"#,
    )
    .bind(book_id)
    .bind(profile)
    .bind(file_size as i64)
    .execute(pool)
    .await
    .map(|_| ())
}

pub(crate) async fn persisted_book_exists(
    pool: &SqlitePool,
    book_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query(
        r#"SELECT 1 AS FOUND
 FROM BOOK
 WHERE ID = ?
   AND DELETED_DATE IS NULL
 LIMIT 1"#,
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub(crate) async fn load_book_created_timestamp(
    pool: &SqlitePool,
    book_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        r#"SELECT CREATED_DATE
 FROM BOOK
 WHERE ID = ?
   AND DELETED_DATE IS NULL
 LIMIT 1"#,
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await?;

    Ok(row
        .map(|row| row.get::<Option<String>, _>("CREATED_DATE"))
        .unwrap_or(None)
        .filter(|value| !value.trim().is_empty()))
}

pub(crate) async fn load_read_progress(
    pool: &SqlitePool,
    book_id: &str,
    user_id_value: &str,
) -> Result<Option<PersistedReadProgressRecord>, sqlx::Error> {
    let row = sqlx::query(
        r#"SELECT PAGE, COMPLETED, CREATED_DATE, LAST_MODIFIED_DATE,
       COALESCE(DEVICE_ID, '') AS DEVICE_ID,
       COALESCE(DEVICE_NAME, '') AS DEVICE_NAME,
       LOCATOR
 FROM READ_PROGRESS
 WHERE BOOK_ID = ?
   AND USER_ID = ?
 LIMIT 1"#,
    )
    .bind(book_id)
    .bind(user_id_value)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| PersistedReadProgressRecord {
        page: row.get::<i64, _>("PAGE"),
        completed: row.get::<bool, _>("COMPLETED"),
        created: row.get::<String, _>("CREATED_DATE"),
        last_modified: row.get::<String, _>("LAST_MODIFIED_DATE"),
        device_id: row.get::<String, _>("DEVICE_ID"),
        device_name: row.get::<String, _>("DEVICE_NAME"),
        locator: row
            .try_get::<Option<Vec<u8>>, _>("LOCATOR")
            .or_else(|_| row.try_get::<Option<Vec<u8>>, _>("locator"))
            .ok()
            .flatten(),
    }))
}

pub(crate) async fn load_koreader_book_target(
    pool: &SqlitePool,
    book_hash: &str,
) -> Result<Option<KoreaderBookTarget>, KoreaderBookLookupError> {
    let rows = sqlx::query(
        r#"SELECT b.ID AS BOOK_ID,
         COALESCE(m.PAGE_COUNT, 0) AS PAGE_COUNT,
         COALESCE(m.MEDIA_TYPE, '') AS MEDIA_TYPE
  FROM BOOK b
  LEFT JOIN MEDIA m ON m.BOOK_ID = b.ID
  WHERE b.FILE_HASH_KOREADER = ?
   AND b.DELETED_DATE IS NULL
 ORDER BY b.ID ASC"#,
    )
    .bind(book_hash)
    .fetch_all(pool)
    .await
    .map_err(|_| KoreaderBookLookupError::Persistence)?;

    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() > 1 {
        return Err(KoreaderBookLookupError::Conflict);
    }

    let row = rows.first().expect("koreader target row should exist");
    Ok(Some(KoreaderBookTarget {
        id: row.get::<String, _>("BOOK_ID"),
        page_count: row.get::<i64, _>("PAGE_COUNT").max(0) as u64,
        media_type: row.get::<String, _>("MEDIA_TYPE"),
    }))
}
