use komga_epub::{
    NormalizedPublication, normalize_mobi, parse_epub_guide_cover_href, parse_epub_manifest_items,
    parse_epub_metadata_cover_id, parse_epub_rootfile_path,
};
use regex::Regex;
use std::fs::File;
use std::io::ErrorKind;
use std::io::{Read, Seek};
use std::path::Path;

use komga_application::media_assets::{
    BookMediaRecord, EpubCoverImage, EpubNavigationExtension, EpubNavigationLink,
    EpubNavigationPosition, book_media_is_epub,
};
use zip::ZipArchive;
use zip::result::ZipError;

use crate::content::page_rendering as page_content;

pub async fn read_epub_publication_bytes(
    media: &BookMediaRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !book_media_is_epub(media) {
        return Ok(None);
    }
    if is_mobi_path(media.file_path.as_path()) {
        return with_mobi_publication(media.file_path.as_path(), |publication| {
            publication
                .epub_bytes()
                .map_err(|error| anyhow::anyhow!(error))
        })
        .await
        .map(Some);
    }
    page_content::read_media_file_bytes(&media.file_path).await
}

pub async fn read_epub_resource_bytes(
    epub_path: &Path,
    resource_name: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    if is_mobi_path(epub_path) {
        let resource_name = resource_name.to_string();
        return with_mobi_publication(epub_path, move |publication| {
            publication
                .resource_bytes(resource_name.as_str())
                .map_err(|error| anyhow::anyhow!(error))
        })
        .await;
    }
    read_epub_resource_from_archive_path(epub_path, resource_name).await
}

async fn read_epub_resource_from_archive_path(
    epub_path: &Path,
    resource_name: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = epub_path.to_path_buf();
    let resource_name = resource_name.to_string();
    let display_path = path.display().to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<u8>>> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "open EPUB '{}': {error}",
                    path.display()
                )));
            }
        };
        let mut archive = ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!("open EPUB archive '{}'", path.display()))
        })?;
        read_zip_entry_bytes_result(&mut archive, &resource_name, &path)
    })
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!("join EPUB resource read for '{display_path}'"))
    })?
}

pub fn decode_epub_navigation_extension(blob: &[u8]) -> anyhow::Result<EpubNavigationExtension> {
    let extension = komga_epub::decode_epub_navigation_extension(blob)?;
    let positions = extension
        .positions
        .into_iter()
        .map(EpubNavigationPosition::from_raw)
        .collect();

    Ok(EpubNavigationExtension {
        positions,
        is_fixed_layout: extension.is_fixed_layout,
        toc: extension
            .toc
            .into_iter()
            .map(map_epub_navigation_link)
            .collect(),
        landmarks: extension
            .landmarks
            .into_iter()
            .map(map_epub_navigation_link)
            .collect(),
        page_list: extension
            .page_list
            .into_iter()
            .map(map_epub_navigation_link)
            .collect(),
    })
}

fn map_epub_navigation_link(link: komga_epub::EpubNavigationLink) -> EpubNavigationLink {
    EpubNavigationLink {
        title: link.title,
        href: link.href,
        children: link
            .children
            .into_iter()
            .map(map_epub_navigation_link)
            .collect(),
    }
}

fn extract_image_from_html_page<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    page_path: &str,
    opf_dir: &str,
    manifest: &indexmap::IndexMap<String, komga_epub::EpubManifestItem>,
    archive_path: &Path,
) -> anyhow::Result<Option<komga_epub::EpubManifestItem>> {
    let html_bytes = read_zip_entry_bytes_normalized_result(archive, page_path, archive_path)?;
    let html_content = match html_bytes {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => return Ok(None),
    };

    let img_src_regex =
        Regex::new(r#"<img[^>]*\ssrc\s*=\s*["']([^"']+)["']"#).expect("valid img src regex");
    let svg_xlink_regex = Regex::new(r#"<svg:image[^>]*\sxlink:href\s*=\s*["']([^"']+)["']"#)
        .expect("valid svg xlink regex");
    let svg_image_xlink_regex = Regex::new(r#"<image[^>]*\sxlink:href\s*=\s*["']([^"']+)["']"#)
        .expect("valid image xlink regex");
    let svg_image_regex =
        Regex::new(r#"<image[^>]*\shref\s*=\s*["']([^"']+)["']"#).expect("valid image href regex");

    let img_href = img_src_regex
        .captures(&html_content)
        .and_then(|cap| cap.get(1))
        .or_else(|| {
            svg_xlink_regex
                .captures(&html_content)
                .and_then(|cap| cap.get(1))
        })
        .or_else(|| {
            svg_image_xlink_regex
                .captures(&html_content)
                .and_then(|cap| cap.get(1))
        })
        .or_else(|| {
            svg_image_regex
                .captures(&html_content)
                .and_then(|cap| cap.get(1))
        });

    let img_href = match img_href {
        Some(href) => href.as_str(),
        None => return Ok(None),
    };

    let parent_dir = std::path::Path::new(page_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let resolved_img_path = if parent_dir.is_empty() {
        img_href.to_string()
    } else {
        format!("{}/{}", parent_dir, img_href)
    };
    let normalized_img_path = komga_epub::normalize_epub_resource_href(opf_dir, &resolved_img_path);

    Ok(manifest
        .values()
        .find(|item| {
            komga_epub::normalize_epub_resource_href(opf_dir, &item.href) == normalized_img_path
        })
        .cloned())
}

pub async fn load_epub_cover_bytes(
    media: &BookMediaRecord,
) -> anyhow::Result<Option<EpubCoverImage>> {
    if !book_media_is_epub(media) {
        return Ok(None);
    }
    if is_mobi_path(media.file_path.as_path()) {
        return with_mobi_publication(media.file_path.as_path(), load_mobi_cover_bytes).await;
    }
    let path = media.file_path.clone();
    let display_path = path.display().to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Option<EpubCoverImage>> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow::anyhow!(format!(
                    "open EPUB '{}': {error}",
                    path.display()
                )));
            }
        };
        let mut archive = ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!("open EPUB archive '{}'", path.display()))
        })?;
        let Some(container_xml) =
            read_zip_entry_bytes_normalized_result(&mut archive, "META-INF/container.xml", &path)?
        else {
            return Ok(None);
        };
        let Some(rootfile_path) = parse_epub_rootfile_path(&container_xml)? else {
            return Ok(None);
        };
        let Some(package_document) =
            read_zip_entry_bytes_normalized_result(&mut archive, &rootfile_path, &path)?
        else {
            return Ok(None);
        };
        let manifest = parse_epub_manifest_items(&package_document, &rootfile_path)?;
        let metadata_cover_item = parse_epub_metadata_cover_id(&package_document)?
            .and_then(|cover_id| manifest.get(&cover_id).cloned());
        let guide_cover_href = parse_epub_guide_cover_href(&package_document)?;

        let guide_cover_item = guide_cover_href.and_then(|href| {
            let normalized_href = komga_epub::normalize_epub_resource_href(&rootfile_path, &href);
            if normalized_href.ends_with(".xhtml") || normalized_href.ends_with(".html") {
                extract_image_from_html_page(
                    &mut archive,
                    &normalized_href,
                    &rootfile_path,
                    &manifest,
                    &path,
                )
                .ok()
                .flatten()
            } else {
                manifest
                    .values()
                    .find(|item| {
                        komga_epub::normalize_epub_resource_href(&rootfile_path, &item.href)
                            == normalized_href
                    })
                    .cloned()
            }
        });

        let cover_items = [
            manifest
                .values()
                .find(|item| {
                    item.properties
                        .split_ascii_whitespace()
                        .any(|property| property.eq_ignore_ascii_case("cover-image"))
                })
                .cloned(),
            metadata_cover_item,
            manifest
                .values()
                .find(|item| item.id == "cover-image")
                .cloned(),
            guide_cover_item,
            manifest
                .values()
                .filter(|item| {
                    item.id.to_lowercase().contains("cover")
                        && item.media_type.starts_with("image/")
                })
                .min_by(|a, b| {
                    a.id.to_lowercase()
                        .cmp(&b.id.to_lowercase())
                        .then_with(|| a.href.cmp(&b.href))
                })
                .cloned(),
            manifest
                .values()
                .filter(|item| {
                    item.href.to_lowercase().contains("cover")
                        && item.media_type.starts_with("image/")
                })
                .min_by(|a, b| {
                    a.href
                        .to_lowercase()
                        .cmp(&b.href.to_lowercase())
                        .then_with(|| a.id.cmp(&b.id))
                })
                .cloned(),
        ];

        for cover_item in cover_items.into_iter().flatten() {
            let Some(bytes) =
                read_zip_entry_bytes_normalized_result(&mut archive, &cover_item.href, &path)?
            else {
                continue;
            };
            return Ok(Some(EpubCoverImage {
                bytes,
                media_type: cover_item.media_type,
            }));
        }

        Ok(None)
    })
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!("join EPUB cover read for '{display_path}'"))
    })?
}

pub async fn load_epub_package_document(
    media: &BookMediaRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !book_media_is_epub(media) {
        return Ok(None);
    }
    if is_mobi_path(media.file_path.as_path()) {
        return with_mobi_publication(media.file_path.as_path(), |publication| {
            publication
                .resource_bytes("OEBPS/content.opf")
                .map_err(|error| anyhow::anyhow!(error))
        })
        .await;
    }
    let path = media.file_path.clone();
    let display_path = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<u8>>> {
        let file = File::open(&path).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to open EPUB package source '{}'",
                path.display()
            ))
        })?;
        let mut archive = ZipArchive::new(file).map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "failed to open EPUB package archive '{}'",
                path.display()
            ))
        })?;
        let container_xml = read_zip_entry_bytes_normalized_required(
            &mut archive,
            "META-INF/container.xml",
            path.as_path(),
        )?;
        let rootfile_path = parse_epub_rootfile_path(&container_xml)?.ok_or_else(|| {
            anyhow::anyhow!(format!(
                "failed to resolve EPUB package document rootfile in '{}'",
                path.display()
            ))
        })?;
        let package_document =
            read_zip_entry_bytes_normalized_required(&mut archive, &rootfile_path, path.as_path())?;
        Ok(Some(package_document))
    })
    .await
    .map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to join EPUB package document load for '{display_path}': "
        ))
    })?;
    result.map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to load EPUB package document '{display_path}': "
        ))
    })
}

fn is_mobi_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mobi"))
}

async fn with_mobi_publication<T, F>(path: &Path, operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(NormalizedPublication) -> anyhow::Result<T> + Send + 'static,
{
    let path = path.to_path_buf();
    let display_path = path.display().to_string();
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path).map_err(|error| {
            anyhow::anyhow!(error).context(format!("read MOBI source '{}'", path.display()))
        })?;
        let publication = normalize_mobi(&bytes).map_err(|error| {
            anyhow::anyhow!(error).context(format!("normalize MOBI source '{}'", path.display()))
        })?;
        operation(publication)
    })
    .await
    .map_err(|error| anyhow::anyhow!(error).context("join MOBI normalization"))?
    .map_err(|error| error.context(format!("load MOBI publication '{display_path}'")))
}

fn load_mobi_cover_bytes(
    publication: NormalizedPublication,
) -> anyhow::Result<Option<EpubCoverImage>> {
    Ok(publication
        .resources
        .into_iter()
        .find(|resource| resource.path.contains("/cover."))
        .map(|resource| EpubCoverImage {
            bytes: resource.bytes,
            media_type: resource.media_type,
        }))
}

fn read_zip_entry_bytes_normalized_result<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
    archive_path: &Path,
) -> anyhow::Result<Option<Vec<u8>>> {
    let normalized = path.trim_start_matches('/');
    for entry_name in std::iter::once(path).chain((normalized != path).then_some(normalized)) {
        if let Some(bytes) = read_zip_entry_bytes_result(archive, entry_name, archive_path)? {
            return Ok(Some(bytes));
        }
    }

    Ok(None)
}

fn read_zip_entry_bytes_result<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    entry_name: &str,
    archive_path: &Path,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut entry = match archive.by_name(entry_name) {
        Ok(entry) => entry,
        Err(ZipError::FileNotFound) => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "failed to open EPUB archive entry '{entry_name}' from '{}': {error}",
                archive_path.display()
            )));
        }
    };
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).map_err(|error| {
        anyhow::anyhow!(error).context(format!(
            "failed to read EPUB archive entry '{entry_name}' from '{}'",
            archive_path.display()
        ))
    })?;
    Ok(Some(bytes))
}

fn read_zip_entry_bytes_normalized_required<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    path: &str,
    archive_path: &Path,
) -> anyhow::Result<Vec<u8>> {
    read_zip_entry_bytes_normalized_result(archive, path, archive_path)?.ok_or_else(|| {
        anyhow::anyhow!(format!(
            "missing EPUB archive entry '{path}' in '{}'",
            archive_path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use komga_application::media_assets::BookMediaRecord;
    use serde_json::{Value, json};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipArchive, ZipWriter};

    use super::{decode_epub_navigation_extension, load_epub_cover_bytes};

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn build_test_zip_archive(entries: Vec<(String, Vec<u8>)>) -> anyhow::Result<Vec<u8>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);

        for (file_name, bytes) in entries {
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer
                .start_file(file_name.as_str(), options)
                .map_err(|error| {
                    anyhow::anyhow!(error).context(format!("start zip entry '{file_name}'"))
                })?;
            writer.write_all(&bytes).map_err(|error| {
                anyhow::anyhow!(error).context(format!("write zip entry '{file_name}'"))
            })?;
        }

        writer
            .finish()
            .map(|cursor| cursor.into_inner())
            .context("finalize zip archive")
    }

    fn epub_media(file_path: PathBuf) -> BookMediaRecord {
        BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.epub".to_string(),
            file_path,
            media_type: "application/epub+zip".to_string(),
            page_count: 0,
        }
    }

    fn mobi_media(file_path: PathBuf) -> BookMediaRecord {
        BookMediaRecord {
            library_id: "lib".to_string(),
            file_name: "book.mobi".to_string(),
            file_path,
            media_type: "application/x-mobipocket-ebook".to_string(),
            page_count: 0,
        }
    }

    fn basic_container_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_extracts_manifest_cover_image() {
        let file_path = unique_temp_path("komga-media-epub-cover");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="cover" href="images/cover.png" media-type="image/png" properties="cover-image"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/cover.png".to_string(),
                b"cover-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist");
        assert_eq!(cover.bytes, b"cover-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_falls_back_to_guide_cover_image() {
        let file_path = unique_temp_path("komga-media-epub-cover-guide");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="2.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="cover-img" href="images/cover.jpg" media-type="image/jpeg"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
  <guide>
    <reference type="cover" href="images/cover.jpg"/>
  </guide>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/cover.jpg".to_string(),
                b"cover-jpg-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via guide fallback");
        assert_eq!(cover.bytes, b"cover-jpg-bytes");
        assert_eq!(cover.media_type, "image/jpeg");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_falls_back_to_guide_xhtml_with_img() {
        let file_path = unique_temp_path("komga-media-epub-cover-guide-xhtml");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="2.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="cover-img" href="images/cover.png" media-type="image/png"/>
    <item id="cover-page" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
  <guide>
    <reference type="cover" href="cover.xhtml"/>
  </guide>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/cover.xhtml".to_string(),
                br#"<?xml version="1.0"?><html><body><img src="images/cover.png"/></body></html>"#
                    .to_vec(),
            ),
            (
                "OEBPS/images/cover.png".to_string(),
                b"cover-png-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via guide xhtml fallback");
        assert_eq!(cover.bytes, b"cover-png-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_falls_back_to_id_containing_cover() {
        let file_path = unique_temp_path("komga-media-epub-cover-id-fallback");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="my-cover-image" href="images/cover.jpg" media-type="image/jpeg"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/cover.jpg".to_string(),
                b"cover-jpg-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via id fallback");
        assert_eq!(cover.bytes, b"cover-jpg-bytes");
        assert_eq!(cover.media_type, "image/jpeg");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_falls_back_to_href_containing_cover() {
        let file_path = unique_temp_path("komga-media-epub-cover-href-fallback");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="img1" href="images/front-cover.png" media-type="image/png"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/front-cover.png".to_string(),
                b"cover-png-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via href fallback");
        assert_eq!(cover.bytes, b"cover-png-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_skips_missing_high_priority_candidate() {
        let file_path = unique_temp_path("komga-media-epub-cover-missing-priority");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <metadata>
    <meta name="cover" content="legacy-cover"/>
  </metadata>
  <manifest>
    <item id="legacy-cover" href="images/missing.png" media-type="image/png"/>
    <item id="front" href="images/front-cover.png" media-type="image/png"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/front-cover.png".to_string(),
                b"front-cover-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should fall back after missing metadata entry");
        assert_eq!(cover.bytes, b"front-cover-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn read_epub_resource_bytes_reports_invalid_archive_errors() {
        let file_path = unique_temp_path("komga-media-invalid-epub-resource");
        fs::write(&file_path, b"not a zip").expect("invalid epub test file should be written");

        let error = super::read_epub_resource_bytes(&file_path, "OPS/chapter.xhtml")
            .await
            .expect_err("invalid EPUB archive should be reported as an error");

        assert!(
            error.to_string().contains("open EPUB archive"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn read_epub_resource_bytes_keeps_missing_entries_as_absent() {
        let file_path = unique_temp_path("komga-media-missing-epub-resource");
        let archive = build_test_zip_archive(vec![(
            "OPS/chapter-1.xhtml".to_string(),
            b"chapter".to_vec(),
        )])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let bytes = super::read_epub_resource_bytes(&file_path, "OPS/missing.xhtml")
            .await
            .expect("missing EPUB entry should not fail");

        assert_eq!(bytes, None);
        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn read_mobi_resources_generates_content_on_request() {
        let file_path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sample/epub3.mobi");
        if !file_path.is_file() {
            return;
        }

        let bytes = super::read_epub_resource_bytes(&file_path, "OEBPS/text/chapter-0000.xhtml")
            .await
            .expect("MOBI resource should be readable")
            .expect("MOBI chapter should exist");

        assert!(String::from_utf8_lossy(&bytes).contains("<html"));

        let bytes = super::read_epub_publication_bytes(&mobi_media(file_path))
            .await
            .expect("MOBI publication should be readable")
            .expect("MOBI publication should exist");
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("MOBI EPUB should be valid");
        assert!(archive.by_name("OEBPS/content.opf").is_ok());
        assert!(archive.by_name("OEBPS/nav.xhtml").is_ok());
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_reports_invalid_archive_errors() {
        let file_path = unique_temp_path("komga-media-invalid-epub-cover");
        fs::write(&file_path, b"not a zip").expect("invalid epub test file should be written");

        let error = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect_err("invalid EPUB archive should be reported as an error");

        assert!(
            error.to_string().contains("open EPUB archive"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_reports_malformed_container_attributes() {
        let file_path = unique_temp_path("komga-media-malformed-epub-container");
        let archive = build_test_zip_archive(vec![(
            "META-INF/container.xml".to_string(),
            br#"<container><rootfiles><rootfile full-path= /></rootfiles></container>"#.to_vec(),
        )])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let error = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect_err("malformed EPUB container should be reported as an error");

        assert!(
            error.to_string().contains("EPUB container document"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_reports_malformed_package_attributes() {
        let file_path = unique_temp_path("komga-media-malformed-epub-package");
        let package_document = br#"<package><manifest><item id= href="images/cover.png" properties="cover-image"/></manifest></package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            ("OEBPS/content.opf".to_string(), package_document.to_vec()),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let error = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect_err("malformed EPUB package should be reported as an error");

        assert!(
            error.to_string().contains("EPUB package document"),
            "unexpected error: {error}"
        );
        let _ = fs::remove_file(file_path);
    }

    #[test]
    fn decode_epub_navigation_extension_returns_typed_navigation_parts() {
        let payload = json!({
            "isFixedLayout": true,
            "toc": [
                {
                    "title": "Chapter 1",
                    "href": "/chap-1.xhtml",
                    "children": [{ "title": "Part 1", "href": "/chap-1.xhtml#part-1" }]
                }
            ],
            "landmarks": [{ "title": "Cover", "href": "/cover.xhtml" }],
            "pageList": [{ "title": "1", "href": "/chap-1.xhtml#page-1" }],
            "positions": [
                {
                    "href": "/chap-1.xhtml",
                    "type": "application/xhtml+xml",
                    "locations": { "position": 1, "progression": 0.1 }
                },
                {
                    "href": "/chap-2.xhtml",
                    "type": "application/xhtml+xml",
                    "locations": { "position": 2, "progression": 0.2 }
                }
            ]
        });
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(payload.to_string().as_bytes())
            .expect("gzip payload should be writable");
        let blob = encoder.finish().expect("gzip payload should finalize");

        let extension =
            decode_epub_navigation_extension(&blob).expect("epub positions should decode");
        assert_eq!(extension.positions.len(), 2);
        assert_eq!(
            extension.positions[0].raw().get("href"),
            Some(&Value::String("/chap-1.xhtml".to_string()))
        );
        assert!(extension.is_fixed_layout);
        assert_eq!(extension.toc.len(), 1);
        assert_eq!(extension.toc[0].title.as_deref(), Some("Chapter 1"));
        assert_eq!(
            extension.toc[0].children[0].href.as_deref(),
            Some("/chap-1.xhtml#part-1")
        );
        assert_eq!(extension.landmarks.len(), 1);
        assert_eq!(extension.page_list.len(), 1);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_resolves_utf8_percent_encoded_guide_image() {
        let file_path = unique_temp_path("komga-media-epub-cover-utf8-guide");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="2.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="cover-img" href="images/caf%C3%A9.png" media-type="image/png"/>
    <item id="cover-page" href="cover.xhtml" media-type="application/xhtml+xml"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="cover-page"/>
    <itemref idref="chap-1"/>
  </spine>
  <guide>
    <reference type="cover" href="cover.xhtml"/>
  </guide>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/cover.xhtml".to_string(),
                br#"<?xml version="1.0"?><html><body><img src="images/caf%C3%A9.png"/></body></html>"#.to_vec(),
            ),
            (
                "OEBPS/images/café.png".to_string(),
                b"cafe-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via utf8 guide xhtml fallback");
        assert_eq!(cover.bytes, b"cafe-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }

    #[tokio::test]
    async fn load_epub_cover_bytes_selects_deterministic_cover_from_multiple_matches() {
        let file_path = unique_temp_path("komga-media-epub-cover-deterministic");
        let package_document = r#"<?xml version="1.0" encoding="UTF-8"?>
<package version="3.0" xmlns="http://www.idpf.org/2007/opf">
  <manifest>
    <item id="zz-cover" href="images/zz-cover.png" media-type="image/png"/>
    <item id="aa-cover" href="images/aa-cover.png" media-type="image/png"/>
    <item id="chap-1" href="chapter-1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="chap-1"/>
  </spine>
</package>"#;
        let archive = build_test_zip_archive(vec![
            (
                "META-INF/container.xml".to_string(),
                basic_container_xml().as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".to_string(),
                package_document.as_bytes().to_vec(),
            ),
            (
                "OEBPS/images/zz-cover.png".to_string(),
                b"zz-cover-bytes".to_vec(),
            ),
            (
                "OEBPS/images/aa-cover.png".to_string(),
                b"aa-cover-bytes".to_vec(),
            ),
        ])
        .expect("epub archive should be created");
        fs::write(&file_path, archive).expect("epub test file should be written");

        let cover = load_epub_cover_bytes(&epub_media(file_path.clone()))
            .await
            .expect("epub cover bytes should be readable")
            .expect("epub cover should exist via deterministic fallback");
        assert_eq!(cover.bytes, b"aa-cover-bytes");
        assert_eq!(cover.media_type, "image/png");

        let _ = fs::remove_file(file_path);
    }
}
