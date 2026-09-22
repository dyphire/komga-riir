use std::collections::BTreeSet;

use komga_application::operational::{
    RemoteAnnouncementAuthor, RemoteAnnouncementItem, RemoteAnnouncementsFeed, RemoteFeedPort,
    RemoteRelease,
};
use reqwest::Client;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[derive(Clone, Debug)]
pub struct RemoteFeedAccess {
    announcements_url: String,
    releases_url: String,
}

impl RemoteFeedAccess {
    pub fn default_announcements_url() -> &'static str {
        "https://komga.org/blog/feed.json"
    }

    pub fn default_releases_url() -> &'static str {
        "https://api.github.com/repos/dyphire/komga-riir/releases?per_page=20"
    }

    pub fn new(announcements_url: impl Into<String>, releases_url: impl Into<String>) -> Self {
        Self {
            announcements_url: announcements_url.into(),
            releases_url: releases_url.into(),
        }
    }
}

impl Default for RemoteFeedAccess {
    fn default() -> Self {
        Self::new(
            Self::default_announcements_url(),
            Self::default_releases_url(),
        )
    }
}

#[async_trait::async_trait]
impl RemoteFeedPort for RemoteFeedAccess {
    async fn load_announcements_feed(&self) -> anyhow::Result<Option<RemoteAnnouncementsFeed>> {
        let bytes = Client::new()
            .get(&self.announcements_url)
            .send()
            .await
            .map_err(anyhow::Error::from)?
            .error_for_status()
            .map_err(anyhow::Error::from)?
            .bytes()
            .await
            .map_err(anyhow::Error::from)?;
        parse_announcements_feed_bytes(bytes.as_ref())
    }

    async fn load_releases(&self) -> anyhow::Result<Vec<RemoteRelease>> {
        let bytes = Client::new()
            .get(&self.releases_url)
            .header("User-Agent", "komga-rust-runtime")
            .send()
            .await
            .map_err(anyhow::Error::from)?
            .error_for_status()
            .map_err(anyhow::Error::from)?
            .bytes()
            .await
            .map_err(anyhow::Error::from)?;
        parse_releases_bytes(bytes.as_ref())
    }
}

fn parse_announcements_feed_bytes(bytes: &[u8]) -> anyhow::Result<Option<RemoteAnnouncementsFeed>> {
    if bytes.is_empty() {
        return Ok(None);
    }

    serde_json::from_slice::<Option<AnnouncementsFeedDto>>(bytes)
        .map(|feed| feed.map(RemoteAnnouncementsFeed::from))
        .map_err(anyhow::Error::from)
}

fn parse_releases_bytes(bytes: &[u8]) -> anyhow::Result<Vec<RemoteRelease>> {
    let upstream = serde_json::from_slice::<Vec<GithubReleaseUpstreamDto>>(bytes)
        .map_err(anyhow::Error::from)?;
    Ok(map_github_releases(upstream))
}

#[derive(Debug, Clone, Deserialize)]
struct AnnouncementsFeedDto {
    version: String,
    title: String,
    #[serde(rename = "home_page_url")]
    home_page_url: Option<String>,
    description: Option<String>,
    #[serde(default)]
    items: Vec<AnnouncementItemDto>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnnouncementItemDto {
    id: String,
    url: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    #[serde(rename = "content_html")]
    content_html: Option<String>,
    #[serde(default)]
    #[serde(with = "optional_rfc3339")]
    #[serde(rename = "date_modified")]
    date_modified: Option<OffsetDateTime>,
    author: Option<AnnouncementAuthorDto>,
    #[serde(default)]
    tags: BTreeSet<String>,
    #[serde(rename = "_komga")]
    komga_extension: Option<AnnouncementKomgaExtensionDto>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnnouncementAuthorDto {
    name: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnnouncementKomgaExtensionDto {
    read: bool,
}

impl From<AnnouncementsFeedDto> for RemoteAnnouncementsFeed {
    fn from(feed: AnnouncementsFeedDto) -> Self {
        Self {
            version: feed.version,
            title: feed.title,
            home_page_url: feed.home_page_url,
            description: feed.description,
            items: feed
                .items
                .into_iter()
                .map(RemoteAnnouncementItem::from)
                .collect(),
        }
    }
}

impl From<AnnouncementItemDto> for RemoteAnnouncementItem {
    fn from(item: AnnouncementItemDto) -> Self {
        Self {
            id: item.id,
            url: item.url,
            title: item.title,
            summary: item.summary,
            content_html: item.content_html,
            date_modified: item.date_modified,
            author: item.author.map(RemoteAnnouncementAuthor::from),
            tags: item.tags,
            read: item.komga_extension.is_some_and(|extension| extension.read),
        }
    }
}

impl From<AnnouncementAuthorDto> for RemoteAnnouncementAuthor {
    fn from(author: AnnouncementAuthorDto) -> Self {
        Self {
            name: author.name,
            url: author.url,
        }
    }
}

mod optional_rfc3339 {
    use super::{OffsetDateTime, Rfc3339};
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<String>::deserialize(deserializer)?;
        value
            .map(|value| OffsetDateTime::parse(&value, &Rfc3339).map_err(serde::de::Error::custom))
            .transpose()
    }
}

fn map_github_releases(upstream: Vec<GithubReleaseUpstreamDto>) -> Vec<RemoteRelease> {
    upstream
        .into_iter()
        .enumerate()
        .map(|(index, release)| RemoteRelease {
            version: release.tag_name,
            release_date: release.published_at,
            url: release.html_url,
            latest: index == 0,
            pre_release: release.prerelease,
            description: release.body,
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
struct GithubReleaseUpstreamDto {
    html_url: String,
    tag_name: String,
    #[serde(with = "required_rfc3339")]
    published_at: OffsetDateTime,
    body: String,
    prerelease: bool,
}

mod required_rfc3339 {
    use super::{OffsetDateTime, Rfc3339};
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        OffsetDateTime::parse(&value, &Rfc3339).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use time::format_description::well_known::Rfc3339;

    use super::*;

    #[test]
    fn parse_announcements_feed_bytes_maps_json_feed_to_typed_records() {
        let feed = parse_announcements_feed_bytes(
            br#"{
                "version": "https://jsonfeed.org/version/1.1",
                "title": "Komga News",
                "home_page_url": "https://komga.org",
                "unknown": "removed",
                "items": [
                    {
                        "id": "announcement-1",
                        "url": "https://komga.org/1",
                        "title": "One",
                        "date_modified": "2024-01-01T00:00:00Z",
                        "author": { "name": "Komga", "url": "https://komga.org" },
                        "tags": ["release"],
                        "_komga": { "read": true },
                        "unknown": "removed"
                    }
                ]
            }"#,
        )
        .expect("announcements feed should parse")
        .expect("announcements feed should exist");

        assert_eq!(feed.title, "Komga News");
        assert_eq!(feed.items[0].id, "announcement-1");
        assert_eq!(feed.items[0].title.as_deref(), Some("One"));
        assert!(feed.items[0].read);
        assert_eq!(
            feed.items[0]
                .date_modified
                .expect("date should exist")
                .format(&Rfc3339)
                .expect("date should format"),
            "2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn parse_releases_bytes_maps_github_releases_to_typed_records() {
        let releases = parse_releases_bytes(
            br#"[{
                "html_url": "https://github.test/releases/v1",
                "tag_name": "v1.0.0",
                "published_at": "2024-01-02T03:04:05Z",
                "body": "Release notes",
                "prerelease": false
            }]"#,
        )
        .expect("releases should parse");

        assert_eq!(releases[0].version, "v1.0.0");
        assert!(releases[0].latest);
        assert!(!releases[0].pre_release);
        assert_eq!(
            releases[0]
                .release_date
                .format(&Rfc3339)
                .expect("release date should format"),
            "2024-01-02T03:04:05Z"
        );
    }

    #[test]
    fn parse_releases_bytes_rejects_null_payload() {
        parse_releases_bytes(b"null").expect_err("null releases payload should be rejected");
    }
}
