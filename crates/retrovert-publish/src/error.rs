//! Error type for the publish CLI.

use std::path::PathBuf;

/// Errors produced by publish-side commands.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A metadata or key operation failed.
    #[error(transparent)]
    Tuf(#[from] retrovert_tuf::Error),

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An existing metadata file in the channel could not be parsed.
    #[error("{path}: {source}")]
    Metadata {
        /// The metadata file being read.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A parent role does not pin the metadata file publish must supersede.
    #[error("{path}: does not pin {name}")]
    MissingPin {
        /// The parent role's metadata file.
        path: PathBuf,
        /// The unversioned name of the missing pin.
        name: String,
    },

    /// A metadata version counter cannot be advanced. Version numbers this
    /// large never arise from real publishes; the metadata is corrupt.
    #[error("{path}: version {version} cannot be incremented")]
    VersionExhausted {
        /// The metadata file carrying the version.
        path: PathBuf,
        /// The version that could not be advanced.
        version: u64,
    },

    /// `init` was pointed at a directory that already has contents.
    #[error("{0} is not empty; pass --force to re-initialize it and replace its keys")]
    NotEmpty(PathBuf),

    /// A private-key directory already exists as a symlink, which would place
    /// the keys somewhere this tool cannot vouch for.
    #[error("{0} is a symlink; private keys must live in a real directory")]
    KeyDirIsSymlink(PathBuf),

    /// A TUF client operation on a channel failed.
    #[error(transparent)]
    Client(#[from] sigstore_tuf::Error),

    /// A request to the release host failed.
    #[error("{method} {url}: {message}")]
    Host {
        /// The HTTP method used.
        method: &'static str,
        /// The URL requested.
        url: String,
        /// What went wrong, transport error or refused status.
        message: String,
    },

    /// No credential is available for the release host.
    #[error("no release-host token; set GH_TOKEN or GITHUB_TOKEN")]
    MissingToken,

    /// A repository reference was not of the form `owner/name`.
    #[error("{0:?} is not an owner/name repository reference")]
    MalformedRepo(String),

    /// A release that should have been created moments ago is not there.
    #[error("{repo} has no release tagged {tag}")]
    MissingRelease {
        /// The repository hosting the channel.
        repo: String,
        /// The release tag looked for.
        tag: String,
    },

    /// A publish was stopped before its commit point, as asked.
    #[error("stopped after {after} asset(s), before the channel's commit point")]
    PublishStopped {
        /// How many assets had been published when the run stopped.
        after: usize,
    },

    /// A channel file has no file name to publish it under.
    #[error("{0} has no file name to publish as")]
    Unnameable(PathBuf),

    /// A pull found no root in the workspace to authenticate the channel
    /// against.
    #[error("{0}: no root here; a pull verifies against the root the workspace already pins")]
    MissingRoot(PathBuf),

    /// The channel verified but did not yield a metadata file the pull has to
    /// write.
    #[error("the channel verified without producing {0}")]
    IncompleteRefresh(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
