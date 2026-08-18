//! `resign`: refresh a channel's expiries without changing what it names.
//!
//! Metadata goes stale on a clock the publisher does not control — timestamp 14
//! days after each signature, snapshot and targets 60 — so a channel that stops
//! publishing eventually stops verifying, and clients see a dead channel rather
//! than a quiet one. Re-signing advances every online role's version and expiry
//! while leaving the targets it lists byte-for-byte as they were, which is what
//! lets a scheduled job keep a channel valid with no release to make.
//!
//! Only the online keys sign here. Root's own expiry is a year out and its key
//! is offline, so nothing in this path needs it.

use std::path::PathBuf;

use jiff::Timestamp;
use retrovert_tuf::{RoleName, policy};

use crate::chain::{self, SignedRole};
use crate::error::Result;
use crate::workspace::Workspace;

/// What `resign` wrote.
#[derive(Debug, Clone)]
pub struct ResignReport {
    /// The three online roles as this re-sign left them.
    pub roles: Vec<SignedRole>,
    /// Every file written, in publication order; the closing `timestamp.json`
    /// write is what makes the refreshed chain current.
    pub written: Vec<PathBuf>,
}

/// Re-sign the online roles of `workspace`'s channel, dated `now`.
pub fn resign(workspace: &Workspace, now: Timestamp) -> Result<ResignReport> {
    let channel = workspace.channel();
    let next = chain::Next::read(&channel)?;

    let mut targets = next.current_targets()?;
    targets.version = next.targets;
    targets.expires = policy::expires(RoleName::Targets, now)?;

    let committed = chain::commit(&channel, &workspace.keys(), targets, &next, now)?;
    Ok(ResignReport {
        roles: committed.roles,
        written: committed.written,
    })
}
