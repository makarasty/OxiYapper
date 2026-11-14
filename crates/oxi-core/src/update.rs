//! The update check's decisions (ARCHITECTURE section 12, docs/RELEASING.md "Assets and tags"):
//! version order, the GitHub release list, the exe's line in `SHA256SUMS` and the status line the
//! System page shows. The daemon's `update` module does the network and the files.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Every release, pre-releases included (`/releases/latest` skips them). Ten, so a draft or a
/// hand-pushed test tag on top cannot hide the newest build.
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/makarasty/OxiYapper/releases?per_page=10";
/// The release page a "Download" link opens: built here, never taken from the answer.
const RELEASE_PAGE: &str = "https://github.com/makarasty/OxiYapper/releases/tag/v";
pub const SUMS_NAME: &str = "SHA256SUMS";
/// Hidden test hook (docs/RELEASING.md): the version the check takes as the running one, so a
/// build updates itself from the real latest release.
pub const FAKE_VERSION_ENV: &str = "OXI_UPDATE_FAKE_VERSION";

/// A `X.Y.Z` or `X.Y.Z-<pre>` version, ordered by semver: a pre-release is older than its
/// release, pre-release fields compare one by one, numbers as numbers (`pre.10` after `pre.9`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<Ident>,
}

/// One dot-separated pre-release field. Numbers sort before words (semver 11.4.3), which the
/// variant order gives.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Ident {
    Num(u64),
    Text(String),
}

impl Version {
    /// `0.2.0`, `v0.2.0-pre.57`, `0.2.0-rc.1`; build metadata (`+...`) is ignored. `None` for
    /// anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let mut nums = core
            .split('.')
            .map(|n| digits(n).then(|| n.parse().ok()).flatten());
        let core = [nums.next()??, nums.next()??, nums.next()??];
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|id| {
                    if digits(id) {
                        id.parse().ok().map(Ident::Num)
                    } else if !id.is_empty()
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        Some(Ident::Text(id.to_owned()))
                    } else {
                        None
                    }
                })
                .collect::<Option<_>>()?,
        };
        Some(Self { core, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.core;
        write!(f, "{major}.{minor}.{patch}")?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Text(t) => f.write_str(t)?,
            }
        }
        Ok(())
    }
}

/// A downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub url: String,
    pub size: u64,
}

/// A published release with the two files the updater needs, when it has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// `OxiYapper-<version>-windows-x64.exe`.
    pub exe: Option<Asset>,
    pub sums: Option<Asset>,
}

impl Release {
    /// Its page on GitHub, for the "Download" link.
    #[must_use]
    pub fn page(&self) -> String {
        format!("{RELEASE_PAGE}{}", self.version)
    }
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// The release list of the GitHub API. Drafts and tags that are no version are left out.
///
/// # Errors
/// The text is not such a list.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, serde_json::Error> {
    let raw: Vec<RawRelease> = serde_json::from_str(json)?;
    Ok(raw
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = Version::parse(&r.tag_name)?;
            let find = |name: &str| {
                r.assets.iter().find(|a| a.name == name).map(|a| Asset {
                    url: a.browser_download_url.clone(),
                    size: a.size,
                })
            };
            Some(Release {
                exe: find(&exe_name(&version)),
                sums: find(SUMS_NAME),
                version,
            })
        })
        .collect())
}

/// The newest release above `current`, pre-releases included.
#[must_use]
pub fn newest(releases: Vec<Release>, current: &Version) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| r.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// The single-file exe asset of `version`.
#[must_use]
pub fn exe_name(version: &Version) -> String {
    format!("{}-{version}-windows-x64.exe", crate::APP_NAME)
}

/// The lowercase hex SHA256 of `file` in a `sha256sum` listing (`<hex>  <name>`, or `<hex> *<name>`
/// in binary mode).
#[must_use]
pub fn checksum(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

/// Where the updater is, for the System page. Sent by the daemon (`Push::Update`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "s")]
pub enum Status {
    /// Nothing checked yet since the start.
    #[default]
    Unknown,
    /// Update checks are off.
    Off,
    Checking,
    UpToDate,
    /// A newer release that is not installed by itself: automatic install is off, the exe folder
    /// cannot be written, the release lacks the exe, or the install failed. `page` is its release
    /// page.
    Available {
        version: String,
        page: String,
    },
    Downloading {
        version: String,
        percent: u8,
    },
    /// Downloaded and checked; installs and restarts once no dictation runs.
    Ready {
        version: String,
    },
    /// This start finished an update to the running version.
    Updated,
    /// The last check did not get an answer (offline, rate-limited); the next one is tomorrow.
    Failed,
}

/// The System page's status line; `running` is the running version.
#[must_use]
pub fn status_line(status: &Status, running: &str) -> String {
    match status {
        Status::Unknown => format!("Version {running}"),
        Status::Off => format!("Version {running} · update checks are off"),
        Status::Checking => "Checking for updates…".to_owned(),
        Status::UpToDate => format!("Up to date · {running}"),
        Status::Available { version, .. } => format!("Update available · {version}"),
        Status::Downloading { version, percent } => format!("Downloading {version}… {percent} %"),
        Status::Ready { version } => format!("Installing {version} once you stop dictating…"),
        Status::Updated => format!("Updated to {running}"),
        Status::Failed => format!("Could not reach GitHub · {running}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text}"))
    }

    #[test]
    fn versions_order_by_semver_with_numeric_pre_release_fields() {
        let ascending = [
            "0.0.1",
            "0.1.0-pre.2",
            "0.1.0-pre.9",
            "0.1.0-pre.10",
            "0.1.0-pre.12",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1-pre.1",
            "0.2.0",
            "1.0.0",
        ];
        for pair in ascending.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("v0.1.0-pre.12"), v("0.1.0-pre.12"));
        assert_eq!(v("0.1.0+build.5"), v("0.1.0"));
        assert_eq!(v("v0.1.0-pre.12").to_string(), "0.1.0-pre.12");
        assert_eq!(v("1.2.3-rc.1.x-y").to_string(), "1.2.3-rc.1.x-y");
    }

    #[test]
    fn junk_is_no_version() {
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1..3",
            "1.2.3-",
            "1.2.3-pre..1",
            "1.2.3-p_re",
            "latest",
            " -1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_built_version_parses() {
        assert!(
            Version::parse(crate::VERSION).is_some(),
            "{}",
            crate::VERSION
        );
    }

    /// Trimmed shape of `GET /repos/<owner>/<repo>/releases`.
    const FIXTURE: &str = r#"[
      {"tag_name": "v0.1.0-pre.13", "draft": false, "prerelease": true,
       "html_url": "https://example.com/not-used",
       "assets": [
         {"name": "OxiYapper-0.1.0-pre.13-windows-x64.exe", "size": 31000000,
          "browser_download_url": "https://github.com/makarasty/OxiYapper/releases/download/v0.1.0-pre.13/OxiYapper-0.1.0-pre.13-windows-x64.exe"},
         {"name": "OxiYapper-0.1.0-pre.13-windows-x64.zip", "size": 30000000,
          "browser_download_url": "https://github.com/makarasty/OxiYapper/releases/download/v0.1.0-pre.13/OxiYapper-0.1.0-pre.13-windows-x64.zip"},
         {"name": "SHA256SUMS", "size": 200,
          "browser_download_url": "https://github.com/makarasty/OxiYapper/releases/download/v0.1.0-pre.13/SHA256SUMS"}
       ]},
      {"tag_name": "v0.2.0", "draft": true, "assets": []},
      {"tag_name": "nightly", "draft": false, "assets": []},
      {"tag_name": "v0.1.0-pre.9", "draft": false, "prerelease": true, "assets": []},
      {"tag_name": "v0.1.0-pre.10", "draft": false, "prerelease": true, "unknown_field": 1}
    ]"#;

    #[test]
    fn the_release_list_keeps_published_versions_and_their_two_files() {
        let releases = parse_releases(FIXTURE).unwrap();
        let tags: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(tags, ["0.1.0-pre.13", "0.1.0-pre.9", "0.1.0-pre.10"]);
        let top = &releases[0];
        assert_eq!(top.exe.as_ref().unwrap().size, 31_000_000);
        assert!(
            top.exe
                .as_ref()
                .unwrap()
                .url
                .ends_with("/OxiYapper-0.1.0-pre.13-windows-x64.exe")
        );
        assert!(top.sums.as_ref().unwrap().url.ends_with("/SHA256SUMS"));
        assert_eq!(
            top.page(),
            "https://github.com/makarasty/OxiYapper/releases/tag/v0.1.0-pre.13"
        );
        assert_eq!(releases[1].exe, None);
        assert!(parse_releases(r#"{"message": "API rate limit exceeded"}"#).is_err());
    }

    #[test]
    fn the_newest_release_above_the_running_one_wins() {
        let releases = parse_releases(FIXTURE).unwrap();
        let pick =
            |current: &str| newest(releases.clone(), &v(current)).map(|r| r.version.to_string());
        assert_eq!(pick("0.1.0-pre.12").as_deref(), Some("0.1.0-pre.13"));
        assert_eq!(pick("0.0.1").as_deref(), Some("0.1.0-pre.13"));
        assert_eq!(pick("0.1.0-pre.13"), None, "same version");
        assert_eq!(
            pick("0.1.0"),
            None,
            "a release is newer than its pre-releases"
        );
        assert_eq!(pick("0.1.0-pre.10").as_deref(), Some("0.1.0-pre.13"));
    }

    #[test]
    fn checksum_finds_the_line_of_the_file() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  OxiYapper-1.0.0-windows-x64.zip\n{b} *OxiYapper-1.0.0-windows-x64.exe\r\nshort  x.exe\n"
        );
        assert_eq!(
            checksum(&sums, "OxiYapper-1.0.0-windows-x64.exe"),
            Some("b".repeat(64))
        );
        assert_eq!(checksum(&sums, "OxiYapper-1.0.0-windows-x64.zip"), Some(a));
        assert_eq!(checksum(&sums, "x.exe"), None, "not a SHA256");
        assert_eq!(checksum(&sums, "OxiYapper-1.0.0-windows-x64"), None);
        assert_eq!(checksum("", "x"), None);
    }

    #[test]
    fn status_survives_the_wire_and_reads_as_a_line() {
        let all = [
            Status::Unknown,
            Status::Off,
            Status::Checking,
            Status::UpToDate,
            Status::Available {
                version: "0.1.0-pre.13".into(),
                page: "https://github.com/x".into(),
            },
            Status::Downloading {
                version: "0.1.0-pre.13".into(),
                percent: 40,
            },
            Status::Ready {
                version: "0.1.0-pre.13".into(),
            },
            Status::Updated,
            Status::Failed,
        ];
        for status in all {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<Status>(&json).unwrap(),
                status,
                "{json}"
            );
            assert_ne!(status_line(&status, "0.1.0-pre.12"), "");
        }
        assert_eq!(
            status_line(&Status::UpToDate, "0.1.0-pre.12"),
            "Up to date · 0.1.0-pre.12"
        );
        assert_eq!(
            status_line(
                &Status::Downloading {
                    version: "0.1.0-pre.13".into(),
                    percent: 40
                },
                "x"
            ),
            "Downloading 0.1.0-pre.13… 40 %"
        );
    }
}
