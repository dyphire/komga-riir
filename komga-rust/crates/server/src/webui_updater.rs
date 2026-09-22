//! Tracks the latest kmweb release on GitHub: downloads the bundle, verifies it
//! against the sha256 published alongside, and atomically points the served web UI
//! directory at it. The managed copy lives under `<config-dir>/webui` so it survives
//! container rebuilds; the bundled/configured directory stays the offline baseline.
//!
//! Layout of the managed copy:
//!   <config-dir>/webui/current            active version, e.g. "0.3.0"
//!   <config-dir>/webui/versions/<v>/      extracted bundle (+ a kmweb.version marker)
//!   <config-dir>/webui/.tmp/              download/extract staging, cleaned after use
//!
//! Ported from kmrs (`crates/komga-server/src/service/webui_updater.rs`).

use anyhow::{Context, bail};
use komga_config::env_config::RuntimeConfig;
use komga_interfaces::state::WebUiDirState;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GITHUB_RELEASES: &str = "https://api.github.com/repos/kmworks/kmweb/releases";
const RELEASES_URL_ENV: &str = "KOMGA_RUST_WEBUI_RELEASES_URL";
const MARKER: &str = "kmweb.version";

/// GitHub releases API base to track, overridable via `KOMGA_RUST_WEBUI_RELEASES_URL`.
/// The updater appends `/latest` to this base, mirroring kmrs' `GITHUB_RELEASES`.
fn releases_url(env_value: Option<&str>) -> String {
    env_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(GITHUB_RELEASES)
        .to_string()
}

fn managed_root(config_dir: &Path) -> PathBuf {
    config_dir.join("webui")
}

fn managed_dir(root: &Path) -> Option<PathBuf> {
    let version = std::fs::read_to_string(root.join("current")).ok()?;
    let dir = root.join("versions").join(version.trim());
    dir.join("index.html").is_file().then_some(dir)
}

/// The web UI directory to serve at startup: the updater's managed copy when one is
/// already installed, otherwise the configured bundle. A disabled UI stays disabled.
pub fn initial_dir(config: &RuntimeConfig) -> Option<PathBuf> {
    let configured = config.webui_dir.as_ref()?;
    let config_dir = config.config_dir.as_deref()?;
    Some(managed_dir(&managed_root(config_dir)).unwrap_or_else(|| configured.clone()))
}

/// Version of a served bundle, from its `kmweb.version` marker (stamped into the
/// docker image's bundled copy, and written by the updater into managed copies).
fn served_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(MARKER))
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub enum Outcome {
    UpToDate,
    Updated(String),
}

pub struct WebuiUpdater {
    base_url: String,
    http: reqwest::Client,
    webui_dir: WebUiDirState,
    config_dir: PathBuf,
}

impl WebuiUpdater {
    fn new(base_url: &str, webui_dir: WebUiDirState, config_dir: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("komga-riir/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self {
            base_url: base_url.to_string(),
            http,
            webui_dir,
            config_dir,
        }
    }

    /// Periodic checks: first one on startup, then every `webui.update-interval`.
    pub fn start(
        webui_dir: WebUiDirState,
        config_dir: PathBuf,
        period: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let base_url = releases_url(std::env::var(RELEASES_URL_ENV).ok().as_deref());
        let updater = Self::new(&base_url, webui_dir, config_dir);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            loop {
                // interval's first tick completes immediately
                interval.tick().await;
                match updater.check_once().await {
                    Ok(Outcome::Updated(version)) => {
                        tracing::info!("web UI updated to kmweb v{version}")
                    }
                    Ok(Outcome::UpToDate) => tracing::debug!("web UI is up to date"),
                    Err(error) => tracing::warn!("web UI update check failed: {error:#}"),
                }
            }
        })
    }

    async fn check_once(&self) -> anyhow::Result<Outcome> {
        let Some(served) = self.webui_dir.get() else {
            return Ok(Outcome::UpToDate);
        };
        let release = self.latest_release().await?;
        let version = release.tag_name.trim_start_matches('v').to_string();
        if version.is_empty() {
            bail!("unexpected kmweb release tag: {:?}", release.tag_name);
        }
        if served_version(&served).as_deref() == Some(version.as_str()) {
            return Ok(Outcome::UpToDate);
        }

        let tarball_name = format!("kmweb-v{version}.tar.gz");
        let tarball_url = asset_url(&release, &tarball_name)?;
        let sha_url = asset_url(&release, &format!("{tarball_name}.sha256"))?;

        let root = managed_root(&self.config_dir);
        let tmp = root.join(".tmp");
        std::fs::create_dir_all(&tmp)?;
        let tarball = tmp.join(&tarball_name);
        let bytes = self.get_bytes(&tarball_url).await?;
        tokio::fs::write(&tarball, &bytes).await?;
        let expected = parse_sha256(&String::from_utf8(self.get_bytes(&sha_url).await?)?)?;

        let install = Install {
            root: root.clone(),
            tmp: tmp.clone(),
            tarball,
            expected,
            version: version.clone(),
        };
        let managed = tokio::task::spawn_blocking(move || install.run())
            .await
            .context("install task")??;
        self.webui_dir.set(Some(managed));
        Ok(Outcome::Updated(version))
    }

    async fn latest_release(&self) -> anyhow::Result<Release> {
        Ok(self
            .http
            .get(format!("{}/latest", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn get_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }
}

/// Download/install split so the blocking filesystem half can run in spawn_blocking.
struct Install {
    root: PathBuf,
    tmp: PathBuf,
    tarball: PathBuf,
    expected: String,
    version: String,
}

impl Install {
    fn run(self) -> anyhow::Result<PathBuf> {
        if sha256_hex(&self.tarball)? != self.expected {
            bail!("sha256 mismatch for {}", self.tarball.display());
        }
        let staging = self.tmp.join(format!("extract-{}", self.version));
        let _ = std::fs::remove_dir_all(&staging);
        extract_bundle(&self.tarball, &staging)?;
        if !staging.join("index.html").is_file() {
            bail!("{} has no index.html at its root", self.tarball.display());
        }
        std::fs::write(staging.join(MARKER), &self.version)?;

        let versions = self.root.join("versions");
        std::fs::create_dir_all(&versions)?;
        let target = versions.join(&self.version);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&staging, &target)?;

        // write-then-rename keeps `current` consistent with a fully installed version
        let current_tmp = self.tmp.join("current");
        std::fs::write(&current_tmp, &self.version)?;
        std::fs::rename(&current_tmp, self.root.join("current"))?;

        let _ = std::fs::remove_dir_all(&self.tmp);
        for entry in std::fs::read_dir(&versions)?.flatten() {
            if entry.file_name() != self.version.as_str() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(target)
    }
}

fn asset_url(release: &Release, name: &str) -> anyhow::Result<String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .with_context(|| format!("kmweb release {} has no asset {name}", release.tag_name))
}

/// First whitespace-separated token of a `sha256sum`-format file.
fn parse_sha256(text: &str) -> anyhow::Result<String> {
    let token = text.split_whitespace().next().unwrap_or_default();
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(token.to_ascii_lowercase())
    } else {
        bail!("no sha256 found in {text:?}");
    }
}

fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_encode(hasher.finalize().as_slice()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `unpack_in` refuses entries escaping the destination; turn that into an error
/// instead of silently skipping them.
fn extract_bundle(tarball: &Path, dst: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(tarball)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(dst)?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.unpack_in(dst)? {
            bail!(
                "archive entry escapes the target directory: {:?}",
                entry.path()
            );
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use komga_config::profile::RuntimeProfile;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const VERSION: &str = "0.9.9";

    fn test_config(config_dir: &Path, webui_dir: Option<PathBuf>) -> RuntimeConfig {
        let mut config = RuntimeConfig::for_runtime_profile(RuntimeProfile::SnapshotAligned);
        config.config_dir = Some(config_dir.to_path_buf());
        config.webui_dir = webui_dir;
        config
    }

    /// A bundle tarball with the given index.html, gzipped the way kmweb publishes it.
    fn bundle_tarball(index_html: &str) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(index_html.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "./index.html", index_html.as_bytes())
                .unwrap();
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    /// Serves /releases/latest plus the two assets; asset URLs point back at this server.
    async fn serve_github(tarball: Vec<u8>, sha: String) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = {
            let hits = hits.clone();
            axum::Router::new()
                .route(
                    "/releases/latest",
                    axum::routing::get(move || async move {
                        axum::Json(serde_json::json!({
                            "tag_name": format!("v{VERSION}"),
                            "assets": [
                                {"name": format!("kmweb-v{VERSION}.tar.gz"),
                                 "browser_download_url": format!("http://{addr}/kmweb.tar.gz")},
                                {"name": format!("kmweb-v{VERSION}.tar.gz.sha256"),
                                 "browser_download_url": format!("http://{addr}/kmweb.sha256")},
                            ],
                        }))
                    }),
                )
                .route(
                    "/kmweb.tar.gz",
                    axum::routing::get(move || {
                        let hits = hits.clone();
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            tarball
                        }
                    }),
                )
                .route(
                    "/kmweb.sha256",
                    axum::routing::get(move || async move { sha }),
                )
        };
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/releases"), hits)
    }

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        hex_encode(sha2::Sha256::digest(bytes).as_slice())
    }

    fn baseline(index_html: &str, marker: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), index_html).unwrap();
        if let Some(version) = marker {
            std::fs::write(dir.path().join(MARKER), version).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn installs_new_version_and_swaps_served_dir() {
        let config_dir = tempfile::tempdir().unwrap();
        let configured = baseline("<html>old</html>", None);
        let config = test_config(config_dir.path(), Some(configured.path().to_path_buf()));
        let webui_dir = WebUiDirState::new(initial_dir(&config));
        let tarball = bundle_tarball("<html>new</html>");
        let (base, _) = serve_github(
            tarball.clone(),
            format!("{}  dist-release/kmweb.tar.gz", sha_of(&tarball)),
        )
        .await;
        let updater =
            WebuiUpdater::new(&base, webui_dir.clone(), config.config_dir.clone().unwrap());

        let outcome = updater.check_once().await.unwrap();
        assert!(matches!(outcome, Outcome::Updated(v) if v == VERSION));

        let managed = config_dir.path().join("webui/versions").join(VERSION);
        assert_eq!(webui_dir.get().unwrap(), managed);
        assert_eq!(
            std::fs::read_to_string(config_dir.path().join("webui/current")).unwrap(),
            VERSION
        );
        assert_eq!(
            std::fs::read_to_string(managed.join(MARKER)).unwrap(),
            VERSION
        );
        assert_eq!(
            std::fs::read_to_string(managed.join("index.html")).unwrap(),
            "<html>new</html>"
        );
        // .tmp cleaned, only the active version kept
        assert!(!config_dir.path().join("webui/.tmp").exists());

        // the managed marker makes the next check a no-op
        let outcome = updater.check_once().await.unwrap();
        assert!(matches!(outcome, Outcome::UpToDate));
    }

    #[tokio::test]
    async fn up_to_date_marker_skips_the_download() {
        let config_dir = tempfile::tempdir().unwrap();
        let configured = baseline("<html>old</html>", Some(VERSION));
        let config = test_config(config_dir.path(), Some(configured.path().to_path_buf()));
        let webui_dir = WebUiDirState::new(initial_dir(&config));
        let tarball = bundle_tarball("<html>new</html>");
        let (base, hits) = serve_github(tarball.clone(), sha_of(&tarball)).await;
        let updater =
            WebuiUpdater::new(&base, webui_dir.clone(), config.config_dir.clone().unwrap());

        let outcome = updater.check_once().await.unwrap();
        assert!(matches!(outcome, Outcome::UpToDate));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert!(!config_dir.path().join("webui/current").exists());
    }

    #[tokio::test]
    async fn sha256_mismatch_keeps_the_configured_dir() {
        let config_dir = tempfile::tempdir().unwrap();
        let configured = baseline("<html>old</html>", None);
        let configured_path = configured.path().to_path_buf();
        let config = test_config(config_dir.path(), Some(configured_path.clone()));
        let webui_dir = WebUiDirState::new(initial_dir(&config));
        let tarball = bundle_tarball("<html>new</html>");
        let (base, _) = serve_github(tarball, format!("{}  kmweb.tar.gz", "0".repeat(64))).await;
        let updater =
            WebuiUpdater::new(&base, webui_dir.clone(), config.config_dir.clone().unwrap());

        assert!(updater.check_once().await.is_err());
        assert_eq!(webui_dir.get().unwrap(), configured_path);
        assert!(!config_dir.path().join("webui/current").exists());
    }

    #[test]
    fn extract_rejects_path_traversal() {
        let tarball = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            {
                let mut builder = tar::Builder::new(&mut encoder);
                // Builder's set_path refuses `..`, so write the name field directly —
                // this is the shape a hostile archive would arrive in
                let mut header = tar::Header::new_gnu();
                header.as_mut_bytes()[..11].copy_from_slice(b"../evil.txt");
                header.set_size(4);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, b"evil".as_slice()).unwrap();
                builder.finish().unwrap();
            }
            encoder.finish().unwrap()
        };
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("evil.tar.gz");
        std::fs::write(&file, tarball).unwrap();
        let dst = tmp.path().join("dst");
        assert!(extract_bundle(&file, &dst).is_err());
        assert!(!tmp.path().join("evil.txt").exists());
    }

    #[test]
    fn initial_dir_prefers_managed_copy() {
        let config_dir = tempfile::tempdir().unwrap();
        let configured = PathBuf::from("/bundled/webui");

        // no managed copy yet: the configured bundle wins
        let config = test_config(config_dir.path(), Some(configured.clone()));
        assert_eq!(initial_dir(&config), Some(configured.clone()));

        // a valid managed copy wins
        let managed = config_dir.path().join("webui/versions").join(VERSION);
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("index.html"), "<html/>").unwrap();
        std::fs::write(config_dir.path().join("webui/current"), VERSION).unwrap();
        assert_eq!(initial_dir(&config), Some(managed));

        // a disabled UI stays disabled even with a managed copy present
        let config = test_config(config_dir.path(), None);
        assert_eq!(initial_dir(&config), None);

        // a broken managed copy (no index.html) falls back to the baseline
        let config = test_config(config_dir.path(), Some(configured.clone()));
        std::fs::remove_file(
            config_dir
                .path()
                .join("webui/versions")
                .join(VERSION)
                .join("index.html"),
        )
        .unwrap();
        assert_eq!(initial_dir(&config), Some(configured));
    }

    #[test]
    fn releases_url_defaults_and_overrides() {
        assert_eq!(releases_url(None), GITHUB_RELEASES);
        assert_eq!(releases_url(Some(" ")), GITHUB_RELEASES);
        assert_eq!(
            releases_url(Some("https://github.example.com/kmweb/releases")),
            "https://github.example.com/kmweb/releases"
        );
    }

    #[test]
    fn parses_sha256sum_format() {
        let hash = "a".repeat(64);
        assert_eq!(
            parse_sha256(&format!("{hash}  dist-release/kmweb.tar.gz\n")).unwrap(),
            hash
        );
        assert!(parse_sha256("not a hash").is_err());
        assert!(parse_sha256("").is_err());
    }
}
