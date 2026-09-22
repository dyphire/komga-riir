use anyhow::Context;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use komga_application::media_assets::{
    BookImportPort, BooksImportEntry, ImportBookOutcome, ImportCopyMode,
};
use komga_domain::media_assets::ThumbnailType;
use sqlx::{Row, SqlitePool};

use komga_infrastructure_base::{
    random_hex_token, resolve_library_item_path, resolve_rooted_path, resolve_stored_path,
};

#[derive(Clone, Debug)]
pub struct FilesystemBookImport {
    read_pool: SqlitePool,
    write_pool: SqlitePool,
}

impl FilesystemBookImport {
    pub fn new(read_pool: SqlitePool, write_pool: SqlitePool) -> Self {
        Self {
            read_pool,
            write_pool,
        }
    }
}

#[async_trait::async_trait]
impl BookImportPort for FilesystemBookImport {
    async fn import_book(
        &self,
        copy_mode: ImportCopyMode,
        entry: BooksImportEntry,
    ) -> anyhow::Result<Option<ImportBookOutcome>> {
        import_book_impl(&self.read_pool, &self.write_pool, copy_mode, entry).await
    }
}

async fn import_book_impl(
    read_pool: &SqlitePool,
    write_pool: &SqlitePool,
    copy_mode: ImportCopyMode,
    entry: BooksImportEntry,
) -> anyhow::Result<Option<ImportBookOutcome>> {
    if !path_exists(entry.source_file.as_path(), "inspect import source file")? {
        return Err(anyhow::anyhow!("source file does not exist"));
    }

    let library_roots = load_library_roots(read_pool).await?;
    if source_inside_library_roots(entry.source_file.as_path(), &library_roots)? {
        return Err(anyhow::anyhow!(
            "cannot import file that is part of an existing library"
        ));
    }

    let target = match load_import_series_target(read_pool, &entry.series_id).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            return Err(anyhow::anyhow!(format!(
                "series target for import was not found: {}",
                entry.series_id
            )));
        }
        Err(error) => return Err(error),
    };

    if target.oneshot && entry.upgrade_book_id.is_none() {
        return Err(anyhow::anyhow!(
            "destination series is oneshot but upgradeBookId is missing"
        ));
    }

    let mut upgrade_file: Option<PathBuf> = None;
    let mut upgrade_sidecars: Vec<PathBuf> = Vec::new();
    if let Some(upgrade_book_id) = entry.upgrade_book_id.as_deref() {
        let loaded_upgrade_target =
            match load_import_upgrade_book_target(read_pool, upgrade_book_id).await {
                Ok(Some(target)) => target,
                Ok(None) => {
                    return Err(anyhow::anyhow!(format!(
                        "upgrade target for import was not found: {upgrade_book_id}"
                    )));
                }
                Err(error) => return Err(error),
            };

        if loaded_upgrade_target.series_id != entry.series_id {
            return Err(anyhow::anyhow!(format!(
                "upgrade target series mismatch for import: expected {}, got {}",
                entry.series_id, loaded_upgrade_target.series_id
            )));
        }

        let loaded_upgrade_file = resolve_library_item_path(
            &loaded_upgrade_target.library_root,
            &loaded_upgrade_target.book_url,
        );
        upgrade_sidecars = collect_book_sidecar_paths(&loaded_upgrade_file)?;
        upgrade_file = Some(loaded_upgrade_file);
    }

    let Some(destination_name) = resolve_import_destination_name(
        entry.source_file.as_path(),
        entry.destination_name.as_deref(),
    ) else {
        return Err(anyhow::anyhow!(format!(
            "destination name for import is invalid: {}",
            entry.destination_name.as_deref().unwrap_or_default()
        )));
    };

    let destination_dir = resolve_import_destination_dir(&target);
    fs::create_dir_all(&destination_dir).context("create destination directory for import")?;

    let destination_file = destination_dir.join(destination_name);
    let imported_sidecars =
        collect_import_book_sidecars(entry.source_file.as_path(), &destination_file)?;
    if let Some(upgrade_file) = upgrade_file.as_ref()
        && destination_file == *upgrade_file
    {
        remove_import_upgrade_path(upgrade_file, "remove previous upgraded book file")?;
    }
    for upgrade_sidecar in &upgrade_sidecars {
        remove_import_upgrade_path(upgrade_sidecar, "remove previous upgraded book sidecar")?;
    }

    if path_exists(
        destination_file.as_path(),
        "inspect import destination file",
    )? {
        return Err(anyhow::anyhow!(format!(
            "destination file already exists: {}",
            destination_file.display()
        )));
    }

    apply_import_copy_mode(
        copy_mode,
        entry.source_file.as_path(),
        &destination_file,
        false,
    )?;

    let sidecar_result = import_book_sidecars(copy_mode, &imported_sidecars)?;

    if let Some(upgrade_file) = upgrade_file.as_ref()
        && destination_file != *upgrade_file
    {
        remove_import_upgrade_path(upgrade_file, "remove previous upgraded book file")?;
    }

    let imported_book_id = scanner_book_id_for_path(&destination_file);
    if let Some(upgrade_book_id) = entry.upgrade_book_id.as_deref() {
        let resolved_library_root = resolve_stored_path(&target.library_root);
        migrate_upgraded_book_identity(
            write_pool,
            upgrade_book_id,
            imported_book_id.as_str(),
            resolved_library_root.as_path(),
            &destination_file,
        )
        .await?;
    }

    persist_book_imported_event(
        write_pool,
        imported_book_id.as_str(),
        target.series_id.as_str(),
        &destination_file,
        entry.source_file.as_path(),
        entry.upgrade_book_id.is_some(),
    )
    .await?;

    Ok(Some(ImportBookOutcome {
        library_id: target.library_id,
        imported_book_id,
        sidecar_imported: sidecar_result.metadata_imported,
        artwork_sidecar_imported: sidecar_result.artwork_imported,
    }))
}

struct ImportSeriesTarget {
    series_id: String,
    library_id: String,
    library_root: String,
    series_url: String,
    oneshot: bool,
}

struct ImportUpgradeBookTarget {
    series_id: String,
    library_root: String,
    book_url: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImportBookSidecarType {
    Metadata,
    Artwork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportBookSidecarClassification {
    sidecar_type: ImportBookSidecarType,
    suffix: String,
    extension: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportBookSidecarTransfer {
    source_path: PathBuf,
    destination_path: PathBuf,
    sidecar_type: ImportBookSidecarType,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ImportBookSidecarResult {
    metadata_imported: bool,
    artwork_imported: bool,
}

async fn load_import_series_target(
    pool: &SqlitePool,
    series_id: &str,
) -> anyhow::Result<Option<ImportSeriesTarget>> {
    let row = sqlx::query(
        r#"SELECT s.ID AS SERIES_ID, s.LIBRARY_ID AS LIBRARY_ID, s.URL AS SERIES_URL,
            l.ROOT AS LIBRARY_ROOT, s.ONESHOT AS ONESHOT
        FROM SERIES s
        JOIN LIBRARY l ON l.ID = s.LIBRARY_ID
        WHERE s.ID = ?
        LIMIT 1"#,
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .context("query series target for import")?;

    Ok(row.map(|row| ImportSeriesTarget {
        series_id: row.get::<String, _>("SERIES_ID"),
        library_id: row.get::<String, _>("LIBRARY_ID"),
        library_root: row.get::<String, _>("LIBRARY_ROOT"),
        series_url: row.get::<String, _>("SERIES_URL"),
        oneshot: row.get::<i64, _>("ONESHOT") != 0,
    }))
}

async fn load_import_upgrade_book_target(
    pool: &SqlitePool,
    book_id: &str,
) -> anyhow::Result<Option<ImportUpgradeBookTarget>> {
    let row = sqlx::query(
        r#"SELECT b.SERIES_ID AS SERIES_ID, b.URL AS BOOK_URL,
            l.ROOT AS LIBRARY_ROOT
        FROM BOOK b
        JOIN LIBRARY l ON l.ID = b.LIBRARY_ID
        WHERE b.ID = ?
        LIMIT 1"#,
    )
    .bind(book_id)
    .fetch_optional(pool)
    .await
    .context("query upgrade book target for import")?;

    Ok(row.map(|row| ImportUpgradeBookTarget {
        series_id: row.get::<String, _>("SERIES_ID"),
        library_root: row.get::<String, _>("LIBRARY_ROOT"),
        book_url: row.get::<String, _>("BOOK_URL"),
    }))
}

fn resolve_import_destination_dir(target: &ImportSeriesTarget) -> PathBuf {
    let root = resolve_stored_path(&target.library_root);
    if target.oneshot {
        let series_path = resolve_rooted_path(root.as_path(), &target.series_url);
        if let Some(parent) = series_path.parent()
            && !parent.as_os_str().is_empty()
        {
            return parent.to_path_buf();
        }
        root
    } else {
        resolve_rooted_path(root.as_path(), &target.series_url)
    }
}

async fn load_library_roots(pool: &SqlitePool) -> anyhow::Result<Vec<PathBuf>> {
    let rows = sqlx::query("SELECT ROOT FROM LIBRARY")
        .fetch_all(pool)
        .await
        .context("query library roots for import")?;

    Ok(rows
        .into_iter()
        .map(|row| resolve_stored_path(row.get::<String, _>("ROOT").as_str()))
        .collect())
}

fn source_inside_library_roots(
    source_file: &Path,
    library_roots: &[PathBuf],
) -> anyhow::Result<bool> {
    let source = match fs::canonicalize(source_file) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "canonicalize import source file '{}': {error}",
                source_file.display()
            )));
        }
    };

    for root in library_roots {
        let root = match fs::canonicalize(root) {
            Ok(root) => root,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "canonicalize import library root '{}': {error}",
                    root.display()
                )));
            }
        };
        if source.starts_with(root) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn resolve_import_destination_name(
    source_file: &Path,
    destination_name: Option<&str>,
) -> Option<String> {
    if let Some(destination_name) = destination_name {
        if destination_name.contains('/') || destination_name.contains('\\') {
            return None;
        }
        return match source_file.extension().and_then(|value| value.to_str()) {
            Some(extension) if !extension.is_empty() => {
                Some(format!("{destination_name}.{extension}"))
            }
            _ => Some(destination_name.to_string()),
        };
    }

    source_file
        .file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
}

fn apply_import_copy_mode(
    copy_mode: ImportCopyMode,
    source_file: &Path,
    destination_file: &Path,
    replace_existing: bool,
) -> anyhow::Result<()> {
    if !path_exists(source_file, "inspect import source file")? {
        return Err(anyhow::anyhow!("source file does not exist"));
    }

    let destination_exists = path_exists(destination_file, "inspect import destination file")?;
    if replace_existing && destination_exists {
        fs::remove_file(destination_file).context("remove existing destination file")?;
    } else if destination_exists {
        return Err(anyhow::anyhow!(format!(
            "destination file already exists: {}",
            destination_file.display()
        )));
    }

    match copy_mode {
        ImportCopyMode::Copy => {
            fs::copy(source_file, destination_file).context("copy source file for import")?;
            Ok(())
        }
        ImportCopyMode::Move => {
            if let Err(rename_error) = fs::rename(source_file, destination_file) {
                fs::copy(source_file, destination_file).map_err(|copy_error| {
                    anyhow::Error::from(copy_error).context(format!(
                        "move source file for import failed ({rename_error}); copy attempt failed"
                    ))
                })?;
                fs::remove_file(source_file).map_err(|remove_error| {
                    anyhow::anyhow!(remove_error)
                        .context("remove source file after move-then-copy attempt")
                })?;
            }
            Ok(())
        }
        ImportCopyMode::Hardlink => {
            if fs::hard_link(source_file, destination_file).is_err() {
                fs::copy(source_file, destination_file)
                    .context("hardlink/copy source file for import")?;
            }
            Ok(())
        }
    }
}

fn path_exists(path: &Path, context: &str) -> anyhow::Result<bool> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow::anyhow!(format!(
            "{context} '{}': {error}",
            path.display()
        ))),
    }
}

fn remove_import_upgrade_path(path: &Path, context: &str) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::anyhow!(format!(
            "{context} '{}': {error}",
            path.display()
        ))),
    }
}

fn collect_book_sidecar_paths(book_file: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let Some(book_dir) = book_file.parent() else {
        return Ok(Vec::new());
    };
    let Some(book_base_name) = book_file.file_stem().and_then(|value| value.to_str()) else {
        return Ok(Vec::new());
    };

    let entries = fs::read_dir(book_dir).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "read book sidecar directory '{}' for import: ",
            book_dir.display()
        ))
    })?;

    let mut sidecar_paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read book sidecar directory entry '{}' for import: ",
                book_dir.display()
            ))
        })?;
        let path = entry.path();
        if path == book_file {
            continue;
        }

        if classify_import_book_sidecar(path.as_path(), book_base_name).is_some() {
            let metadata = fs::metadata(&path).map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "read book sidecar metadata '{}' for import: ",
                    path.display()
                ))
            })?;
            if !metadata.is_file() {
                continue;
            }
            sidecar_paths.push(path);
        }
    }
    sidecar_paths.sort();
    Ok(sidecar_paths)
}

fn collect_import_book_sidecars(
    source_file: &Path,
    destination_file: &Path,
) -> anyhow::Result<Vec<ImportBookSidecarTransfer>> {
    let Some(destination_dir) = destination_file.parent() else {
        return Ok(Vec::new());
    };
    let Some(source_base_name) = source_file.file_stem().and_then(|value| value.to_str()) else {
        return Ok(Vec::new());
    };
    let Some(destination_base_name) = destination_file
        .file_stem()
        .and_then(|value| value.to_str())
    else {
        return Ok(Vec::new());
    };

    let mut transfers = Vec::new();
    for source_path in collect_book_sidecar_paths(source_file)? {
        let Some(classification) =
            classify_import_book_sidecar(source_path.as_path(), source_base_name)
        else {
            continue;
        };

        let destination_name = format!(
            "{}{}.{}",
            destination_base_name, classification.suffix, classification.extension
        );
        transfers.push(ImportBookSidecarTransfer {
            source_path,
            destination_path: destination_dir.join(destination_name),
            sidecar_type: classification.sidecar_type,
        });
    }
    transfers.sort_by(|left, right| left.source_path.cmp(&right.source_path));
    Ok(transfers)
}

fn classify_import_book_sidecar(
    sidecar_path: &Path,
    book_base_name: &str,
) -> Option<ImportBookSidecarClassification> {
    let extension = sidecar_path.extension().and_then(|value| value.to_str())?;
    let extension_lower = extension.to_ascii_lowercase();
    let sidecar_stem = sidecar_path.file_stem().and_then(|value| value.to_str())?;

    if extension_lower == "xml" && sidecar_stem.eq_ignore_ascii_case(book_base_name) {
        return Some(ImportBookSidecarClassification {
            sidecar_type: ImportBookSidecarType::Metadata,
            suffix: String::new(),
            extension: extension.to_string(),
        });
    }

    if !is_supported_book_artwork_extension(extension_lower.as_str()) {
        return None;
    }

    import_book_artwork_suffix(sidecar_stem, book_base_name).map(|suffix| {
        ImportBookSidecarClassification {
            sidecar_type: ImportBookSidecarType::Artwork,
            suffix,
            extension: extension.to_string(),
        }
    })
}

fn import_book_artwork_suffix(candidate_stem: &str, book_base_name: &str) -> Option<String> {
    if candidate_stem.eq_ignore_ascii_case(book_base_name) {
        return Some(String::new());
    }

    let lower_candidate = candidate_stem.to_ascii_lowercase();
    let lower_book_base_name = book_base_name.to_ascii_lowercase();
    lower_candidate
        .strip_prefix(&format!("{lower_book_base_name}-"))
        .filter(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
        .and_then(|_| candidate_stem.get(book_base_name.len()..))
        .map(str::to_string)
}

fn is_supported_book_artwork_extension(extension: &str) -> bool {
    matches!(extension, "png" | "jpeg" | "jpg" | "tbn" | "webp" | "gif")
}

fn import_book_sidecars(
    copy_mode: ImportCopyMode,
    sidecars: &[ImportBookSidecarTransfer],
) -> anyhow::Result<ImportBookSidecarResult> {
    let mut result = ImportBookSidecarResult::default();
    for sidecar in sidecars {
        apply_import_copy_mode(
            copy_mode,
            sidecar.source_path.as_path(),
            sidecar.destination_path.as_path(),
            true,
        )?;
        match sidecar.sidecar_type {
            ImportBookSidecarType::Metadata => result.metadata_imported = true,
            ImportBookSidecarType::Artwork => result.artwork_imported = true,
        }
    }
    Ok(result)
}

fn scanner_book_id_for_path(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("book-{:016x}", hasher.finish())
}

async fn migrate_upgraded_book_identity(
    pool: &SqlitePool,
    old_book_id: &str,
    new_book_id: &str,
    library_root: &Path,
    destination_file: &Path,
) -> anyhow::Result<()> {
    if old_book_id == new_book_id {
        return Ok(());
    }

    let destination_name = destination_file
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let destination_url = import_book_url_for_library_root(library_root, destination_file)?;

    let mut tx = pool
        .begin()
        .await
        .context("begin import-upgrade migration tx")?;

    let source_exists = sqlx::query("SELECT 1 AS FOUND FROM BOOK WHERE ID = ? LIMIT 1")
        .bind(old_book_id)
        .fetch_optional(&mut *tx)
        .await
        .context("query upgraded source book for migration")?
        .is_some();
    if !source_exists {
        tx.rollback()
            .await
            .context("rollback import-upgrade migration tx")?;
        return Ok(());
    }

    sqlx::query(
        r#"INSERT INTO BOOK (ID, CREATED_DATE, LAST_MODIFIED_DATE, FILE_LAST_MODIFIED, NAME, URL,
           SERIES_ID, FILE_SIZE, NUMBER, LIBRARY_ID, FILE_HASH, DELETED_DATE, oneshot,
           FILE_HASH_KOREADER) SELECT ?,
           CREATED_DATE, LAST_MODIFIED_DATE, FILE_LAST_MODIFIED, ?, ?, SERIES_ID, FILE_SIZE,
           NUMBER, LIBRARY_ID, FILE_HASH, DELETED_DATE, oneshot, FILE_HASH_KOREADER
         FROM BOOK
         WHERE ID = ?
         ON CONFLICT(ID) DO UPDATE
         SET FILE_LAST_MODIFIED = excluded.FILE_LAST_MODIFIED, NAME = excluded.NAME,
             URL = excluded.URL, SERIES_ID = excluded.SERIES_ID, FILE_SIZE = excluded.FILE_SIZE,
             NUMBER = excluded.NUMBER, LIBRARY_ID = excluded.LIBRARY_ID,
             FILE_HASH = excluded.FILE_HASH, DELETED_DATE = excluded.DELETED_DATE,
             oneshot = excluded.oneshot, FILE_HASH_KOREADER = excluded.FILE_HASH_KOREADER,
             LAST_MODIFIED_DATE = CURRENT_TIMESTAMP"#,
    )
    .bind(new_book_id)
    .bind(destination_name)
    .bind(destination_url)
    .bind(old_book_id)
    .execute(&mut *tx)
    .await
    .context("upsert upgraded destination book identity")?;

    sqlx::query("DELETE FROM BOOK_METADATA WHERE BOOK_ID = ?")
        .bind(new_book_id)
        .execute(&mut *tx)
        .await
        .context("delete destination metadata before upgrade migration: ")?;
    sqlx::query(
        "UPDATE BOOK_METADATA SET BOOK_ID = ?, LAST_MODIFIED_DATE = CURRENT_TIMESTAMP WHERE BOOK_ID = ?",
    )
    .bind(new_book_id)
    .bind(old_book_id)
    .execute(&mut *tx)
    .await
    .context("move book metadata during upgrade migration")?;

    for table in [
        "BOOK_METADATA_AUTHOR",
        "BOOK_METADATA_TAG",
        "BOOK_METADATA_LINK",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE BOOK_ID = ?"
        )))
        .bind(new_book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "delete destination {table} rows before upgrade migration: "
            ))
        })?;

        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET BOOK_ID = ? WHERE BOOK_ID = ?"
        )))
        .bind(new_book_id)
        .bind(old_book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!("move {table} rows during upgrade migration"))
        })?;
    }

    for table in ["MEDIA", "MEDIA_FILE", "MEDIA_PAGE", "READ_PROGRESS"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE BOOK_ID = ?"
        )))
        .bind(new_book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "delete destination {table} rows before upgrade migration: "
            ))
        })?;

        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET BOOK_ID = ? WHERE BOOK_ID = ?"
        )))
        .bind(new_book_id)
        .bind(old_book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!("move {table} rows during upgrade migration"))
        })?;
    }

    sqlx::query("DELETE FROM THUMBNAIL_BOOK WHERE BOOK_ID = ? AND TYPE = ?")
        .bind(new_book_id)
        .bind(ThumbnailType::UserUploaded.persisted_name())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context("delete destination user-uploaded thumbnails before upgrade migration: ")
        })?;
    sqlx::query("UPDATE THUMBNAIL_BOOK SET BOOK_ID = ? WHERE BOOK_ID = ? AND TYPE = ?")
        .bind(new_book_id)
        .bind(old_book_id)
        .bind(ThumbnailType::UserUploaded.persisted_name())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context("move user-uploaded thumbnails during upgrade migration: ")
        })?;

    sqlx::query(
        r#"INSERT INTO READLIST_BOOK (READLIST_ID, BOOK_ID, NUMBER) SELECT READLIST_ID, ?, NUMBER
         FROM READLIST_BOOK
         WHERE BOOK_ID = ?
         ON CONFLICT(READLIST_ID, BOOK_ID) DO NOTHING"#,
    )
    .bind(new_book_id)
    .bind(old_book_id)
    .execute(&mut *tx)
    .await
    .context("copy readlist mapping rows during upgrade migration: ")?;
    sqlx::query("DELETE FROM READLIST_BOOK WHERE BOOK_ID = ?")
        .bind(old_book_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!(error)
                .context("delete source readlist mappings during upgrade migration: ")
        })?;

    sqlx::query("DELETE FROM THUMBNAIL_BOOK WHERE BOOK_ID = ?")
        .bind(old_book_id)
        .execute(&mut *tx)
        .await
        .context("delete source thumbnails during upgrade migration: ")?;

    sqlx::query("DELETE FROM BOOK WHERE ID = ?")
        .bind(old_book_id)
        .execute(&mut *tx)
        .await
        .context("delete source book after upgrade migration")?;

    tx.commit()
        .await
        .context("commit import-upgrade migration tx")?;
    Ok(())
}

fn import_book_url_for_library_root(
    library_root: &Path,
    destination_file: &Path,
) -> anyhow::Result<String> {
    destination_file
        .strip_prefix(library_root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "derive imported book url '{}' from library root '{}'",
                destination_file.display(),
                library_root.display()
            ))
        })
}

async fn persist_book_imported_event(
    pool: &SqlitePool,
    book_id: &str,
    series_id: &str,
    destination_file: &Path,
    source_file: &Path,
    upgrade: bool,
) -> anyhow::Result<()> {
    let event_id = format!("historical-event-{}", random_hex_token(12));
    let destination_name = destination_file.to_string_lossy().to_string();
    let source_name = source_file.to_string_lossy().to_string();
    let upgrade_value = if upgrade { "Yes" } else { "No" };

    let mut tx = pool
        .begin()
        .await
        .context("begin historical-event tx for import")?;

    sqlx::query("INSERT INTO HISTORICAL_EVENT (ID, TYPE, BOOK_ID, SERIES_ID) VALUES (?, ?, ?, ?)")
        .bind(&event_id)
        .bind("BookImported")
        .bind(book_id)
        .bind(series_id)
        .execute(&mut *tx)
        .await
        .context("insert historical BookImported event")?;

    for (key, value) in [
        ("name", destination_name.as_str()),
        ("source", source_name.as_str()),
        ("upgrade", upgrade_value),
    ] {
        sqlx::query("INSERT INTO HISTORICAL_EVENT_PROPERTIES (ID, KEY, VALUE) VALUES (?, ?, ?)")
            .bind(&event_id)
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "insert historical event property '{key}' for BookImported: "
                ))
            })?;
    }

    tx.commit()
        .await
        .context("commit historical-event tx for import")?;

    Ok(())
}

#[cfg(test)]
mod tests;
