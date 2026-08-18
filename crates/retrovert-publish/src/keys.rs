//! Private-key storage on the publisher's side.
//!
//! The directory split is the trust boundary: `online/` holds the keys the
//! signing workflow needs and that are destined for a protected GitHub
//! environment; `offline/` holds the root key, which never reaches CI. Keeping
//! them apart at rest makes "which keys may this job see" a directory-level
//! question rather than a per-file one.

use std::io::Write;
use std::path::{Path, PathBuf};

use retrovert_tuf::{KeyPair, RoleName};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// A directory of PKCS#8 PEM private keys.
///
/// # Platform
///
/// Owner-only permissions are enforced on Unix. On other platforms the key
/// files inherit whatever the parent directory's ACL grants, so the store's
/// confidentiality is only as good as the directory it is created in — see
/// [`KeyStore::write`].
#[derive(Debug, Clone)]
pub struct KeyStore {
    path: PathBuf,
}

impl KeyStore {
    /// Address the store rooted at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keys that CI is allowed to hold.
    #[must_use]
    pub fn online_dir(&self) -> PathBuf {
        self.path.join("online")
    }

    /// Keys that must stay off CI.
    #[must_use]
    pub fn offline_dir(&self) -> PathBuf {
        self.path.join("offline")
    }

    /// Where `role`'s private key lives.
    #[must_use]
    pub fn key_path(&self, role: RoleName) -> PathBuf {
        let dir = match role {
            RoleName::Root => self.offline_dir(),
            RoleName::Targets | RoleName::Snapshot | RoleName::Timestamp => self.online_dir(),
        };
        dir.join(format!("{role}.pem"))
    }

    /// Create the store root and the online and offline directories, each
    /// owner-only.
    ///
    /// Refuses a symlinked directory: re-initializing over a workspace someone
    /// else prepared must not redirect private keys out of the restricted
    /// directories this store creates.
    pub fn create_dirs(&self) -> Result<()> {
        for dir in [self.path.clone(), self.online_dir(), self.offline_dir()] {
            std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
            let meta = std::fs::symlink_metadata(&dir).map_err(|e| Error::io(&dir, e))?;
            if meta.file_type().is_symlink() {
                return Err(Error::KeyDirIsSymlink(dir));
            }
            restrict(&dir, 0o700)?;
        }
        Ok(())
    }

    /// Write `role`'s private key as PKCS#8 PEM, readable only by its owner.
    ///
    /// Any existing entry is unlinked rather than truncated, so a symlink left
    /// at this path cannot redirect the key elsewhere and the owner-only mode
    /// applies from the moment the file exists rather than after the secret has
    /// already been written. See [`KeyStore`] for the non-Unix caveat.
    pub fn write(&self, role: RoleName, key: &KeyPair) -> Result<()> {
        let path = self.key_path(role);
        let pem = key.to_pkcs8_pem()?;

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&path, e)),
        }

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| Error::io(&path, e))?;
        file.write_all(pem.as_bytes())
            .map_err(|e| Error::io(&path, e))?;
        // `mode` above is masked by the process umask, which can only clear
        // bits; normalize so the stored mode does not depend on the caller's
        // environment.
        restrict(&path, 0o600)
    }

    /// Read `role`'s private key back.
    pub fn read(&self, role: RoleName) -> Result<KeyPair> {
        let path = self.key_path(role);
        // Zeroizing so the plaintext PEM does not linger in the heap after the
        // key is parsed, matching the write path.
        let pem = Zeroizing::new(std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?);
        Ok(KeyPair::from_pkcs8_pem(&pem)?)
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io(path, e))
}

/// No-op off Unix: there is no portable mode to set, and the caller is told so
/// by [`KeyStore`]'s documentation and by the CLI's own output.
#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
