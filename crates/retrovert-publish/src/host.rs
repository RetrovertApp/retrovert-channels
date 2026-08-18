//! The release host a channel is served from.
//!
//! [`ReleaseHost`] is the seam: the rules that make a publish safe — what order
//! files land in, and that replacing a name never destroys the old bytes before
//! the new ones exist — are host-independent, so they stay testable without a
//! network. [`GitHubReleases`] is the one real implementation.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Environment variables a token is read from, in order.
const TOKEN_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];

/// Suffix an asset is uploaded under while it is being swapped in.
const STAGING_SUFFIX: &str = ".incoming";

/// A GitHub repository, `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    /// The account the repository belongs to.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repository name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl FromStr for Repo {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let malformed = || Error::MalformedRepo(s.to_string());
        let (owner, name) = s.split_once('/').ok_or_else(malformed)?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return Err(malformed());
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A host that serves a channel's files as named assets on named releases.
pub trait ReleaseHost {
    /// Create `tag`'s release unless it already exists.
    fn ensure_release(&self, tag: &str, prerelease: bool) -> Result<()>;

    /// Publish `bytes` under `name`, replacing whatever that name held.
    fn put_asset(&self, tag: &str, name: &str, bytes: &[u8]) -> Result<()>;
}

/// Releases in a GitHub repository, reached over the REST API.
pub struct GitHubReleases {
    repo: Repo,
    token: String,
    api: String,
    uploads: String,
    agent: ureq::Agent,
}

impl GitHubReleases {
    /// Address `repo`'s releases on github.com, authenticating with `token`.
    pub fn new(repo: Repo, token: impl Into<String>) -> Self {
        Self::with_endpoints(
            repo,
            token,
            "https://api.github.com",
            "https://uploads.github.com",
        )
    }

    /// Address `repo`'s releases on a deployment that serves the API and the
    /// upload endpoint from somewhere other than github.com.
    ///
    /// Both are given because GitHub splits them: uploads do not go to the API
    /// host. A trailing separator on either is dropped, since paths are joined
    /// onto them with one of their own.
    pub fn with_endpoints(
        repo: Repo,
        token: impl Into<String>,
        api: impl Into<String>,
        uploads: impl Into<String>,
    ) -> Self {
        Self {
            repo,
            token: token.into(),
            api: origin(api),
            uploads: origin(uploads),
            agent: agent(),
        }
    }

    /// Address `repo`'s releases with the token in `GH_TOKEN` or `GITHUB_TOKEN`.
    pub fn from_env(repo: Repo) -> Result<Self> {
        let token = TOKEN_VARS
            .iter()
            .find_map(|var| std::env::var(var).ok().filter(|t| !t.is_empty()))
            .ok_or(Error::MissingToken)?;
        Ok(Self::new(repo, token))
    }

    /// The repository being published to.
    #[must_use]
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    fn release(&self, tag: &str) -> Result<Option<Release>> {
        let url = format!(
            "{}/repos/{}/releases/tags/{}",
            self.api,
            self.repo,
            encode_segment(tag)
        );
        let (status, body) = self.send(Method::Get, &url, None)?;
        if status == 404 {
            return Ok(None);
        }
        let body = expect_ok(Method::Get, &url, status, body)?;
        Release::parse(&body).map(Some).ok_or_else(|| Error::Host {
            method: "GET",
            url,
            message: "response was not a release".to_string(),
        })
    }

    fn upload_asset(&self, release_id: u64, name: &str, bytes: &[u8]) -> Result<u64> {
        let url = format!(
            "{}/repos/{}/releases/{release_id}/assets?name={}",
            self.uploads,
            self.repo,
            encode_segment(name)
        );
        let (status, body) = self.send(
            Method::Post,
            &url,
            Some(Payload {
                content_type: "application/octet-stream",
                bytes,
            }),
        )?;
        let body = expect_ok(Method::Post, &url, status, body)?;
        serde_json::from_str(&body)
            .ok()
            .as_ref()
            .and_then(id_of)
            .ok_or_else(|| Error::Host {
                method: "POST",
                url,
                message: "upload response carried no asset id".to_string(),
            })
    }

    fn delete_asset(&self, asset_id: u64) -> Result<()> {
        let url = format!(
            "{}/repos/{}/releases/assets/{asset_id}",
            self.api, self.repo
        );
        let (status, body) = self.send(Method::Delete, &url, None)?;
        // A concurrent delete is the outcome this call wanted anyway.
        if status == 404 {
            return Ok(());
        }
        expect_ok(Method::Delete, &url, status, body).map(drop)
    }

    fn rename_asset(&self, asset_id: u64, name: &str) -> Result<()> {
        let url = format!(
            "{}/repos/{}/releases/assets/{asset_id}",
            self.api, self.repo
        );
        let body = serde_json::json!({ "name": name }).to_string();
        let (status, response) = self.send(
            Method::Patch,
            &url,
            Some(Payload {
                content_type: "application/json",
                bytes: body.as_bytes(),
            }),
        )?;
        expect_ok(Method::Patch, &url, status, response).map(drop)
    }

    fn send(
        &self,
        method: Method,
        url: &str,
        payload: Option<Payload<'_>>,
    ) -> Result<(u16, String)> {
        let fault = |e: &dyn fmt::Display| Error::Host {
            method: method.as_str(),
            url: url.to_string(),
            message: e.to_string(),
        };

        let mut request = ureq::http::Request::builder()
            .method(method.as_str())
            .uri(url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(payload) = &payload {
            request = request.header("content-type", payload.content_type);
        }
        let request = request
            .body(payload.map_or(&[][..], |p| p.bytes))
            .map_err(|e| fault(&e))?;

        let mut response = self.agent.run(request).map_err(|e| fault(&e))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| fault(&e))?;
        Ok((status, body))
    }
}

impl ReleaseHost for GitHubReleases {
    fn ensure_release(&self, tag: &str, prerelease: bool) -> Result<()> {
        if self.release(tag)?.is_some() {
            return Ok(());
        }
        let url = format!("{}/repos/{}/releases", self.api, self.repo);
        let body = serde_json::json!({
            "tag_name": tag,
            "name": tag,
            "prerelease": prerelease,
        })
        .to_string();
        let (status, response) = self.send(
            Method::Post,
            &url,
            Some(Payload {
                content_type: "application/json",
                bytes: body.as_bytes(),
            }),
        )?;
        // Another publisher winning the create race leaves the release this call
        // wanted, so the 422 it gets back is success.
        if status == 422 && self.release(tag)?.is_some() {
            return Ok(());
        }
        expect_ok(Method::Post, &url, status, response).map(drop)
    }

    /// GitHub has no atomic asset replacement, so a name already in use is
    /// swapped rather than overwritten: the new bytes go up under a staging name
    /// first, and only then is the old asset dropped and the staged one renamed
    /// over it. That keeps the window where the published name resolves to
    /// nothing down to a single rename call instead of a whole upload — and if
    /// the process dies inside that window, the bytes are still on the release
    /// under the staging name for the next run to clear.
    fn put_asset(&self, tag: &str, name: &str, bytes: &[u8]) -> Result<()> {
        let release = self.release(tag)?.ok_or_else(|| Error::MissingRelease {
            repo: self.repo.to_string(),
            tag: tag.to_string(),
        })?;

        let staging = format!("{name}{STAGING_SUFFIX}");
        if let Some(abandoned) = release.asset_id(&staging) {
            self.delete_asset(abandoned)?;
        }

        let Some(published) = release.asset_id(name) else {
            self.upload_asset(release.id, name, bytes)?;
            return Ok(());
        };
        let staged = self.upload_asset(release.id, &staging, bytes)?;
        self.delete_asset(published)?;
        self.rename_asset(staged, name)
    }
}

/// A release and the assets currently on it.
struct Release {
    id: u64,
    assets: Vec<(String, u64)>,
}

impl Release {
    /// Read the fields this crate uses, ignoring the rest of the response.
    ///
    /// Returns `None` for a body that is not a release. A cache or a proxy
    /// answering 200 with something else would otherwise read as an empty
    /// release under id 0, and the publish would go on to upload against it and
    /// fail somewhere further along with the original cause thrown away.
    fn parse(body: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        let assets = match &value["assets"] {
            serde_json::Value::Null => Vec::new(),
            listed => listed
                .as_array()?
                .iter()
                .map(|a| Some((a["name"].as_str()?.to_string(), id_of(a)?)))
                .collect::<Option<_>>()?,
        };
        Some(Self {
            id: id_of(&value)?,
            assets,
        })
    }

    fn asset_id(&self, name: &str) -> Option<u64> {
        self.assets
            .iter()
            .find(|(asset, _)| asset == name)
            .map(|(_, id)| *id)
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

struct Payload<'a> {
    content_type: &'static str,
    bytes: &'a [u8],
}

/// The HTTP agent every request in this crate goes out through, publishing to a
/// host or reading back from one.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!("retrovert-publish/", env!("CARGO_PKG_VERSION")))
        // Statuses are read, not thrown: 404 and 422 are ordinary answers here.
        .http_status_as_error(false)
        .build()
        .into()
}

fn id_of(value: &serde_json::Value) -> Option<u64> {
    value["id"].as_u64()
}

/// An endpoint with no trailing separator, so joining a path onto it cannot
/// double up.
fn origin(endpoint: impl Into<String>) -> String {
    let mut endpoint = endpoint.into();
    endpoint.truncate(endpoint.trim_end_matches('/').len());
    endpoint
}

fn expect_ok(method: Method, url: &str, status: u16, body: String) -> Result<String> {
    if (200..300).contains(&status) {
        return Ok(body);
    }
    Err(Error::Host {
        method: method.as_str(),
        url: url.to_string(),
        message: format!("HTTP {status}: {}", body.trim()),
    })
}

/// Percent-encode one URL path segment or query value.
///
/// Release tags carry a `/` (`dev/v1`), which is a path separator to the API
/// unless it is escaped.
fn encode_segment(segment: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_reference_is_owner_and_name() {
        let repo: Repo = "RetrovertApp/playback_plugins".parse().unwrap();
        assert_eq!(repo.owner(), "RetrovertApp");
        assert_eq!(repo.name(), "playback_plugins");
        assert_eq!(repo.to_string(), "RetrovertApp/playback_plugins");

        for bad in ["", "playback_plugins", "/name", "owner/", "a/b/c"] {
            assert!(bad.parse::<Repo>().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn a_tag_with_a_slash_survives_the_api_path() {
        assert_eq!(encode_segment("dev/v1"), "dev%2Fv1");
        assert_eq!(
            encode_segment("dev/channel-metadata"),
            "dev%2Fchannel-metadata"
        );
        assert_eq!(encode_segment("3.snapshot.json"), "3.snapshot.json");
    }

    #[test]
    fn a_release_reports_the_assets_it_carries() {
        let release = Release::parse(
            &serde_json::json!({
                "id": 7,
                "assets": [
                    { "id": 11, "name": "timestamp.json" },
                    { "id": 12, "name": "2.snapshot.json" },
                ],
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(release.id, 7);
        assert_eq!(release.asset_id("timestamp.json"), Some(11));
        assert_eq!(release.asset_id("2.snapshot.json"), Some(12));
        assert_eq!(release.asset_id("root.json"), None);
    }

    #[test]
    fn a_release_without_assets_carries_none() {
        let release = Release::parse(&serde_json::json!({ "id": 7 }).to_string()).unwrap();
        assert_eq!(release.id, 7);
        assert_eq!(release.asset_id("timestamp.json"), None);
    }

    #[test]
    fn a_body_that_is_not_a_release_is_not_read_as_an_empty_one() {
        for body in [
            "<html>not the api</html>",
            "{}",
            &serde_json::json!({ "id": "seven" }).to_string(),
            &serde_json::json!({ "id": 7, "assets": "none" }).to_string(),
            &serde_json::json!({ "id": 7, "assets": [{ "name": "x.json" }] }).to_string(),
        ] {
            assert!(Release::parse(body).is_none(), "{body} must not parse");
        }
    }

    #[test]
    fn an_endpoint_keeps_no_trailing_separator() {
        for given in ["https://api.github.com", "https://api.github.com/"] {
            assert_eq!(origin(given), "https://api.github.com");
        }
        assert_eq!(origin("https://host/api///"), "https://host/api");
    }

    #[test]
    fn a_failing_status_carries_the_body_into_the_error() {
        let err = expect_ok(Method::Post, "https://host/x", 422, "{\"m\":1}".into()).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("POST"), "{rendered}");
        assert!(rendered.contains("422"), "{rendered}");
        assert!(rendered.contains("{\"m\":1}"), "{rendered}");
    }
}
