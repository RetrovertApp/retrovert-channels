//! Where a channel lives on a release host, and how a local publish gets there.
//!
//! A channel is two kinds of release. Per-generation assets — the manifest, and
//! the artifacts it names — go on the `<channel>/vN` tag release and are never
//! touched again. The TUF metadata naming the current generation is
//! continuously replaced on one rolling `<channel>/channel-metadata` release,
//! whose flat asset namespace *is* the channel's base URL.
//!
//! The order the local publish wrote in carries over unchanged, because it is
//! what makes a generation appear all at once: every file the new
//! `timestamp.json` transitively pins is published before it, so a run that dies
//! anywhere earlier leaves a channel still naming the previous generation.
//!
//! A workspace decides what it writes, but it does not own the channel: the
//! scheduled re-sign job signs the same roles from CI, so a workspace that has
//! not pulled is a version behind and would write different bytes over names
//! the host already serves. Recovery is always forward — a run that stops short
//! leaves unreferenced assets behind and the next publish supersedes them;
//! nothing is ever un-published.

use std::path::{Path, PathBuf};

use retrovert_tuf::manifest;

use crate::error::{Error, Result};
use crate::host::{ReleaseHost, Repo};
use crate::publish::PublishReport;

/// The channel whose releases are ordinary releases rather than prereleases.
const STABLE: &str = "stable";

/// The rolling release whose assets are the channel: its TUF metadata and the
/// manifest target they authenticate.
#[must_use]
pub fn metadata_tag(channel: &str) -> String {
    format!("{channel}/channel-metadata")
}

/// The release carrying one generation's immutable assets.
#[must_use]
pub fn release_tag(channel: &str, version: u64) -> String {
    format!("{channel}/v{version}")
}

/// The base URL a client resolves `channel` from.
#[must_use]
pub fn base_url(repo: &Repo, channel: &str) -> String {
    format!(
        "https://github.com/{repo}/releases/download/{}/",
        metadata_tag(channel)
    )
}

/// Publish channel metadata onto the rolling metadata release, in the order it
/// was written locally — which is the order that leaves `timestamp.json` last.
///
/// This is the whole of bringing a channel live and the whole of re-signing
/// one; a generation additionally has assets of its own, and goes through
/// [`push_generation`].
///
/// Every asset that reaches the host is appended to `pushed`, so a run that ends
/// in an error still says how far it got.
pub fn push_metadata(
    host: &dyn ReleaseHost,
    channel: &str,
    metadata: &[PathBuf],
    pushed: &mut Vec<String>,
) -> Result<()> {
    let mut push = Push::new(host, None, pushed);
    push.metadata_release(channel)?;
    push.files(&metadata_tag(channel), metadata)
}

/// Publish a generation: the manifest onto its own tag release, then the
/// metadata that names it.
///
/// `stop_after` cuts the run off once that many assets are published, which is
/// how the channel's failed-publish drill is run against the real host.
/// `pushed` accumulates as in [`push_metadata`].
pub fn push_generation(
    host: &dyn ReleaseHost,
    channel: &str,
    report: &PublishReport,
    manifest_bytes: &[u8],
    stop_after: Option<usize>,
    pushed: &mut Vec<String>,
) -> Result<()> {
    let mut push = Push::new(host, stop_after, pushed);

    let tag = release_tag(channel, report.version);
    push.host.ensure_release(&tag, channel != STABLE)?;
    push.put(&tag, manifest::TARGET_PATH, manifest_bytes)?;

    push.metadata_release(channel)?;
    push.files(&metadata_tag(channel), &report.written)
}

struct Push<'a> {
    host: &'a dyn ReleaseHost,
    stop_after: Option<usize>,
    /// How many entries `pushed` already held, so `stop_after` counts this
    /// run's assets rather than the caller's log.
    start: usize,
    pushed: &'a mut Vec<String>,
}

impl<'a> Push<'a> {
    fn new(
        host: &'a dyn ReleaseHost,
        stop_after: Option<usize>,
        pushed: &'a mut Vec<String>,
    ) -> Self {
        Self {
            host,
            stop_after,
            start: pushed.len(),
            pushed,
        }
    }

    /// The metadata release is always a prerelease: it is channel plumbing, and
    /// letting it become the repository's "latest release" would misname it.
    fn metadata_release(&mut self, channel: &str) -> Result<()> {
        self.host.ensure_release(&metadata_tag(channel), true)
    }

    fn files(&mut self, tag: &str, paths: &[PathBuf]) -> Result<()> {
        for path in paths {
            let name = asset_name(path)?;
            let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
            self.put(tag, name, &bytes)?;
        }
        Ok(())
    }

    fn put(&mut self, tag: &str, name: &str, bytes: &[u8]) -> Result<()> {
        if self.stop_after == Some(self.pushed.len() - self.start) {
            return Err(Error::PublishStopped {
                after: self.pushed.len() - self.start,
            });
        }
        self.host.put_asset(tag, name, bytes)?;
        self.pushed.push(format!("{tag}/{name}"));
        Ok(())
    }
}

/// The asset name a local channel file is published under.
///
/// Release assets are one flat namespace, so a channel file's directory is
/// dropped. Consistent snapshots make that safe: every metadata file but
/// `timestamp.json` carries its version, and the manifest target carries its
/// digest.
fn asset_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Unnameable(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_names_one_rolling_release_and_one_release_per_generation() {
        assert_eq!(metadata_tag("dev"), "dev/channel-metadata");
        assert_eq!(release_tag("dev", 1), "dev/v1");
        assert_eq!(release_tag("stable", 42), "stable/v42");
    }

    #[test]
    fn the_base_url_addresses_the_rolling_release() {
        let repo: Repo = "RetrovertApp/playback_plugins".parse().unwrap();
        assert_eq!(
            base_url(&repo, "dev"),
            "https://github.com/RetrovertApp/playback_plugins/releases/download/dev/channel-metadata/"
        );
    }

    #[test]
    fn an_asset_takes_its_name_from_the_file_alone() {
        assert_eq!(
            asset_name(Path::new("/w/repository/metadata/timestamp.json")).unwrap(),
            "timestamp.json"
        );
        assert_eq!(
            asset_name(Path::new("/w/repository/targets/ab.manifest.json")).unwrap(),
            "ab.manifest.json"
        );
        assert!(asset_name(Path::new("/w/..")).is_err());
    }
}
