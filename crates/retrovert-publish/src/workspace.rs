//! The publisher's on-disk workspace: one channel plus its private keys.

use std::path::{Path, PathBuf};

use retrovert_tuf::Channel;

use crate::error::{Error, Result};
use crate::keys::KeyStore;

/// A directory holding a channel and the keys that sign it.
///
/// `repository/` is the part that ships — it is exactly what a client sees at
/// the channel's base URL. `keys/` never leaves the publisher.
#[derive(Debug, Clone)]
pub struct Workspace {
    path: PathBuf,
}

impl Workspace {
    /// Address the workspace rooted at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The workspace root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The publishable channel.
    #[must_use]
    pub fn channel(&self) -> Channel {
        Channel::new(self.path.join("repository"))
    }

    /// The private keys that sign the channel.
    #[must_use]
    pub fn keys(&self) -> KeyStore {
        KeyStore::new(self.path.join("keys"))
    }

    /// Whether the root directory is absent or has no entries.
    pub fn is_empty(&self) -> Result<bool> {
        match std::fs::read_dir(&self.path) {
            Ok(mut entries) => Ok(entries.next().is_none()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(Error::io(&self.path, e)),
        }
    }
}
