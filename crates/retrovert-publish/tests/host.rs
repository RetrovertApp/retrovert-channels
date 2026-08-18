//! The GitHub release host, against a socket.
//!
//! What matters here is the call sequence, which is where the safety of a
//! publish lives: a name already in use must never be taken away before the
//! bytes replacing it are on the release, and a run interrupted mid-swap must
//! leave something the next run can clear. An in-memory host cannot show any of
//! that, because it is the REST choreography itself that is under test.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::{Request, Response, TestServer};
use retrovert_publish::{Error, GitHubReleases, ReleaseHost, Repo};

const TAG: &str = "dev/channel-metadata";
/// The tag as it appears in an API path once escaped.
const TAG_PATH: &str = "dev%2Fchannel-metadata";

/// Enough of the releases API to answer the calls this crate makes.
#[derive(Default)]
struct Releases {
    /// Release id by tag.
    by_tag: BTreeMap<String, u64>,
    /// Asset id → (release id, name, bytes).
    assets: BTreeMap<u64, (u64, String, Vec<u8>)>,
    next_id: u64,
}

impl Releases {
    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn release_json(&self, id: u64) -> serde_json::Value {
        let assets: Vec<_> = self
            .assets
            .iter()
            .filter(|(_, (release, _, _))| *release == id)
            .map(|(asset, (_, name, _))| serde_json::json!({ "id": asset, "name": name }))
            .collect();
        serde_json::json!({ "id": id, "assets": assets })
    }

    /// The names on `tag`'s release, sorted.
    fn names(&self, tag: &str) -> Vec<String> {
        let Some(id) = self.by_tag.get(tag) else {
            return Vec::new();
        };
        let mut names: Vec<_> = self
            .assets
            .values()
            .filter(|(release, _, _)| release == id)
            .map(|(_, name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    fn bytes(&self, tag: &str, name: &str) -> Option<Vec<u8>> {
        let id = self.by_tag.get(tag)?;
        self.assets
            .values()
            .find(|(release, asset, _)| release == id && asset == name)
            .map(|(_, _, bytes)| bytes.clone())
    }
}

/// A shared release store plus whatever a test wants to go wrong.
#[derive(Default)]
struct Host {
    state: Mutex<Releases>,
    /// Answer the next release creation with this status instead of creating.
    refuse_create: Mutex<Option<u16>>,
    /// What the release held after each answered request.
    snapshots: Mutex<Vec<Vec<String>>>,
}

impl Host {
    /// What the release held at each point a request was answered — the record
    /// that shows whether a name was ever missing.
    fn timeline(&self) -> Vec<Vec<String>> {
        self.snapshots.lock().unwrap().clone()
    }

    /// Where the timeline stands now, so a test can read only what follows.
    fn mark(&self) -> usize {
        self.snapshots.lock().unwrap().len()
    }

    fn answer(&self, request: &Request) -> Response {
        let (path, query) = match request.target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (request.target.as_str(), ""),
        };
        let response = self.route(&request.method, path, query, &request.body);
        self.snapshots
            .lock()
            .unwrap()
            .push(self.state.lock().unwrap().names(TAG));
        response
    }

    fn route(&self, method: &str, path: &str, query: &str, body: &[u8]) -> Response {
        let mut state = self.state.lock().unwrap();
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

        match (method, segments.as_slice()) {
            ("GET", ["repos", _, _, "releases", "tags", tag]) => {
                // The tag reaches the API escaped, because a release tag has a
                // `/` in it and the path would otherwise split around it.
                match state.by_tag.get(&percent_decode(tag)).copied() {
                    Some(id) => Response::json(200, &state.release_json(id)),
                    None => Response::json(404, &serde_json::json!({ "message": "Not Found" })),
                }
            }

            ("POST", ["repos", _, _, "releases"]) => {
                if let Some(status) = self.refuse_create.lock().unwrap().take() {
                    // A lost create race: the release exists by the time the
                    // refusal comes back.
                    let id = state.take_id();
                    state.by_tag.insert(TAG.to_string(), id);
                    return Response::json(
                        status,
                        &serde_json::json!({ "message": "already_exists" }),
                    );
                }
                let tag = serde_json::from_slice::<serde_json::Value>(body).unwrap()["tag_name"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let id = state.take_id();
                state.by_tag.insert(tag, id);
                Response::json(201, &state.release_json(id))
            }

            ("POST", ["repos", _, _, "releases", release, "assets"]) => {
                let release: u64 = release.parse().unwrap();
                let name = query.strip_prefix("name=").unwrap();
                let name = percent_decode(name);
                if state
                    .assets
                    .values()
                    .any(|(on, asset, _)| *on == release && *asset == name)
                {
                    return Response::json(
                        422,
                        &serde_json::json!({ "message": "already_exists" }),
                    );
                }
                let id = state.take_id();
                state.assets.insert(id, (release, name, body.to_vec()));
                Response::json(201, &serde_json::json!({ "id": id }))
            }

            ("DELETE", ["repos", _, _, "releases", "assets", asset]) => {
                let asset: u64 = asset.parse().unwrap();
                match state.assets.remove(&asset) {
                    Some(_) => Response::new(204, Vec::new()),
                    None => Response::json(404, &serde_json::json!({ "message": "Not Found" })),
                }
            }

            ("PATCH", ["repos", _, _, "releases", "assets", asset]) => {
                let asset: u64 = asset.parse().unwrap();
                let name = serde_json::from_slice::<serde_json::Value>(body).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_string();
                match state.assets.get_mut(&asset) {
                    Some(entry) => {
                        entry.1 = name;
                        Response::json(200, &serde_json::json!({ "id": asset }))
                    }
                    None => Response::json(404, &serde_json::json!({ "message": "Not Found" })),
                }
            }

            _ => Response::json(404, &serde_json::json!({ "message": "no route" })),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex: String = [bytes.next(), bytes.next()]
                .into_iter()
                .flatten()
                .map(char::from)
                .collect();
            out.push(u8::from_str_radix(&hex, 16).unwrap() as char);
        } else {
            out.push(byte as char);
        }
    }
    out
}

/// Reach `server` as if it were github.com, API and uploads alike.
fn releases_on(server: &TestServer) -> GitHubReleases {
    GitHubReleases::with_endpoints(
        "RetrovertApp/playback_plugins".parse::<Repo>().unwrap(),
        "test-token",
        server.origin(),
        server.origin(),
    )
}

fn host() -> (Arc<Host>, TestServer, GitHubReleases) {
    let state = Arc::new(Host::default());
    let answering = Arc::clone(&state);
    let server = TestServer::start(move |request| answering.answer(request));
    let releases = releases_on(&server);
    (state, server, releases)
}

#[test]
fn a_missing_release_is_created_and_an_existing_one_is_left_alone() {
    let (state, server, host) = host();

    host.ensure_release(TAG, true).unwrap();
    host.ensure_release(TAG, true).unwrap();

    assert_eq!(
        server.requests(),
        [
            format!("GET /repos/RetrovertApp/playback_plugins/releases/tags/{TAG_PATH}"),
            "POST /repos/RetrovertApp/playback_plugins/releases".to_string(),
            format!("GET /repos/RetrovertApp/playback_plugins/releases/tags/{TAG_PATH}"),
        ],
        "the second call must find the release, not make another"
    );
    assert_eq!(state.state.lock().unwrap().by_tag.len(), 1);
}

#[test]
fn a_release_created_by_someone_else_first_is_not_a_failure() {
    let (state, _server, host) = host();
    *state.refuse_create.lock().unwrap() = Some(422);

    host.ensure_release(TAG, true)
        .expect("losing the create race still leaves the release this call wanted");
}

#[test]
fn a_name_not_yet_in_use_is_uploaded_directly() {
    let (state, server, host) = host();
    host.ensure_release(TAG, true).unwrap();

    host.put_asset(TAG, "timestamp.json", b"first").unwrap();

    assert_eq!(
        state.state.lock().unwrap().bytes(TAG, "timestamp.json"),
        Some(b"first".to_vec())
    );
    assert!(
        !server.requests().iter().any(|r| r.starts_with("DELETE")),
        "nothing was there to delete: {:?}",
        server.requests()
    );
}

#[test]
fn a_replaced_name_never_stops_resolving_until_its_bytes_are_on_the_release() {
    let (state, server, host) = host();
    host.ensure_release(TAG, true).unwrap();
    host.put_asset(TAG, "timestamp.json", b"first").unwrap();

    let before_swap = state.mark();
    host.put_asset(TAG, "timestamp.json", b"second").unwrap();

    assert_eq!(
        state.state.lock().unwrap().bytes(TAG, "timestamp.json"),
        Some(b"second".to_vec())
    );
    assert_eq!(
        state.state.lock().unwrap().names(TAG),
        ["timestamp.json"],
        "the staging name must not survive the swap"
    );

    let swap: Vec<String> = server
        .requests()
        .into_iter()
        .skip_while(|r| !r.contains("timestamp.json.incoming"))
        .collect();
    assert!(
        swap[0].starts_with("POST") && swap[0].contains("assets?name=timestamp.json.incoming"),
        "the new bytes go up first: {swap:?}"
    );
    assert!(swap[1].starts_with("DELETE"), "then the old name: {swap:?}");
    assert!(swap[2].starts_with("PATCH"), "then the rename: {swap:?}");

    let timeline = state.timeline();
    let during_swap = &timeline[before_swap..];
    for names in during_swap {
        assert!(
            names.iter().any(|n| n.starts_with("timestamp.json")),
            "the replacement's bytes must be on the release throughout: {during_swap:?}"
        );
    }
    // GitHub cannot replace a name atomically, so the published name is
    // unresolvable across the rename — but across that one call only, and never
    // across an upload. Pinning the width here is what keeps it from growing.
    let unresolvable = during_swap
        .iter()
        .filter(|names| !names.iter().any(|n| n == "timestamp.json"))
        .count();
    assert_eq!(unresolvable, 1, "{during_swap:?}");
}

#[test]
fn a_staging_name_left_by_an_interrupted_swap_is_cleared_before_the_retry() {
    let (state, server, host) = host();
    host.ensure_release(TAG, true).unwrap();
    host.put_asset(TAG, "timestamp.json", b"first").unwrap();

    // What a process killed between the upload and the delete leaves behind.
    {
        let mut state = state.state.lock().unwrap();
        let release = state.by_tag[TAG];
        let id = state.take_id();
        state.assets.insert(
            id,
            (
                release,
                "timestamp.json.incoming".to_string(),
                b"abandoned".to_vec(),
            ),
        );
    }

    host.put_asset(TAG, "timestamp.json", b"second").unwrap();

    assert_eq!(
        state.state.lock().unwrap().bytes(TAG, "timestamp.json"),
        Some(b"second".to_vec()),
        "the retry must publish its own bytes, never adopt the abandoned ones"
    );
    assert_eq!(state.state.lock().unwrap().names(TAG), ["timestamp.json"]);
    assert!(
        server
            .requests()
            .iter()
            .filter(|r| r.starts_with("DELETE"))
            .count()
            == 2,
        "the abandoned asset and the superseded one both go: {:?}",
        server.requests()
    );
}

#[test]
fn publishing_to_a_release_that_is_not_there_is_reported() {
    let (_state, _server, host) = host();

    let err = host.put_asset(TAG, "timestamp.json", b"bytes").unwrap_err();
    assert!(matches!(err, Error::MissingRelease { .. }), "{err}");
}

#[test]
fn a_refused_request_carries_its_status_and_body() {
    let server = TestServer::start(|_| {
        Response::json(500, &serde_json::json!({ "message": "unavailable" }))
    });
    let host = releases_on(&server);

    let err = host.ensure_release(TAG, true).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("500"), "{rendered}");
    assert!(rendered.contains("unavailable"), "{rendered}");
}

#[test]
fn a_release_lookup_answered_with_something_else_is_not_read_as_empty() {
    let server = TestServer::canned(200, b"<html>not the api</html>".to_vec());
    let host = releases_on(&server);

    let err = host.ensure_release(TAG, true).unwrap_err();
    assert!(err.to_string().contains("not a release"), "{err}");
}
