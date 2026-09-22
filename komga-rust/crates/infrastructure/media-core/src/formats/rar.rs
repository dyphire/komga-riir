use std::fs;
use std::io::Read;
use std::path::Path;

use unrar::Archive;

const RAR4_SIGNATURE: &[u8] = b"Rar!\x1A\x07\x00";
const RAR5_SIGNATURE: &[u8] = b"Rar!\x1A\x07\x01\x00";

pub fn detect_rar_media_type(path: &Path) -> &'static str {
    let mut header = [0; 8];
    let Ok(bytes_read) = fs::File::open(path).and_then(|mut file| file.read(&mut header)) else {
        return "application/x-rar-compressed";
    };
    let header = &header[..bytes_read];

    if header.starts_with(RAR5_SIGNATURE) {
        "application/x-rar-compressed; version=5"
    } else if header.starts_with(RAR4_SIGNATURE) {
        "application/x-rar-compressed; version=4"
    } else {
        "application/x-rar-compressed"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RarEntryRecord {
    pub file_name: String,
    pub unpacked_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RarEntryBytesRecord {
    pub file_name: String,
    pub unpacked_size: u64,
    pub bytes: Vec<u8>,
}

pub fn list_rar_entries(path: &Path) -> anyhow::Result<Vec<RarEntryRecord>> {
    let mut archive = Archive::new(path).open_for_listing().map_err(|error| {
        anyhow::anyhow!(error).context(format!("open rar for listing '{}'", path.display()))
    })?;

    let mut entries = Vec::new();
    for entry in archive.by_ref() {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(error).context(format!("read rar entry '{}'", path.display()))
        })?;
        if entry.is_directory() {
            continue;
        }
        entries.push(RarEntryRecord {
            file_name: entry.filename.to_string_lossy().replace('\\', "/"),
            unpacked_size: entry.unpacked_size,
        });
    }
    drop(archive);

    Ok(entries)
}

pub fn read_rar_entries_bytes(path: &Path) -> anyhow::Result<Vec<RarEntryBytesRecord>> {
    let mut archive = Archive::new(path).open_for_processing().map_err(|error| {
        anyhow::anyhow!(error).context(format!("open rar for processing '{}'", path.display()))
    })?;

    let mut entries = Vec::new();
    loop {
        let Some(header) = archive.read_header().map_err(|error| {
            anyhow::anyhow!(error).context(format!("read rar header '{}'", path.display()))
        })?
        else {
            break;
        };

        let file_name = header.entry().filename.to_string_lossy().replace('\\', "/");
        let unpacked_size = header.entry().unpacked_size;
        if header.entry().is_directory() {
            archive = header.skip().map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "skip rar entry '{file_name}' in '{}'",
                    path.display()
                ))
            })?;
            continue;
        }

        let (bytes, rest) = header.read().map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read rar entry '{}' from '{}'",
                file_name,
                path.display()
            ))
        })?;
        archive = rest;
        entries.push(RarEntryBytesRecord {
            file_name,
            unpacked_size,
            bytes,
        });
    }

    Ok(entries)
}

pub fn read_rar_entry_bytes(path: &Path, entry_name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let matched_bytes = {
        let mut archive = Archive::new(path).open_for_processing().map_err(|error| {
            anyhow::anyhow!(error)
                .context(format!("open rar for processing '{}'", path.display()))
        })?;

        loop {
            let Some(header) = archive.read_header().map_err(|error| {
                anyhow::anyhow!(error).context(format!("read rar header '{}'", path.display()))
            })?
            else {
                break None;
            };

            let current_name = header.entry().filename.to_string_lossy().replace('\\', "/");
            if current_name == entry_name {
                let (data, rest) = header.read().map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "read rar entry '{}' from '{}'",
                        entry_name,
                        path.display()
                    ))
                })?;
                drop(rest);
                break Some(data);
            }

            archive = header.skip().map_err(|error| {
                anyhow::anyhow!(error).context(format!(
                    "skip rar entry '{}' in '{}'",
                    current_name,
                    path.display()
                ))
            })?;
        }
    };

    Ok(matched_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn detect_rar_media_type_recognizes_versioned_headers() {
        let rar4 = unique_temp_path("komga-rar4-signature");
        let rar5 = unique_temp_path("komga-rar5-signature");
        fs::write(&rar4, RAR4_SIGNATURE).expect("rar4 signature fixture should be written");
        fs::write(&rar5, RAR5_SIGNATURE).expect("rar5 signature fixture should be written");

        assert_eq!(
            detect_rar_media_type(&rar4),
            "application/x-rar-compressed; version=4"
        );
        assert_eq!(
            detect_rar_media_type(&rar5),
            "application/x-rar-compressed; version=5"
        );

        let _ = fs::remove_file(rar4);
        let _ = fs::remove_file(rar5);
    }
}
