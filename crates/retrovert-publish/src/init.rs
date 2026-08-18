//! `init`: create a channel from nothing, signed by a fresh disposable root.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jiff::Timestamp;
use retrovert_tuf::{
    Channel, KeyPair, MetaFile, PublicKey, RoleName, Root, Signed, Snapshot, Targets, policy,
    published_names,
};

use crate::error::{Error, Result};
use crate::workspace::Workspace;

/// The version every role carries in a freshly initialized channel.
pub const INITIAL_VERSION: u64 = 1;

/// One signing key per top-level role.
#[derive(Debug, Clone)]
pub struct KeySet {
    /// Signs `root`; kept offline.
    pub root: KeyPair,
    /// Signs `targets`.
    pub targets: KeyPair,
    /// Signs `snapshot`.
    pub snapshot: KeyPair,
    /// Signs `timestamp`.
    pub timestamp: KeyPair,
}

impl KeySet {
    /// Generate four independent keys from the OS random source.
    pub fn generate() -> Result<Self> {
        Ok(Self {
            root: KeyPair::generate()?,
            targets: KeyPair::generate()?,
            snapshot: KeyPair::generate()?,
            timestamp: KeyPair::generate()?,
        })
    }

    /// The key that signs `role`.
    #[must_use]
    pub fn get(&self, role: RoleName) -> &KeyPair {
        match role {
            RoleName::Root => &self.root,
            RoleName::Targets => &self.targets,
            RoleName::Snapshot => &self.snapshot,
            RoleName::Timestamp => &self.timestamp,
        }
    }

    fn public_keys(&self) -> Vec<(RoleName, PublicKey)> {
        RoleName::ALL
            .iter()
            .map(|role| (*role, self.get(*role).public()))
            .collect()
    }
}

/// What `init` wrote.
#[derive(Debug, Clone)]
pub struct InitReport {
    /// Metadata files written, in publication order.
    pub metadata: Vec<PathBuf>,
    /// Private-key files written.
    pub keys: Vec<PathBuf>,
    /// The root key's TUF key ID — the fingerprint clients pin.
    pub root_key_id: String,
}

/// Initialize `workspace` as a channel signed by `keys`, dated `now`.
///
/// Refuses a non-empty workspace unless `force`, so an existing channel's keys
/// cannot be silently replaced.
pub fn init(
    workspace: &Workspace,
    keys: &KeySet,
    now: Timestamp,
    force: bool,
) -> Result<InitReport> {
    if !force && !workspace.is_empty()? {
        return Err(Error::NotEmpty(workspace.path().to_path_buf()));
    }

    let store = workspace.keys();
    store.create_dirs()?;
    for role in RoleName::ALL {
        store.write(role, keys.get(role))?;
    }

    let channel = workspace.channel();
    // A forced re-init replaces the channel wholesale. Clear the published
    // directories first: metadata and targets from the previous channel are
    // unreferenced by the new chain but would otherwise still be uploaded and
    // fetchable by direct URL.
    if force {
        for dir in [channel.metadata_dir(), channel.targets_dir()] {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(dir, e)),
            }
        }
    }
    channel.create_dirs()?;

    // Signing order is the publication order: a role is signed only after
    // everything it pins, so timestamp lands last and commits the set.
    let root = Signed::new(
        Root::single_key_per_role(
            INITIAL_VERSION,
            policy::expires(RoleName::Root, now)?,
            &keys.public_keys(),
        )?,
        &[&keys.root],
    )?
    .to_json()?;

    let targets = Signed::new(
        Targets::new(
            INITIAL_VERSION,
            policy::expires(RoleName::Targets, now)?,
            BTreeMap::new(),
        ),
        &[&keys.targets],
    )?
    .to_json()?;

    let snapshot = Signed::new(
        Snapshot::new(
            INITIAL_VERSION,
            policy::expires(RoleName::Snapshot, now)?,
            BTreeMap::from([(
                RoleName::Targets.file_name(),
                MetaFile::pinning(INITIAL_VERSION, &targets),
            )]),
        ),
        &[&keys.snapshot],
    )?
    .to_json()?;

    let timestamp = Signed::new(
        retrovert_tuf::Timestamp::new(
            INITIAL_VERSION,
            policy::expires(RoleName::Timestamp, now)?,
            MetaFile::pinning(INITIAL_VERSION, &snapshot),
        ),
        &[&keys.timestamp],
    )?
    .to_json()?;

    let written = [
        (RoleName::Root, root),
        (RoleName::Targets, targets),
        (RoleName::Snapshot, snapshot),
        (RoleName::Timestamp, timestamp),
    ]
    .into_iter()
    .map(|(role, bytes)| write_role(&channel, role, &bytes))
    .collect::<Result<Vec<_>>>()?
    .concat();

    Ok(InitReport {
        metadata: written,
        keys: RoleName::ALL.iter().map(|r| store.key_path(*r)).collect(),
        root_key_id: keys.root.key_id()?,
    })
}

fn write_role(channel: &Channel, role: RoleName, bytes: &[u8]) -> Result<Vec<PathBuf>> {
    published_names(role, INITIAL_VERSION)
        .into_iter()
        .map(|name| {
            channel.write_metadata(&name, bytes)?;
            Ok(channel.metadata_dir().join(name))
        })
        .collect()
}
