//! Helpers shared by the publisher's integration tests.

// Each test binary compiles this module separately and uses a different subset.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use jiff::Timestamp;
use retrovert_publish::{Error, InitReport, KeySet, ReleaseHost, Result, Workspace, init};
use retrovert_tuf::{KeyPair, RoleName};
use sigstore_tuf::transport::FetchFuture;
use sigstore_tuf::{Repository, Updater};
use tempfile::TempDir;

/// A fixed instant, so every expiry assertion has an exact expected value.
pub fn now() -> Timestamp {
    "2026-08-15T12:00:00Z".parse().unwrap()
}

/// Fixed keys, so publisher output is reproducible byte for byte.
pub fn seeded_keys() -> KeySet {
    KeySet {
        root: KeyPair::from_seed(&[1u8; 32]),
        targets: KeyPair::from_seed(&[2u8; 32]),
        snapshot: KeyPair::from_seed(&[3u8; 32]),
        timestamp: KeyPair::from_seed(&[4u8; 32]),
    }
}

pub fn seeded_workspace() -> (TempDir, Workspace) {
    let (dir, workspace, _) = seeded_channel();
    (dir, workspace)
}

/// A seeded workspace and what `init` wrote, in publication order.
pub fn seeded_channel() -> (TempDir, Workspace, InitReport) {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));
    let report = init(&workspace, &seeded_keys(), now(), false).unwrap();
    (dir, workspace, report)
}

pub fn metadata_dir(workspace: &Workspace) -> PathBuf {
    workspace.channel().metadata_dir()
}

pub fn read(workspace: &Workspace, name: &str) -> Vec<u8> {
    std::fs::read(metadata_dir(workspace).join(name)).unwrap()
}

/// Copy a workspace tree, so tests can replay publish steps on a clone.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Serves a channel's `metadata/` and `targets/` directories to `sigstore-tuf`
/// the way an HTTP mirror would expose its two base URLs, offline.
pub struct ChannelRepository {
    metadata: PathBuf,
    targets: PathBuf,
}

impl ChannelRepository {
    pub fn new(workspace: &Workspace) -> Self {
        let channel = workspace.channel();
        Self {
            metadata: channel.metadata_dir(),
            targets: channel.targets_dir(),
        }
    }
}

fn read_limited(path: &Path, max_length: u64) -> sigstore_tuf::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() as u64 > max_length => Err(sigstore_tuf::Error::Transport(
            format!("{} exceeds max length {max_length}", path.display()),
        )),
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(sigstore_tuf::Error::Transport(e.to_string())),
    }
}

impl Repository for ChannelRepository {
    fn fetch_metadata<'a>(&'a self, name: &'a str, max_length: u64) -> FetchFuture<'a> {
        let result = read_limited(&self.metadata.join(name), max_length);
        Box::pin(async move { result })
    }

    fn fetch_target<'a>(&'a self, path: &'a str, max_length: u64) -> FetchFuture<'a> {
        let result = read_limited(&self.targets.join(path), max_length);
        Box::pin(async move { result })
    }
}

/// Bootstrap `sigstore-tuf` from the channel's root and run its refresh
/// workflow, offline.
///
/// [`ChannelRepository`] is synchronous under an async trait, so the futures
/// are always ready and a trivial `block_on` is enough — no runtime needed.
pub fn refresh_with_sigstore_tuf(
    workspace: &Workspace,
    at: Timestamp,
) -> sigstore_tuf::Result<Updater> {
    let root = read(workspace, "root.json");
    let mut updater = Updater::new(ChannelRepository::new(workspace), &root)?;
    pollster::block_on(updater.refresh(at))?;
    Ok(updater)
}

/// A workspace holding only what the scheduled re-sign job is handed: the
/// online keys and the root it authenticates the channel against.
pub fn online_only_workspace(root: &[u8]) -> (TempDir, Workspace) {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("job"));

    let store = workspace.keys();
    store.create_dirs().unwrap();
    let keys = seeded_keys();
    for role in [RoleName::Targets, RoleName::Snapshot, RoleName::Timestamp] {
        store.write(role, keys.get(role)).unwrap();
    }
    std::fs::remove_dir_all(store.offline_dir()).unwrap();

    let channel = workspace.channel();
    channel.create_dirs().unwrap();
    channel.write_metadata("root.json", root).unwrap();
    (dir, workspace)
}

/// Serve named files out of `dirs`, first match wins — the flat asset namespace
/// a release exposes, where a file's directory is not part of its name.
///
/// Names in `overrides` are answered from the map instead of from disk, which
/// is how a test puts bytes on the wire that no key ever signed.
pub fn serve_dirs(dirs: Vec<PathBuf>, overrides: BTreeMap<String, Vec<u8>>) -> TestServer {
    TestServer::start(move |request| {
        let name = request
            .target
            .trim_start_matches('/')
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        if let Some(bytes) = overrides.get(&name) {
            return Response::new(200, bytes.clone());
        }
        for dir in &dirs {
            if let Ok(bytes) = std::fs::read(dir.join(&name)) {
                return Response::new(200, bytes);
            }
        }
        Response::new(404, Vec::new())
    })
}

/// Serve one workspace's channel, metadata and targets alike.
pub fn serve_channel(workspace: &Workspace) -> TestServer {
    let channel = workspace.channel();
    serve_dirs(
        vec![channel.metadata_dir(), channel.targets_dir()],
        BTreeMap::new(),
    )
}

/// A release-set manifest for `revision`, pretty-printed so the bytes a test
/// compares against are the bytes that were published.
pub fn manifest_bytes(revision: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&serde_json::json!({
        "schema": 1,
        "version": 1,
        "source_revision": revision,
        "published": "2026-08-15T12:00:00Z",
        "artifacts": [{
            "name": "app",
            "target": "linux-x86_64",
            "path": format!("app-{revision}-linux-x86_64.tar.zst"),
            "sha256": "ab".repeat(32),
            "size": 42,
            "revision": revision,
        }],
    }))
    .unwrap()
}

pub fn write_manifest(dir: &Path, revision: &str) -> PathBuf {
    let path = dir.join(format!("manifest-{revision}.json"));
    std::fs::write(&path, manifest_bytes(revision)).unwrap();
    path
}

/// A release host held in memory: releases, each a flat namespace of named
/// assets, plus the order everything was published in.
///
/// Whether a channel comes out resolvable is decided by that order and by
/// replacement semantics, neither of which needs a network to exercise.
#[derive(Default)]
pub struct FakeHost {
    releases: Mutex<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    log: Mutex<Vec<String>>,
}

impl FakeHost {
    /// Every asset published, in order, as `tag/name`.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The names currently on `tag`'s release.
    pub fn assets(&self, tag: &str) -> Vec<String> {
        self.releases
            .lock()
            .unwrap()
            .get(tag)
            .map(|assets| assets.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A copy of everything published so far, so a test can branch a channel
    /// and take it somewhere the original does not go.
    pub fn fork(&self) -> Self {
        Self {
            releases: Mutex::new(self.releases.lock().unwrap().clone()),
            log: Mutex::new(Vec::new()),
        }
    }

    fn get(&self, tag: &str, name: &str) -> Option<Vec<u8>> {
        self.releases.lock().unwrap().get(tag)?.get(name).cloned()
    }
}

impl ReleaseHost for FakeHost {
    fn ensure_release(&self, tag: &str, _prerelease: bool) -> Result<()> {
        self.releases
            .lock()
            .unwrap()
            .entry(tag.to_string())
            .or_default();
        Ok(())
    }

    fn put_asset(&self, tag: &str, name: &str, bytes: &[u8]) -> Result<()> {
        let mut releases = self.releases.lock().unwrap();
        let release = releases.get_mut(tag).ok_or_else(|| Error::MissingRelease {
            repo: "fake".to_string(),
            tag: tag.to_string(),
        })?;
        release.insert(name.to_string(), bytes.to_vec());
        self.log.lock().unwrap().push(format!("{tag}/{name}"));
        Ok(())
    }
}

/// One release of a [`FakeHost`], read the way a client reads a base URL: one
/// flat namespace serving metadata and targets alike.
pub struct HostedChannel {
    host: Arc<FakeHost>,
    tag: String,
}

impl HostedChannel {
    pub fn new(host: &Arc<FakeHost>, tag: &str) -> Self {
        Self {
            host: Arc::clone(host),
            tag: tag.to_string(),
        }
    }

    fn fetch(&self, name: &str, max_length: u64) -> sigstore_tuf::Result<Option<Vec<u8>>> {
        match self.host.get(&self.tag, name) {
            Some(bytes) if bytes.len() as u64 > max_length => Err(sigstore_tuf::Error::Transport(
                format!("{name} exceeds max length {max_length}"),
            )),
            found => Ok(found),
        }
    }
}

impl Repository for HostedChannel {
    fn fetch_metadata<'a>(&'a self, name: &'a str, max_length: u64) -> FetchFuture<'a> {
        let result = self.fetch(name, max_length);
        Box::pin(async move { result })
    }

    fn fetch_target<'a>(&'a self, path: &'a str, max_length: u64) -> FetchFuture<'a> {
        let result = self.fetch(path, max_length);
        Box::pin(async move { result })
    }
}

/// Bootstrap a client from `root` and refresh it against a pushed channel.
pub fn refresh_pushed_channel(
    host: &Arc<FakeHost>,
    tag: &str,
    root: &[u8],
    at: Timestamp,
) -> sigstore_tuf::Result<Updater> {
    let mut updater = Updater::new(HostedChannel::new(host, tag), root)?;
    pollster::block_on(updater.refresh(at))?;
    Ok(updater)
}

/// The channel name every test that involves a host publishes to.
pub const CHANNEL: &str = "dev";

pub fn metadata_tag() -> String {
    retrovert_publish::remote::metadata_tag(CHANNEL)
}

/// Bring a channel live on a fresh host, with no generation published yet.
pub fn empty_channel() -> (TempDir, Workspace, Arc<FakeHost>) {
    let (dir, workspace, report) = seeded_channel();
    let host = Arc::new(FakeHost::default());
    retrovert_publish::remote::push_metadata(&*host, CHANNEL, &report.metadata, &mut Vec::new())
        .unwrap();
    (dir, workspace, host)
}

/// A live channel whose current generation is `revision`.
pub fn live_channel(revision: &str) -> (TempDir, Workspace, Arc<FakeHost>) {
    let (dir, workspace, host) = empty_channel();
    push_generation(
        &workspace,
        &host,
        &write_manifest(dir.path(), revision),
        None,
    )
    .unwrap();
    (dir, workspace, host)
}

pub fn push_generation(
    workspace: &Workspace,
    host: &Arc<FakeHost>,
    manifest_path: &Path,
    stop_after: Option<usize>,
) -> Result<Vec<String>> {
    let report = retrovert_publish::publish(workspace, manifest_path, now())?;
    let bytes = std::fs::read(manifest_path).unwrap();
    let mut pushed = Vec::new();
    retrovert_publish::remote::push_generation(
        &**host,
        CHANNEL,
        &report,
        &bytes,
        stop_after,
        &mut pushed,
    )?;
    Ok(pushed)
}

/// Resolve the manifest a pushed channel currently names, as a client would.
pub fn resolve_pushed(host: &Arc<FakeHost>, workspace: &Workspace, at: Timestamp) -> Vec<u8> {
    let root = read(workspace, "root.json");
    let mut updater = refresh_pushed_channel(host, &metadata_tag(), &root, at).unwrap();
    fetch_manifest(&mut updater, at)
}

pub fn fetch_manifest(updater: &mut Updater, at: Timestamp) -> Vec<u8> {
    pollster::block_on(updater.get_target(retrovert_tuf::manifest::TARGET_PATH, at)).unwrap()
}

/// One request a [`TestServer`] received.
pub struct Request {
    pub method: String,
    /// Path and query, exactly as it went over the wire.
    pub target: String,
    pub body: Vec<u8>,
}

/// What a [`TestServer`] answers with.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, serde_json::to_vec(value).unwrap())
    }
}

/// An HTTP server on loopback, answering each request from a closure.
///
/// The behaviours this crate's HTTP code has to get right — what a request
/// actually looks like on the wire, in what order requests are made, and how a
/// response's length is handled — are only observable through a socket.
pub struct TestServer {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    port: u16,
}

impl TestServer {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let seen = Arc::clone(&requests);
        let stopping = Arc::clone(&shutdown);
        thread::spawn(move || {
            for connection in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(stream) = connection else { return };
                let Some(request) = read_request(&stream) else {
                    continue;
                };
                seen.lock()
                    .unwrap()
                    .push(format!("{} {}", request.method, request.target));
                write_response(stream, &handler(&request));
            }
        });

        Self {
            base: format!("http://{address}/"),
            requests,
            shutdown,
            port: address.port(),
        }
    }

    /// Serve nothing but a fixed response, for tests that only care about bodies.
    pub fn canned(status: u16, body: Vec<u8>) -> Self {
        Self::start(move |_| Response::new(status, body.clone()))
    }

    /// The base URL, with its trailing separator.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// The same base with the trailing separator stripped, for callers that
    /// join their own paths.
    pub fn origin(&self) -> String {
        self.base.trim_end_matches('/').to_string()
    }

    /// Every request seen, as `METHOD target`, oldest first.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The accept loop is parked inside `incoming()`; one connection wakes it
        // so it can observe the flag and leave rather than outlive the test.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut parts = start.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        body,
    })
}

fn write_response(mut stream: TcpStream, response: &Response) {
    // The reason phrase is a placeholder: the status line needs one to parse,
    // and nothing on either side of these tests reads it.
    let head = format!(
        "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        response.status,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}
