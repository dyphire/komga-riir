use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use komga_infrastructure_base::stored_paths::resolve_rooted_path;

use super::scan_models::{
    ExistingScannedBookRow, LibraryScanConfig, ScannedBookRow, ScannedSidecarRow,
    ScannedSidecarSource, ScannedSidecarType,
};

pub(super) struct ScannedDirectory {
    pub path: PathBuf,
    pub metadata: fs::Metadata,
    pub file_entries: Vec<ScannedDirectoryFileEntry>,
}

pub(super) struct ScannedDirectoryFileEntry {
    pub path: PathBuf,
    pub metadata: fs::Metadata,
}

pub(super) fn collect_series_directories(
    current: &Path,
    scan_config: &LibraryScanConfig,
    discovered: &mut Vec<ScannedDirectory>,
    failed_directories: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if is_hidden_path(current)
        || is_library_path_excluded(current, &scan_config.scan_directory_exclusions)
    {
        return Ok(());
    }

    let entries = match fs::read_dir(current) {
        Ok(entries) => entries,
        Err(error) => {
            // The directory is unreachable (e.g. WebDAV hiccup): record it so
            // its contents are treated as "unknown" instead of "missing" and
            // keep scanning the rest of the library.
            failed_directories.push(current.to_path_buf());
            tracing::warn!(
                path = %current.display(),
                error = %format!("{error:?}"),
                "scan: skipping unreachable directory"
            );
            return Ok(());
        }
    };

    let mut has_supported_book = false;
    let mut children = Vec::new();
    let mut file_entries = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // Reading this entry failed: the directory contents are
                // incomplete, so protect the whole directory from deletion.
                failed_directories.push(current.to_path_buf());
                tracing::warn!(
                    path = %current.display(),
                    error = %format!("{error:?}"),
                    "scan: directory read interrupted, treating contents as unknown"
                );
                break;
            }
        };
        let path = entry.path();
        if is_hidden_path(path.as_path()) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                // This entry's type is unknown: protect this exact path.
                failed_directories.push(path.clone());
                tracing::warn!(
                    path = %path.display(),
                    error = %format!("{error:?}"),
                    "scan: skipping entry with unreadable metadata"
                );
                continue;
            }
        };
        if metadata.is_dir() {
            if is_library_path_excluded(path.as_path(), &scan_config.scan_directory_exclusions) {
                continue;
            }
            children.push(path);
        } else if metadata.is_file() {
            if is_supported_book_file(path.as_path(), scan_config) {
                has_supported_book = true;
            }
            file_entries.push(ScannedDirectoryFileEntry { path, metadata });
        }
    }

    if has_supported_book {
        let directory_metadata = fs::metadata(current).map_err(|error| {
            anyhow::anyhow!("{error:?}").context(format!(
                "failed to read series directory metadata for '{}'",
                current.display()
            ))
        })?;
        discovered.push(ScannedDirectory {
            path: current.to_path_buf(),
            metadata: directory_metadata,
            file_entries,
        });
    }

    for child in children {
        collect_series_directories(child.as_path(), scan_config, discovered, failed_directories)?;
    }

    Ok(())
}

pub(super) fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

pub(super) fn is_supported_book_file(path: &Path, scan_config: &LibraryScanConfig) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };

    match extension.to_ascii_lowercase().as_str() {
        "cbz" | "zip" | "cbr" | "rar" => scan_config.scan_cbx,
        "pdf" => scan_config.scan_pdf,
        "epub" | "mobi" => scan_config.scan_epub,
        _ => false,
    }
}

pub(super) fn path_file_name_utf8(path: &Path) -> anyhow::Result<&str> {
    path.file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(format!(
                "path '{}' has no valid UTF-8 file name",
                path.display()
            ))
        })
}

pub(super) fn path_file_stem_utf8(path: &Path) -> anyhow::Result<&str> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(format!(
                "path '{}' has no valid UTF-8 file stem",
                path.display()
            ))
        })
}

pub(super) fn is_library_path_excluded(path: &Path, exclusions: &[String]) -> bool {
    let path_key = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    exclusions.iter().any(|entry| {
        let exclusion = entry.replace('\\', "/").to_ascii_lowercase();
        if exclusion.is_empty() {
            return false;
        }

        path_key.contains(&exclusion)
    })
}

pub(super) fn resolve_oneshot_series_id(
    existing_books_by_url: &HashMap<String, ExistingScannedBookRow>,
    library_root: &Path,
    book_url: &str,
) -> String {
    existing_books_by_url
        .get(&scanner_url_key(library_root, book_url))
        .map(|existing| existing.series_id.clone())
        .unwrap_or_else(|| {
            let resolved_path = resolve_rooted_path(library_root, book_url);
            route_safe_scanner_id("series", resolved_path.as_path())
        })
}

pub(super) fn scanner_url_key(root: &Path, stored_url: &str) -> String {
    normalize_scanner_path_key(resolve_rooted_path(root, stored_url).as_path())
}

pub(super) fn normalize_scanner_path_key(path: &Path) -> String {
    let normalized = path.components().collect::<PathBuf>();
    #[cfg(windows)]
    {
        normalized
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized.to_string_lossy().to_string()
    }
}

pub(super) fn route_safe_scanner_id(prefix: &str, path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalize_scanner_path_key(path).hash(&mut hasher);
    format!("{prefix}-{:016x}", hasher.finish())
}

pub(super) fn build_sidecars(
    series_url: &str,
    books: &[ScannedBookRow],
    sidecar_candidates: &[(PathBuf, fs::Metadata)],
    include_series_sidecars: bool,
) -> anyhow::Result<Vec<ScannedSidecarRow>> {
    let mut sidecars = Vec::new();

    'candidate: for (path, metadata) in sidecar_candidates {
        let file_name = path_file_name_utf8(path)?;
        let file_stem = path_file_stem_utf8(path)?;

        let is_image = matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("jpg")
                | Some("jpeg")
                | Some("png")
                | Some("tbn")
                | Some("webp")
                | Some("gif")
                | Some("avif")
        );

        if include_series_sidecars && is_image {
            let base = file_stem.to_ascii_lowercase();
            if matches!(
                base.as_str(),
                "cover" | "default" | "folder" | "poster" | "series"
            ) {
                sidecars.push(ScannedSidecarRow {
                    url: path.to_string_lossy().to_string(),
                    parent_url: series_url.to_string(),
                    last_modified_unix_seconds: metadata_updated_unix_seconds(metadata, path)?,
                    source: ScannedSidecarSource::Series,
                    sidecar_type: ScannedSidecarType::Artwork,
                });
                continue;
            }
        }

        if include_series_sidecars && file_name.eq_ignore_ascii_case("series.json") {
            sidecars.push(ScannedSidecarRow {
                url: path.to_string_lossy().to_string(),
                parent_url: series_url.to_string(),
                last_modified_unix_seconds: metadata_updated_unix_seconds(metadata, path)?,
                source: ScannedSidecarSource::Series,
                sidecar_type: ScannedSidecarType::Metadata,
            });
            continue;
        }

        for book in books {
            if is_image && is_book_artwork_sidecar(file_stem, &book.book_name) {
                sidecars.push(ScannedSidecarRow {
                    url: path.to_string_lossy().to_string(),
                    parent_url: book.book_url.clone(),
                    last_modified_unix_seconds: metadata_updated_unix_seconds(metadata, path)?,
                    source: ScannedSidecarSource::Book,
                    sidecar_type: ScannedSidecarType::Artwork,
                });
                continue 'candidate;
            }
        }
    }

    Ok(sidecars)
}

pub(super) fn is_book_artwork_sidecar(base_name: &str, book_name: &str) -> bool {
    let base_name = base_name.to_ascii_lowercase();
    let book_name = book_name.to_ascii_lowercase();
    if base_name == book_name {
        return true;
    }

    base_name
        .strip_prefix(&format!("{book_name}-"))
        .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

fn to_unix_seconds(time: std::time::SystemTime, path: &Path) -> anyhow::Result<i64> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "filesystem timestamp for '{}' is outside i64 range",
                path.display()
            ))
        }),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "filesystem timestamp for '{}' is outside i64 range",
                    path.display()
                ))
            })?;
            Ok(-seconds)
        }
    }
}

pub(super) fn metadata_updated_unix_seconds(
    metadata: &fs::Metadata,
    path: &Path,
) -> anyhow::Result<i64> {
    [metadata.created().ok(), metadata.modified().ok()]
        .into_iter()
        .flatten()
        .map(|time| to_unix_seconds(time, path))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or_else(|| {
            anyhow::anyhow!(format!(
                "failed to read created or modified timestamp for '{}'",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_url_key_normalizes_windows_and_relative_path_shapes() {
        #[cfg(windows)]
        {
            let root = PathBuf::from("C:/library");
            assert_eq!(
                scanner_url_key(root.as_path(), "oneshots/existing.cbz"),
                scanner_url_key(root.as_path(), "C:\\library\\oneshots\\existing.cbz"),
                "scanner url keys should match regardless of separator style so oneshot restoration stays platform-neutral",
            );
        }

        #[cfg(not(windows))]
        {
            let root = PathBuf::from("/library");
            assert_eq!(
                scanner_url_key(root.as_path(), "oneshots/existing.cbz"),
                "/library/oneshots/existing.cbz",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scanner_path_helpers_reject_supported_book_paths_without_utf8_file_stems() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path =
            PathBuf::from("/library/Series 1").join(OsString::from_vec(b"\xff.cbz".to_vec()));
        let scan_config = LibraryScanConfig {
            root: "/library".to_string(),
            scan_cbx: true,
            scan_pdf: true,
            scan_epub: true,
            scan_force_modified_time: false,
            oneshots_directory: None,
            scan_directory_exclusions: Vec::new(),
        };

        assert!(
            is_supported_book_file(path.as_path(), &scan_config),
            "fixture should still be a supported CBZ path before UTF-8 name extraction",
        );
        let error = path_file_stem_utf8(path.as_path())
            .expect_err("scanner should reject non-UTF-8 book stems");
        assert!(
            error.to_string().contains("valid UTF-8 file stem"),
            "{error}"
        );
    }

    #[test]
    fn scanner_treats_mobi_as_epub_when_scan_epub_is_enabled() {
        let scan_config = LibraryScanConfig {
            root: "/library".to_string(),
            scan_cbx: false,
            scan_pdf: false,
            scan_epub: true,
            scan_force_modified_time: false,
            oneshots_directory: None,
            scan_directory_exclusions: Vec::new(),
        };

        assert!(is_supported_book_file(
            Path::new("/library/Series/book.MOBI"),
            &scan_config
        ));

        let mut disabled = scan_config.clone();
        disabled.scan_epub = false;
        assert!(!is_supported_book_file(
            Path::new("/library/Series/book.mobi"),
            &disabled
        ));
    }
}
