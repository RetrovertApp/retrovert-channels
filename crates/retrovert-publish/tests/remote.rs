//! Pushing a channel to a release host: what lands, in what order, and what a
//! client sees when a run does not finish.
//!
//! The host is in memory, but the client is the real `sigstore-tuf` updater
//! reading the asset namespace exactly as it would read a base URL — so a
//! channel that resolves here resolves over HTTPS for the same reasons.

mod common;

use std::sync::Arc;

use common::{
    FakeHost, copy_dir, empty_channel, live_channel, manifest_bytes, metadata_tag, now,
    push_generation, read, refresh_pushed_channel, resolve_pushed as resolve, write_manifest,
};
use retrovert_publish::{Error, ReleaseHost, Workspace};
use retrovert_tuf::manifest;
use tempfile::TempDir;

#[test]
fn a_channel_is_brought_live_root_first_and_timestamp_last() {
    let (_dir, _workspace, host) = empty_channel();

    assert_eq!(
        host.log(),
        [
            "dev/channel-metadata/1.root.json",
            "dev/channel-metadata/root.json",
            "dev/channel-metadata/1.targets.json",
            "dev/channel-metadata/1.snapshot.json",
            "dev/channel-metadata/timestamp.json",
        ]
    );
}

#[test]
fn a_generation_lands_manifest_first_and_timestamp_last() {
    let (dir, workspace, empty) = empty_channel();
    // Forked so the log holds this generation's publish and nothing before it.
    let host = Arc::new(empty.fork());
    push_generation(
        &workspace,
        &host,
        &write_manifest(dir.path(), "rev-1"),
        None,
    )
    .unwrap();
    let generation = manifest::generation_id(&manifest_bytes("rev-1"));

    assert_eq!(
        host.log(),
        [
            format!("dev/v1/{}", manifest::TARGET_PATH),
            format!("dev/channel-metadata/{generation}.manifest.json"),
            "dev/channel-metadata/2.targets.json".to_string(),
            "dev/channel-metadata/2.snapshot.json".to_string(),
            "dev/channel-metadata/timestamp.json".to_string(),
        ],
        "everything the new timestamp pins must be published before it"
    );
}

#[test]
fn the_immutable_assets_of_a_generation_live_on_their_own_release() {
    let (_dir, _workspace, host) = live_channel("rev-1");

    assert_eq!(host.assets("dev/v1"), [manifest::TARGET_PATH]);
    assert!(
        host.assets(&metadata_tag())
            .contains(&"root.json".to_string()),
        "the channel serves the root a client bootstraps from"
    );
}

#[test]
fn a_client_resolves_the_pushed_channel_from_its_asset_namespace() {
    let (_dir, workspace, host) = live_channel("rev-1");
    assert_eq!(resolve(&host, &workspace, now()), manifest_bytes("rev-1"));
}

#[test]
fn a_channel_verifies_before_it_has_a_generation() {
    let (_dir, workspace, host) = empty_channel();

    let root = read(&workspace, "root.json");
    let updater = refresh_pushed_channel(&host, &metadata_tag(), &root, now()).unwrap();
    assert!(
        updater.trusted().targets().unwrap().targets.is_empty(),
        "the channel is live and empty until a generation is published"
    );
}

#[test]
fn a_push_stopped_at_any_point_leaves_the_prior_generation_resolvable() {
    let (dir, workspace, host) = live_channel("rev-1");

    // Learn the second generation's exact publish sequence on a branch of the
    // channel that goes nowhere else.
    let complete_dir = TempDir::new().unwrap();
    let complete = Workspace::new(complete_dir.path().join("channel"));
    copy_dir(workspace.path(), complete.path());
    let complete_host = Arc::new(host.fork());
    let sequence = push_generation(
        &complete,
        &complete_host,
        &write_manifest(dir.path(), "rev-2"),
        None,
    )
    .unwrap();

    for stop_after in 0..sequence.len() {
        let scratch = TempDir::new().unwrap();
        let stopped = Workspace::new(scratch.path().join("channel"));
        copy_dir(workspace.path(), stopped.path());
        let stopped_host = Arc::new(host.fork());

        let err = push_generation(
            &stopped,
            &stopped_host,
            &write_manifest(dir.path(), "rev-2"),
            Some(stop_after),
        )
        .unwrap_err();
        assert!(
            matches!(err, Error::PublishStopped { after } if after == stop_after),
            "stopping after {stop_after} must report itself"
        );
        assert_eq!(stopped_host.log().len(), stop_after);
        assert_eq!(
            resolve(&stopped_host, &stopped, now()),
            manifest_bytes("rev-1"),
            "after {stop_after} of {} assets",
            sequence.len()
        );
    }

    assert_eq!(
        resolve(&complete_host, &complete, now()),
        manifest_bytes("rev-2"),
        "the complete run does advance the channel"
    );
}

#[test]
fn a_stopped_push_is_superseded_by_the_next_complete_one() {
    let (dir, workspace, host) = live_channel("rev-1");

    push_generation(
        &workspace,
        &host,
        &write_manifest(dir.path(), "rev-2"),
        Some(2),
    )
    .unwrap_err();
    assert_eq!(
        resolve(&host, &workspace, now()),
        manifest_bytes("rev-1"),
        "the interrupted generation never became current"
    );

    push_generation(
        &workspace,
        &host,
        &write_manifest(dir.path(), "rev-3"),
        None,
    )
    .unwrap();
    assert_eq!(resolve(&host, &workspace, now()), manifest_bytes("rev-3"));
}

#[test]
fn an_asset_is_never_published_to_a_release_that_was_not_created() {
    let host = FakeHost::default();
    let err = host
        .put_asset("dev/channel-metadata", "timestamp.json", b"{}")
        .expect_err("a missing release must be reported, not created implicitly");
    assert!(matches!(err, Error::MissingRelease { .. }));
}
