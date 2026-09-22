use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use komga_application::operational::{FontPort, build_font_family_css, is_supported_font_file};

#[derive(Clone, Default)]
pub struct FontAccess;

impl FontPort for FontAccess {
    fn list_font_families(&self, path: &Path) -> anyhow::Result<Vec<String>> {
        list_font_families(path)
    }

    fn load_font_family_css(&self, path: &Path, family: &str) -> anyhow::Result<Option<String>> {
        load_font_family_css(path, family)
    }

    fn load_font_file(
        &self,
        path: &Path,
        family: &str,
        file: &str,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        load_font_file(path, family, file)
    }
}

fn list_font_families(fonts_directory: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match fs::read_dir(fonts_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "read fonts directory '{}': {error}",
                fonts_directory.display()
            )));
        }
    };

    let mut families = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read fonts directory entry '{}'",
                fonts_directory.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read fonts directory entry type '{}'",
                entry.path().display()
            ))
        })?;
        if file_type.is_dir() {
            families.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    families.sort();
    Ok(families)
}

fn load_font_file(
    fonts_directory: &Path,
    font_family: &str,
    font_file: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = fonts_directory.join(font_family).join(font_file);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!(format!(
            "read font file '{}': {error}",
            path.display()
        ))),
    }
}

fn load_font_family_css(
    fonts_directory: &Path,
    font_family: &str,
) -> anyhow::Result<Option<String>> {
    let family_dir = fonts_directory.join(font_family);
    let entries = match fs::read_dir(&family_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(format!(
                "read font family directory '{}': {error}",
                family_dir.display()
            )));
        }
    };

    let mut font_files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read font family directory entry '{}'",
                family_dir.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            anyhow::anyhow!(error).context(format!(
                "read font family directory entry type '{}'",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let file_name = match path.file_name().and_then(|value| value.to_str()) {
            Some(value) => value,
            None => continue,
        };
        if !is_supported_font_file(file_name) {
            continue;
        }

        font_files.push(file_name.to_string());
    }

    Ok(build_font_family_css(font_family, font_files))
}

#[cfg(test)]
mod tests {
    use super::{list_font_families, load_font_family_css, load_font_file};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()))
    }

    #[test]
    fn list_font_families_returns_sorted_directories_only() {
        let root = unique_temp_dir("komga-fonts-list");
        fs::create_dir_all(root.join("Beta")).expect("beta dir should be created");
        fs::create_dir_all(root.join("Alpha")).expect("alpha dir should be created");
        fs::write(root.join("ignore.txt"), "not a family").expect("file should be created");

        let families = list_font_families(root.as_path()).expect("font families should list");

        assert_eq!(families, vec!["Alpha".to_string(), "Beta".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_font_file_reads_the_requested_font_bytes() {
        let root = unique_temp_dir("komga-fonts-file");
        let family_dir = root.join("Demo");
        fs::create_dir_all(&family_dir).expect("family dir should be created");
        fs::write(family_dir.join("Demo-Regular.ttf"), b"font-bytes")
            .expect("font file should be created");

        let bytes = load_font_file(root.as_path(), "Demo", "Demo-Regular.ttf")
            .expect("font file should load");

        assert_eq!(bytes.as_deref(), Some(&b"font-bytes"[..]));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_font_family_css_builds_face_blocks_from_font_files() {
        let root = unique_temp_dir("komga-fonts-css");
        let family_dir = root.join("Demo Family");
        fs::create_dir_all(&family_dir).expect("family dir should be created");
        fs::write(family_dir.join("Demo-Regular.ttf"), b"font-bytes")
            .expect("regular font should be created");
        fs::write(family_dir.join("Demo-BoldItalic.woff"), b"font-bytes")
            .expect("bold italic woff should be created");
        fs::write(family_dir.join("Demo-BoldItalic.woff2"), b"font-bytes")
            .expect("bold italic font should be created");
        fs::write(family_dir.join("notes.txt"), b"ignore")
            .expect("non-font file should be created");

        let css = load_font_family_css(root.as_path(), "Demo Family")
            .expect("font family CSS should load");

        let css = css.expect("css should be generated");
        assert_eq!(
            css,
            "@font-face {\n    font-family: 'Demo Family';\n    src: url('Demo-BoldItalic.woff') format('woff'),url('Demo-BoldItalic.woff2') format('woff2');\n    font-weight: bold;\n    font-style: italic;\n}\n\n@font-face {\n    font-family: 'Demo Family';\n    src: url('Demo-Regular.ttf') format('truetype');\n    font-weight: normal;\n    font-style: normal;\n}\n"
        );
        assert!(!css.contains("notes.txt"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_font_families_propagates_directory_read_errors() {
        let root = unique_temp_dir("komga-fonts-list-error");
        fs::write(&root, b"not-a-directory").expect("file fixture should be written");

        let error = list_font_families(root.as_path())
            .expect_err("read_dir errors must not become an empty font family list");

        assert!(
            error.to_string().contains("read fonts directory"),
            "unexpected font family listing error: {error}"
        );

        let _ = fs::remove_file(root);
    }

    #[test]
    fn load_font_file_propagates_read_errors() {
        let root = unique_temp_dir("komga-fonts-file-error");
        let font_path = root.join("Demo").join("Demo-Regular.ttf");
        fs::create_dir_all(&font_path).expect("directory fixture should be created at font path");

        let error = load_font_file(root.as_path(), "Demo", "Demo-Regular.ttf")
            .expect_err("font read errors must not become missing fonts");

        assert!(
            error.to_string().contains("read font file"),
            "unexpected font file read error: {error}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_font_family_css_propagates_directory_read_errors() {
        let root = unique_temp_dir("komga-fonts-css-error");
        fs::create_dir_all(&root).expect("font root should be created");
        fs::write(root.join("Demo"), b"not-a-directory")
            .expect("file fixture should be written at family path");

        let error = load_font_family_css(root.as_path(), "Demo")
            .expect_err("font CSS read_dir errors must not become missing CSS");

        assert!(
            error.to_string().contains("read font family directory"),
            "unexpected font family CSS error: {error}"
        );

        let _ = fs::remove_dir_all(root);
    }
}
