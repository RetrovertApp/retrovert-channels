//! `pull`: make a workspace's channel match what the live channel serves.
//!
//! A machine holding only the online keys — the scheduled re-sign job — has no
//! channel state of its own, and the host is the only place the current
//! metadata exists. Taking the host's word for it would be a mistake: bytes
//! fetched from a host and then re-signed with the online keys would carry that
//! host's choices into metadata every client accepts. So nothing is written
//! until the whole chain has verified against the root the workspace already
//! pins, checked by the same client library a consumer runs.
//!
//! The root is an input here, never an output. A pull authenticates against it
//! and leaves it alone: a trust anchor learned from the thing it authenticates
//! is not one.
//!
//! Refusing to answer reads costs one protection worth naming: `sigstore-tuf`
//! would otherwise seed a version floor from the store, and without it a host
//! serving an older — but still signed, still unexpired — chain would be pulled
//! and re-signed with fresh expiries, turning a freeze into an indefinite one.
//! A stateless job has nothing honest to build that floor from, and seeding it
//! from the workspace would break the retry case instead: after a re-sign the
//! workspace is deliberately a version ahead of the channel. Closing this needs
//! a floor committed somewhere that outlives the run.
//!
//! One further consequence: verification includes expiry, so a channel
//! whose metadata has already lapsed cannot be pulled, and therefore cannot be
//! re-signed unattended. That is the intended edge — metadata nobody can verify
//! is not metadata to re-sign automatically — and it is what puts a floor under
//! how long the scheduled job may go unnoticed. Recovering from there needs the
//! publisher's own workspace.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use retrovert_tuf::{RoleName, published_names};
use sigstore_tuf::{MetadataStore, Updater};

use crate::chain::SignedRole;
use crate::error::{Error, Result};
use crate::verify::HttpChannel;
use crate::workspace::Workspace;

/// What `pull` wrote.
#[derive(Debug, Clone)]
pub struct PullReport {
    /// The three online roles as the channel currently serves them.
    pub roles: Vec<SignedRole>,
    /// Every file written, in publication order.
    pub written: Vec<PathBuf>,
}

/// Replace the online metadata of `workspace`'s channel with what the channel
/// at `base_url` serves as of `now`.
///
/// Fails without writing anything if the channel does not verify against the
/// workspace's root.
pub fn pull(workspace: &Workspace, base_url: &str, now: Timestamp) -> Result<PullReport> {
    let channel = workspace.channel();
    let root_path = channel.metadata_dir().join(RoleName::Root.file_name());
    let root = std::fs::read(&root_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::MissingRoot(root_path.clone()),
        _ => Error::io(&root_path, e),
    })?;

    let verified = Verified::default();
    let captured = verified.share();
    let mut updater = Updater::new(HttpChannel::new(base_url), &root)?.with_store(verified);
    pollster::block_on(updater.refresh(now))?;

    let trusted = updater.trusted();
    let roles = [RoleName::Targets, RoleName::Snapshot, RoleName::Timestamp]
        .into_iter()
        .map(|role| {
            let (version, expires) = match role {
                RoleName::Targets => trusted.targets().map(|r| (r.version, r.expires.clone())),
                RoleName::Snapshot => trusted.snapshot().map(|r| (r.version, r.expires.clone())),
                RoleName::Timestamp => trusted.timestamp().map(|r| (r.version, r.expires.clone())),
                RoleName::Root => None,
            }
            .ok_or_else(|| Error::IncompleteRefresh(role.file_name()))?;
            Ok(SignedRole {
                role,
                version,
                expires,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Written in the chain's own order, so an interrupted pull cannot leave a
    // timestamp naming a snapshot the workspace does not have.
    let captured = captured.lock().unwrap_or_else(PoisonError::into_inner);
    let mut written = Vec::new();
    for role in &roles {
        let name = role.role.file_name();
        let bytes = captured
            .get(&name)
            .ok_or_else(|| Error::IncompleteRefresh(name.clone()))?;
        for published in published_names(role.role, role.version) {
            channel.write_metadata(&published, bytes)?;
            written.push(channel.metadata_dir().join(published));
        }
    }

    Ok(PullReport { roles, written })
}

/// Metadata captured as the client accepts it.
///
/// `sigstore-tuf` writes each metadata file through its store only after
/// verifying it, so what lands here is the byte-exact set the pinned root
/// authenticates. It is a capture, not a cache: `load` never answers, so the
/// updater cannot seed its trusted set from anything but the network, and every
/// pull sees the channel as it is right now.
#[derive(Default)]
struct Verified(Captured);

type Captured = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

impl Verified {
    fn share(&self) -> Captured {
        Arc::clone(&self.0)
    }
}

impl MetadataStore for Verified {
    fn load(&self, _name: &str) -> Option<Vec<u8>> {
        None
    }

    fn store(&self, name: &str, bytes: &[u8]) -> sigstore_tuf::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_string(), bytes.to_vec());
        Ok(())
    }
}
