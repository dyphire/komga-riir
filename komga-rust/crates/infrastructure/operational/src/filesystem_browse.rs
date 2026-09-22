use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use komga_application::operational::{
    FilesystemBrowseError, FilesystemBrowsePort, FilesystemBrowseRequest,
    FilesystemDirectoryListing, FilesystemEntry, FilesystemEntryType,
};

#[derive(Clone, Default)]
pub struct FilesystemBrowseAccess;

impl FilesystemBrowsePort for FilesystemBrowseAccess {
    fn browse(
        &self,
        request: FilesystemBrowseRequest,
    ) -> Result<FilesystemDirectoryListing, FilesystemBrowseError> {
        browse_directory(request)
    }
}

fn browse_directory(
    request: FilesystemBrowseRequest,
) -> Result<FilesystemDirectoryListing, FilesystemBrowseError> {
    if request.path.is_empty() {
        return Ok(FilesystemDirectoryListing {
            parent: None,
            directories: root_directory_entries(),
            files: Vec::new(),
        });
    }

    let requested_path = PathBuf::from(&request.path);
    if !requested_path.is_absolute() {
        return Err(FilesystemBrowseError::BadRequest);
    }

    let directory = listing_directory(&requested_path)?;
    ensure_existing_directory(&directory)?;

    let directories =
        list_directory_entries(&directory, true).map_err(|_| FilesystemBrowseError::Internal)?;
    let files = if request.show_files {
        list_directory_entries(&directory, false).map_err(|_| FilesystemBrowseError::Internal)?
    } else {
        Vec::new()
    };

    Ok(FilesystemDirectoryListing {
        parent: Some(parent_value(&requested_path)),
        directories,
        files,
    })
}

fn listing_directory(requested_path: &Path) -> Result<PathBuf, FilesystemBrowseError> {
    if path_is_existing_directory(requested_path)? {
        return Ok(requested_path.to_path_buf());
    }

    Ok(requested_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| requested_path.to_path_buf()))
}

fn ensure_existing_directory(path: &Path) -> Result<(), FilesystemBrowseError> {
    if path_is_existing_directory(path)? {
        Ok(())
    } else {
        Err(FilesystemBrowseError::BadRequest)
    }
}

fn path_is_existing_directory(path: &Path) -> Result<bool, FilesystemBrowseError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(FilesystemBrowseError::Internal),
    }
}

fn parent_value(requested_path: &Path) -> String {
    requested_path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn root_directory_entries() -> Vec<FilesystemEntry> {
    current_root_directories()
        .into_iter()
        .map(|root| FilesystemEntry {
            entry_type: FilesystemEntryType::Directory,
            name: root.clone(),
            path: root,
        })
        .collect()
}

#[cfg(windows)]
fn current_root_directories() -> Vec<String> {
    ('A'..='Z')
        .map(|drive| format!("{drive}:\\"))
        .filter(|root| Path::new(root).exists())
        .collect()
}

#[cfg(not(windows))]
fn current_root_directories() -> Vec<String> {
    vec![std::path::MAIN_SEPARATOR.to_string()]
}

fn list_directory_entries(
    path: &Path,
    directories_only: bool,
) -> anyhow::Result<Vec<FilesystemEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|error| {
        anyhow::anyhow!(error).context(format!("read filesystem directory '{}'", path.display()))
    })? {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read filesystem directory entry '{}'",
                path.display()
            ))
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry_is_hidden(&entry, &name)? {
            continue;
        }

        let file_type = entry.file_type().map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read filesystem directory entry type '{}'",
                entry.path().display()
            ))
        })?;
        let is_directory = file_type.is_dir();
        if directories_only != is_directory {
            continue;
        }

        let entry_path = entry.path();
        let entry_type = if is_directory {
            FilesystemEntryType::Directory
        } else {
            FilesystemEntryType::File
        };

        entries.push(FilesystemEntry {
            entry_type,
            name,
            path: entry_path.to_string_lossy().to_string(),
        });
    }

    entries.sort_by_key(|entry| entry.path.to_lowercase());
    Ok(entries)
}

#[cfg(windows)]
fn entry_is_hidden(entry: &fs::DirEntry, name: &str) -> anyhow::Result<bool> {
    if name.starts_with('.') {
        return Ok(true);
    }
    entry
        .metadata()
        .map(|metadata| metadata.file_attributes() & 0x2 != 0)
        .map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read filesystem directory entry metadata '{}'",
                entry.path().display()
            ))
        })
}

#[cfg(not(windows))]
fn entry_is_hidden(_entry: &fs::DirEntry, name: &str) -> anyhow::Result<bool> {
    Ok(name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::{browse_directory, list_directory_entries};
    #[cfg(unix)]
    use komga_application::operational::FilesystemBrowseError;
    use komga_application::operational::{FilesystemBrowseRequest, FilesystemEntryType};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn list_directory_entries_propagates_read_directory_errors() {
        let path = unique_temp_path("komga-browser-not-directory");
        fs::write(&path, b"not-a-directory").expect("file fixture should be written");

        let error = list_directory_entries(&path, true)
            .expect_err("read_dir errors must not become empty listings");

        assert!(
            error.to_string().contains("read filesystem directory"),
            "unexpected filesystem browse error: {error}"
        );

        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn browse_directory_propagates_path_metadata_errors() {
        let parent_file = unique_temp_path("komga-browser-parent-file");
        fs::write(&parent_file, b"not-a-directory").expect("parent file fixture should be written");
        let path = parent_file.join("child");

        let result = browse_directory(FilesystemBrowseRequest {
            path: path.to_string_lossy().to_string(),
            show_files: false,
        });

        assert_eq!(result, Err(FilesystemBrowseError::Internal));

        let _ = fs::remove_file(parent_file);
    }

    #[test]
    fn browse_directory_keeps_file_path_as_parent_listing() {
        let directory = unique_temp_path("komga-browser-file-parent");
        fs::create_dir_all(&directory).expect("directory fixture should be created");
        let file = directory.join("book.cbz");
        fs::write(&file, b"book").expect("file fixture should be written");

        let listing = browse_directory(FilesystemBrowseRequest {
            path: file.to_string_lossy().to_string(),
            show_files: true,
        })
        .expect("file path should browse its parent directory");

        assert_eq!(
            listing.parent,
            Some(directory.to_string_lossy().to_string())
        );
        assert!(listing.files.iter().any(|entry| {
            entry.entry_type == FilesystemEntryType::File && entry.path == file.to_string_lossy()
        }));

        let _ = fs::remove_dir_all(directory);
    }
}
