//! The channel's online metadata chain: what a write supersedes, and how it is
//! committed.
//!
//! Publishing a generation and re-signing an expiry differ only in what the
//! targets role ends up saying. Reading the versions in force, signing targets
//! and the snapshot and timestamp that pin it, and writing the three in the
//! order that leaves the closing `timestamp.json` as the commit point is one
//! operation either way — and only the online keys take part in it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use retrovert_tuf::{
    Channel, MetaFile, RoleName, Signed, Snapshot, Targets, policy, published_names, versioned_name,
};
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::keys::KeyStore;

/// A role as one write left it.
#[derive(Debug, Clone)]
pub struct SignedRole {
    /// Which role.
    pub role: RoleName,
    /// The version it now carries.
    pub version: u64,
    /// When it now expires, RFC 3339 in UTC.
    pub expires: String,
}

/// The versions a channel's next write advances its online roles to.
pub(crate) struct Next {
    pub targets: u64,
    pub snapshot: u64,
    pub timestamp: u64,
    /// The targets file that write supersedes.
    current_targets: PathBuf,
}

impl Next {
    /// Read the chain in force in `channel`: timestamp names the snapshot,
    /// which names the targets.
    pub fn read(channel: &Channel) -> Result<Self> {
        let metadata = channel.metadata_dir();

        let timestamp_path = metadata.join(RoleName::Timestamp.file_name());
        let timestamp: Signed<retrovert_tuf::Timestamp> = read_signed(&timestamp_path)?;
        let snapshot_pin =
            pinned_version(&timestamp.signed.meta, &timestamp_path, RoleName::Snapshot)?;

        let snapshot_path = metadata.join(versioned_name(RoleName::Snapshot, snapshot_pin));
        let snapshot: Signed<Snapshot> = read_signed(&snapshot_path)?;
        let targets_pin = pinned_version(&snapshot.signed.meta, &snapshot_path, RoleName::Targets)?;

        Ok(Self {
            targets: next_version(targets_pin, &snapshot_path)?,
            snapshot: next_version(snapshot.signed.version, &snapshot_path)?,
            timestamp: next_version(timestamp.signed.version, &timestamp_path)?,
            current_targets: metadata.join(versioned_name(RoleName::Targets, targets_pin)),
        })
    }

    /// The targets role currently in force, unknown fields and all.
    pub fn current_targets(&self) -> Result<Targets> {
        Ok(read_signed::<Targets>(&self.current_targets)?.signed)
    }
}

/// What [`commit`] wrote.
pub(crate) struct Committed {
    /// Every file written, in publication order.
    pub written: Vec<PathBuf>,
    /// The three online roles as this write left them.
    pub roles: Vec<SignedRole>,
}

/// Sign `targets` and the snapshot and timestamp that pin it with `store`'s
/// online keys, then write all three into `channel`.
///
/// `targets` carries its own version and expiry; the snapshot and timestamp
/// take theirs from `next` and the channel's expiry policy.
pub(crate) fn commit(
    channel: &Channel,
    store: &KeyStore,
    targets: Targets,
    next: &Next,
    now: Timestamp,
) -> Result<Committed> {
    let targets_version = targets.version;
    let targets_expires = targets.expires.clone();
    let targets_bytes = Signed::new(targets, &[&store.read(RoleName::Targets)?])?.to_json()?;

    let snapshot_expires = policy::expires(RoleName::Snapshot, now)?;
    let snapshot_bytes = Signed::new(
        Snapshot::new(
            next.snapshot,
            snapshot_expires.clone(),
            BTreeMap::from([(
                RoleName::Targets.file_name(),
                MetaFile::pinning(targets_version, &targets_bytes),
            )]),
        ),
        &[&store.read(RoleName::Snapshot)?],
    )?
    .to_json()?;

    let timestamp_expires = policy::expires(RoleName::Timestamp, now)?;
    let timestamp_bytes = Signed::new(
        retrovert_tuf::Timestamp::new(
            next.timestamp,
            timestamp_expires.clone(),
            MetaFile::pinning(next.snapshot, &snapshot_bytes),
        ),
        &[&store.read(RoleName::Timestamp)?],
    )?
    .to_json()?;

    let signed = [
        (
            RoleName::Targets,
            targets_version,
            targets_expires,
            targets_bytes,
        ),
        (
            RoleName::Snapshot,
            next.snapshot,
            snapshot_expires,
            snapshot_bytes,
        ),
        (
            RoleName::Timestamp,
            next.timestamp,
            timestamp_expires,
            timestamp_bytes,
        ),
    ];

    let mut committed = Committed {
        written: Vec::new(),
        roles: Vec::new(),
    };
    for (role, version, expires, bytes) in signed {
        for name in published_names(role, version) {
            channel.write_metadata(&name, &bytes)?;
            committed.written.push(channel.metadata_dir().join(name));
        }
        committed.roles.push(SignedRole {
            role,
            version,
            expires,
        });
    }
    Ok(committed)
}

fn read_signed<T: DeserializeOwned>(path: &Path) -> Result<Signed<T>> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Metadata {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Advance a version counter read from `path`, refusing to wrap.
fn next_version(version: u64, path: &Path) -> Result<u64> {
    version
        .checked_add(1)
        .ok_or_else(|| Error::VersionExhausted {
            path: path.to_path_buf(),
            version,
        })
}

fn pinned_version(meta: &BTreeMap<String, MetaFile>, path: &Path, child: RoleName) -> Result<u64> {
    let name = child.file_name();
    match meta.get(&name) {
        Some(pin) => Ok(pin.version),
        None => Err(Error::MissingPin {
            path: path.to_path_buf(),
            name,
        }),
    }
}
