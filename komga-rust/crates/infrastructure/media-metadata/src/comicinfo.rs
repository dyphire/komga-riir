use anyhow::Context;
use komga_application::media_assets::{
    BookMediaRecord, book_media_is_rar_archive, book_media_is_zip_archive,
    content_type_from_filename,
};
use komga_infrastructure_media_core::content::metadata_sources::read_comicinfo_from_zip_archive;
use quick_xml::Reader as XmlReader;
use quick_xml::escape::{resolve_xml_entity, unescape};
use quick_xml::events::{BytesCData, BytesRef, BytesText, Event as XmlEvent};
use std::fs::File;
use std::path::Path;
use zip::ZipArchive;

use komga_infrastructure_media_core::formats::rar::{list_rar_entries, read_rar_entry_bytes};

const COMICINFO_FILE_NAME: &str = "ComicInfo.xml";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ComicInfoDocument {
    pub title: Option<String>,
    pub series: Option<String>,
    pub number: Option<String>,
    pub count: Option<String>,
    pub volume: Option<String>,
    pub alternate_series: Option<String>,
    pub alternate_number: Option<String>,
    pub summary: Option<String>,
    pub year: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
    pub writer: Option<String>,
    pub penciller: Option<String>,
    pub inker: Option<String>,
    pub colorist: Option<String>,
    pub letterer: Option<String>,
    pub cover_artist: Option<String>,
    pub editor: Option<String>,
    pub translator: Option<String>,
    pub publisher: Option<String>,
    pub genre: Option<String>,
    pub tags: Option<String>,
    pub web: Option<String>,
    pub language_iso: Option<String>,
    pub manga: Option<String>,
    pub story_arc: Option<String>,
    pub story_arc_number: Option<String>,
    pub series_group: Option<String>,
    pub age_rating: Option<String>,
    pub gtin: Option<String>,
}

#[derive(Clone, Copy)]
enum ComicInfoField {
    Title,
    Series,
    Number,
    Count,
    Volume,
    AlternateSeries,
    AlternateNumber,
    Summary,
    Year,
    Month,
    Day,
    Writer,
    Penciller,
    Inker,
    Colorist,
    Letterer,
    CoverArtist,
    Editor,
    Translator,
    Publisher,
    Genre,
    Tags,
    Web,
    LanguageIso,
    Manga,
    StoryArc,
    StoryArcNumber,
    SeriesGroup,
    AgeRating,
    Gtin,
}

impl ComicInfoField {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Title" => Self::Title,
            "Series" => Self::Series,
            "Number" => Self::Number,
            "Count" => Self::Count,
            "Volume" => Self::Volume,
            "AlternateSeries" => Self::AlternateSeries,
            "AlternateNumber" => Self::AlternateNumber,
            "Summary" => Self::Summary,
            "Year" => Self::Year,
            "Month" => Self::Month,
            "Day" => Self::Day,
            "Writer" => Self::Writer,
            "Penciller" => Self::Penciller,
            "Inker" => Self::Inker,
            "Colorist" => Self::Colorist,
            "Letterer" => Self::Letterer,
            "CoverArtist" => Self::CoverArtist,
            "Editor" => Self::Editor,
            "Translator" => Self::Translator,
            "Publisher" => Self::Publisher,
            "Genre" => Self::Genre,
            "Tags" => Self::Tags,
            "Web" => Self::Web,
            "LanguageISO" => Self::LanguageIso,
            "Manga" => Self::Manga,
            "StoryArc" => Self::StoryArc,
            "StoryArcNumber" => Self::StoryArcNumber,
            "SeriesGroup" => Self::SeriesGroup,
            "AgeRating" => Self::AgeRating,
            "GTIN" => Self::Gtin,
            _ => return None,
        })
    }
}

impl ComicInfoDocument {
    fn set_field(&mut self, field: ComicInfoField, value: String) {
        let slot = match field {
            ComicInfoField::Title => &mut self.title,
            ComicInfoField::Series => &mut self.series,
            ComicInfoField::Number => &mut self.number,
            ComicInfoField::Count => &mut self.count,
            ComicInfoField::Volume => &mut self.volume,
            ComicInfoField::AlternateSeries => &mut self.alternate_series,
            ComicInfoField::AlternateNumber => &mut self.alternate_number,
            ComicInfoField::Summary => &mut self.summary,
            ComicInfoField::Year => &mut self.year,
            ComicInfoField::Month => &mut self.month,
            ComicInfoField::Day => &mut self.day,
            ComicInfoField::Writer => &mut self.writer,
            ComicInfoField::Penciller => &mut self.penciller,
            ComicInfoField::Inker => &mut self.inker,
            ComicInfoField::Colorist => &mut self.colorist,
            ComicInfoField::Letterer => &mut self.letterer,
            ComicInfoField::CoverArtist => &mut self.cover_artist,
            ComicInfoField::Editor => &mut self.editor,
            ComicInfoField::Translator => &mut self.translator,
            ComicInfoField::Publisher => &mut self.publisher,
            ComicInfoField::Genre => &mut self.genre,
            ComicInfoField::Tags => &mut self.tags,
            ComicInfoField::Web => &mut self.web,
            ComicInfoField::LanguageIso => &mut self.language_iso,
            ComicInfoField::Manga => &mut self.manga,
            ComicInfoField::StoryArc => &mut self.story_arc,
            ComicInfoField::StoryArcNumber => &mut self.story_arc_number,
            ComicInfoField::SeriesGroup => &mut self.series_group,
            ComicInfoField::AgeRating => &mut self.age_rating,
            ComicInfoField::Gtin => &mut self.gtin,
        };

        if slot.is_none() {
            let value = value.trim();
            if !value.is_empty() {
                *slot = Some(value.to_string());
            }
        }
    }
}

pub fn parse_comicinfo_xml(xml: &[u8]) -> anyhow::Result<ComicInfoDocument> {
    let mut reader = XmlReader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut document = ComicInfoDocument::default();
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut saw_root = false;
    let mut current_field = None;
    let mut current_value = String::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(XmlEvent::Start(event)) => {
                let event_name = event.name();
                let name = local_xml_name(event_name.as_ref());
                if depth == 0 {
                    if name != "ComicInfo" {
                        return Err(anyhow::anyhow!(
                            "unexpected ComicInfo root element '{}', expected 'ComicInfo'",
                            name
                        ));
                    }
                    saw_root = true;
                }
                depth += 1;
                if depth == 2 {
                    current_field = ComicInfoField::from_name(name);
                    current_value.clear();
                }
            }
            Ok(XmlEvent::Empty(event)) if depth == 0 => {
                let event_name = event.name();
                let name = local_xml_name(event_name.as_ref());
                if name != "ComicInfo" {
                    return Err(anyhow::anyhow!(
                        "unexpected ComicInfo root element '{}', expected 'ComicInfo'",
                        name
                    ));
                }
                saw_root = true;
            }
            Ok(XmlEvent::Empty(event)) if depth == 1 => {
                let event_name = event.name();
                current_field = ComicInfoField::from_name(local_xml_name(event_name.as_ref()));
                if let Some(field) = current_field.take() {
                    document.set_field(field, String::new());
                }
            }
            Ok(XmlEvent::Text(event)) if current_field.is_some() => {
                current_value.push_str(&decode_xml_text(&event)?);
            }
            Ok(XmlEvent::CData(event)) if current_field.is_some() => {
                current_value.push_str(&decode_xml_cdata(&event)?);
            }
            Ok(XmlEvent::GeneralRef(event)) if current_field.is_some() => {
                current_value.push_str(&decode_xml_general_ref(&event)?);
            }
            Ok(XmlEvent::End(_event)) => {
                if depth == 0 {
                    return Err(anyhow::anyhow!("unexpected ComicInfo closing element"));
                }
                if depth == 2
                    && let Some(field) = current_field.take()
                {
                    document.set_field(field, std::mem::take(&mut current_value));
                }
                depth -= 1;
            }
            Ok(XmlEvent::Eof) => break,
            Err(error) => {
                return Err(anyhow::anyhow!(error).context("failed to parse ComicInfo.xml"));
            }
            _ => {}
        }
        buffer.clear();
    }

    if !saw_root || depth != 0 {
        return Err(anyhow::anyhow!(
            "ComicInfo.xml did not contain a complete ComicInfo root"
        ));
    }

    Ok(document)
}

fn local_xml_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn decode_xml_text(event: &BytesText<'_>) -> anyhow::Result<String> {
    let unescaped = unescape(event.as_ref())?;
    Ok(unescaped.into_owned())
}

fn decode_xml_cdata(event: &BytesCData<'_>) -> anyhow::Result<String> {
    Ok(event.as_ref().to_owned())
}

fn decode_xml_general_ref(event: &BytesRef<'_>) -> anyhow::Result<String> {
    if let Some(character) = event.resolve_char_ref()? {
        return Ok(character.to_string());
    }

    let name = event.as_ref();
    resolve_xml_entity(name)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("unknown ComicInfo.xml entity '&{};'", name))
}

pub fn load_comicinfo_bytes_for_media(media: &BookMediaRecord) -> anyhow::Result<Option<Vec<u8>>> {
    if book_media_is_zip_archive(media) || book_media_is_rar_archive(media) {
        return load_comicinfo_bytes_from_path(&media.file_path, &media.media_type);
    }
    Ok(None)
}

pub fn load_comicinfo_bytes_from_path(
    path: &Path,
    media_type: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let normalized_media_type = content_type_from_filename(file_name, media_type);

    match normalized_media_type.as_str() {
        "application/zip" | "application/vnd.comicbook+zip" | "application/epub+zip" => {
            load_comicinfo_bytes_from_zip(path)
        }
        "application/vnd.comicbook-rar"
        | "application/x-rar-compressed"
        | "application/x-rar-compressed; version=4"
        | "application/x-rar-compressed; version=5" => load_comicinfo_bytes_from_rar(path),
        _ => Ok(None),
    }
}

fn load_comicinfo_bytes_from_zip(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let file = File::open(path).map_err(|error| {
        anyhow::anyhow!(error).context(format!("failed to open ComicInfo archive '{}",
            path.display()
        ))
    })?;
    let mut archive = ZipArchive::new(file).map_err(|error| {
        anyhow::anyhow!(error).context(format!("failed to read ComicInfo archive '{}",
            path.display()
        ))
    })?;

    read_comicinfo_from_zip_archive(&mut archive)
        .with_context(|| format!("read ComicInfo archive '{}'", path.display()))
}

fn load_comicinfo_bytes_from_rar(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let entries = list_rar_entries(path)?;
    let entry_name = entries
        .iter()
        .find(|entry| entry.file_name == COMICINFO_FILE_NAME)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| is_nested_comicinfo_entry(&entry.file_name))
        })
        .map(|entry| entry.file_name.as_str());

    let Some(entry_name) = entry_name else {
        return Ok(None);
    };
    read_rar_entry_bytes(path, entry_name)
}

fn is_nested_comicinfo_entry(entry_name: &str) -> bool {
    entry_name
        .rsplit('/')
        .next()
        .is_some_and(|name| name == COMICINFO_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::*;

    fn unique_temp_path(prefix: &str, extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos(),
            extension
        ))
    }

    fn write_zip_fixture(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).expect("zip fixture should be created");
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (entry_name, bytes) in entries {
            archive
                .start_file(*entry_name, options)
                .expect("zip fixture entry should be created");
            archive
                .write_all(bytes)
                .expect("zip fixture entry should be written");
        }
        archive.finish().expect("zip fixture should be finished");
    }

    #[test]
    fn parses_entities_and_cdata_from_comicinfo_xml() {
        let document = parse_comicinfo_xml(
            br#"<ComicInfo><Summary><![CDATA[Summary & details]]></Summary><Genre>Manga, Movies &amp; TV</Genre></ComicInfo>"#,
        )
        .expect("ComicInfo XML should parse");

        assert_eq!(document.summary.as_deref(), Some("Summary & details"));
        assert_eq!(document.genre.as_deref(), Some("Manga, Movies & TV"));
    }

    #[test]
    fn rejects_non_comicinfo_root() {
        let error = parse_comicinfo_xml(br#"<Metadata><Summary>wrong</Summary></Metadata>"#)
            .expect_err("non-ComicInfo root should fail");

        assert!(error.to_string().contains("expected 'ComicInfo'"));
    }

    #[test]
    fn loads_root_comicinfo_before_nested_entries() {
        let path = unique_temp_path("komga-comicinfo-root-priority", "cbz");
        let root_xml = format!(
            "<ComicInfo><Summary>{}</Summary><Title>root</Title></ComicInfo>",
            "x".repeat(32 * 1024)
        );
        write_zip_fixture(
            &path,
            &[
                (
                    "nested/ComicInfo.xml",
                    br#"<ComicInfo><Title>nested</Title></ComicInfo>"#,
                ),
                ("ComicInfo.xml", root_xml.as_bytes()),
            ],
        );

        let bytes = load_comicinfo_bytes_from_path(&path, "application/vnd.comicbook+zip")
            .expect("CBZ ComicInfo should load")
            .expect("CBZ should contain ComicInfo.xml");
        assert_eq!(
            parse_comicinfo_xml(&bytes).unwrap().title.as_deref(),
            Some("root")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn loads_first_nested_comicinfo_when_root_is_absent() {
        let path = unique_temp_path("komga-comicinfo-nested-fallback", "zip");
        write_zip_fixture(
            &path,
            &[
                (
                    "first\\ComicInfo.xml",
                    br#"<ComicInfo><Title>first</Title></ComicInfo>"#,
                ),
                (
                    "second/ComicInfo.xml",
                    br#"<ComicInfo><Title>second</Title></ComicInfo>"#,
                ),
            ],
        );

        let bytes = load_comicinfo_bytes_from_path(&path, "application/zip")
            .expect("ZIP ComicInfo should load")
            .expect("ZIP should contain a nested ComicInfo.xml");
        assert_eq!(
            parse_comicinfo_xml(&bytes).unwrap().title.as_deref(),
            Some("first")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn loads_comicinfo_from_epub_zip() {
        let path = unique_temp_path("komga-comicinfo-epub", "epub");
        write_zip_fixture(
            &path,
            &[(
                "OEBPS/ComicInfo.xml",
                br#"<ComicInfo><Summary>EPUB summary</Summary></ComicInfo>"#,
            )],
        );

        let bytes = load_comicinfo_bytes_from_path(&path, "application/epub+zip")
            .expect("EPUB ComicInfo should load")
            .expect("EPUB should contain ComicInfo.xml");
        assert_eq!(
            parse_comicinfo_xml(&bytes).unwrap().summary.as_deref(),
            Some("EPUB summary")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn loads_comicinfo_from_rar_fixture() {
        let path = unique_temp_path("komga-comicinfo-rar", "rar");
        fs::write(
            &path,
            include_bytes!("../../../../sample/ComicInfo_duplicateInfos.rar"),
        )
        .expect("RAR fixture should be copied");

        let bytes = load_comicinfo_bytes_from_path(&path, "application/vnd.comicbook-rar")
            .expect("RAR ComicInfo should load")
            .expect("RAR should contain ComicInfo.xml");
        parse_comicinfo_xml(&bytes).expect("RAR ComicInfo XML should parse");
        let _ = fs::remove_file(path);
    }
}
