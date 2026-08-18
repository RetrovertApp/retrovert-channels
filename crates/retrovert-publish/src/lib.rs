//! Publish-side operations for Retrovert release channels.
//!
//! The binary is a thin shell over this library so the same code paths the CLI
//! runs are directly testable.

pub mod chain;
pub mod error;
pub mod host;
pub mod init;
pub mod keys;
pub mod publish;
pub mod pull;
pub mod remote;
pub mod resign;
pub mod verify;
pub mod workspace;

pub use chain::SignedRole;
pub use error::{Error, Result};
pub use host::{GitHubReleases, ReleaseHost, Repo};
pub use init::{InitReport, KeySet, init};
pub use keys::KeyStore;
pub use publish::{PublishReport, publish};
pub use pull::{PullReport, pull};
pub use resign::{ResignReport, resign};
pub use verify::{Generation, HttpChannel, verify};
pub use workspace::Workspace;
