//! Reading a channel over HTTP, against a socket.
//!
//! What matters here is the boundary behaviour a channel depends on and that no
//! amount of in-memory faking can prove: a body of exactly its pinned length is
//! accepted, a longer one is refused before it is buffered, a missing name reads
//! as absent rather than as a failure, and the one name that gets replaced is
//! never fetched from a cache. The last test closes the loop and resolves a real
//! published channel end to end.

mod common;

use common::{TestServer, now, seeded_workspace, serve_channel, write_manifest};
use retrovert_publish::{HttpChannel, verify};
use retrovert_tuf::manifest;
use sigstore_tuf::transport::Repository;

fn fetch(
    channel: &HttpChannel,
    name: &str,
    max_length: u64,
) -> sigstore_tuf::Result<Option<Vec<u8>>> {
    pollster::block_on(channel.fetch_target(name, max_length))
}

fn fetch_metadata(
    channel: &HttpChannel,
    name: &str,
    max_length: u64,
) -> sigstore_tuf::Result<Option<Vec<u8>>> {
    pollster::block_on(channel.fetch_metadata(name, max_length))
}

#[test]
fn a_body_of_exactly_its_pinned_length_is_accepted() {
    let body = vec![b'x'; 157];
    let server = TestServer::canned(200, body.clone());
    let channel = HttpChannel::new(server.base_url());

    assert_eq!(fetch(&channel, "manifest.json", 157).unwrap(), Some(body));
}

#[test]
fn a_body_past_its_limit_is_refused() {
    let server = TestServer::canned(200, vec![b'x'; 158]);
    let channel = HttpChannel::new(server.base_url());

    let err = fetch(&channel, "manifest.json", 157).unwrap_err();
    assert!(
        matches!(err, sigstore_tuf::Error::Transport(_)),
        "an over-long body must fail as transport, not parse: {err}"
    );
}

#[test]
fn a_name_the_channel_does_not_serve_reads_as_absent() {
    let server = TestServer::canned(404, Vec::new());
    let channel = HttpChannel::new(server.base_url());

    assert_eq!(fetch(&channel, "9.root.json", 512_000).unwrap(), None);
}

#[test]
fn a_status_the_channel_cannot_answer_with_is_an_error() {
    let server = TestServer::canned(500, Vec::new());
    let channel = HttpChannel::new(server.base_url());

    let err = fetch(&channel, "timestamp.json", 16_384).unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
}

#[test]
fn the_entry_point_is_requested_under_a_url_no_cache_has_seen() {
    let server = TestServer::canned(200, b"{}".to_vec());
    let channel = HttpChannel::new(server.base_url());

    fetch_metadata(&channel, "timestamp.json", 16_384).unwrap();
    fetch_metadata(&channel, "timestamp.json", 16_384).unwrap();
    fetch_metadata(&channel, "3.snapshot.json", 16_384).unwrap();

    let seen = server.requests();
    assert!(seen[0].starts_with("GET /timestamp.json?t="), "{}", seen[0]);
    assert_ne!(seen[0], seen[1], "two polls must not share a cache entry");
    assert_eq!(seen[2], "GET /3.snapshot.json", "{}", seen[2]);
}

#[test]
fn a_published_channel_resolves_its_generation_over_http() {
    let (dir, workspace) = seeded_workspace();
    let manifest_path = write_manifest(dir.path(), "rev-1");
    let published = retrovert_publish::publish(&workspace, &manifest_path, now()).unwrap();

    let server = serve_channel(&workspace);

    let root = std::fs::read(workspace.channel().metadata_dir().join("root.json")).unwrap();
    let generation = verify::verify(server.base_url(), &root, now()).unwrap();

    assert_eq!(generation.generation_id, published.generation_id);
    assert_eq!(generation.version, 1);
    assert_eq!(generation.source_revision, "rev-1");
    assert_eq!(generation.published, "2026-08-15T12:00:00Z");
    assert_eq!(generation.artifacts, 1);
    assert_eq!(
        generation.generation_id,
        manifest::generation_id(&std::fs::read(&manifest_path).unwrap())
    );
}
