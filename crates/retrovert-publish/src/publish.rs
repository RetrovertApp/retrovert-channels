//! `publish`: commit a release-set manifest to a channel as its next
//! generation.
//!
//! The manifest becomes the channel's sole TUF target. Files land in
//! publication order — target, targets, snapshot, then timestamp — and every
//! file before the last gets a fresh consistent-snapshot name, so an
//! interrupted publish leaves the previous generation untouched. The atomic
//! `timestamp.json` write is the commit point.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use retrovert_tuf::{
    RoleName, TargetFile, Targets, manifest, metadata::sha256_map, policy, target_published_name,
};

use crate::chain;
use crate::error::{Error, Result};
use crate::workspace::Workspace;

/// What `publish` wrote.
#[derive(Debug, Clone)]
pub struct PublishReport {
    /// The generation id: the digest of the manifest's exact bytes.
    pub generation_id: String,
    /// The aggregate repository commit the release set was gathered from.
    pub source_revision: String,
    /// The channel's release-set number, matching its `<channel>/vN` tag.
    pub version: u64,
    /// Every file written, in publication order; the closing `timestamp.json`
    /// write commits the generation.
    pub written: Vec<PathBuf>,
}

/// Publish the release-set manifest at `manifest_path` into `workspace`'s
/// channel, dated `now`.
///
/// Re-publishing byte-identical manifest bytes reproduces the same generation
/// id; metadata versions still advance, so clients see a fresh chain either
/// way.
pub fn publish(
    workspace: &Workspace,
    manifest_path: &Path,
    now: Timestamp,
) -> Result<PublishReport> {
    let manifest_bytes = std::fs::read(manifest_path).map_err(|e| Error::io(manifest_path, e))?;
    let manifest = manifest::Manifest::parse(&manifest_bytes)?;
    let generation_id = manifest::generation_id(&manifest_bytes);

    let channel = workspace.channel();
    let next = chain::Next::read(&channel)?;

    let manifest_entry = TargetFile {
        length: manifest_bytes.len() as u64,
        hashes: sha256_map(&manifest_bytes),
        extra: BTreeMap::new(),
    };
    let targets = Targets::new(
        next.targets,
        policy::expires(RoleName::Targets, now)?,
        BTreeMap::from([(manifest::TARGET_PATH.to_string(), manifest_entry)]),
    );

    let target_name = target_published_name(manifest::TARGET_PATH, &generation_id);
    channel.write_target(&target_name, &manifest_bytes)?;
    let mut written = vec![channel.targets_dir().join(&target_name)];
    written.extend(chain::commit(&channel, &workspace.keys(), targets, &next, now)?.written);

    Ok(PublishReport {
        generation_id,
        source_revision: manifest.source_revision,
        version: manifest.version,
        written,
    })
}
