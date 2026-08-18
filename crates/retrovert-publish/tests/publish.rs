//! `publish` commits a release-set manifest as the channel's sole target, and
//! the real client library resolves the latest generation.
//!
//! Verification goes through `sigstore-tuf` — the crate the updater will use —
//! so publisher and consumer are held to the same reading of the metadata.

mod common;

use std::process::Command;

use common::{
    copy_dir, manifest_bytes, now, refresh_with_sigstore_tuf, seeded_workspace, write_manifest,
};
use jiff::Timestamp;
use retrovert_publish::{Workspace, publish};
use retrovert_tuf::manifest;
use sigstore_tuf::Updater;
use tempfile::TempDir;

/// Resolve and download the channel's manifest the way a client would.
fn resolve_manifest(updater: &mut Updater, at: Timestamp) -> Vec<u8> {
    pollster::block_on(updater.get_target(manifest::TARGET_PATH, at)).unwrap()
}

#[test]
fn publishing_two_generations_resolves_the_latest() {
    let (dir, workspace) = seeded_workspace();
    let first = publish(&workspace, &write_manifest(dir.path(), "rev-1"), now()).unwrap();
    let second = publish(&workspace, &write_manifest(dir.path(), "rev-2"), now()).unwrap();
    assert_ne!(first.generation_id, second.generation_id);
    assert_eq!(first.version, 1);
    assert_eq!(first.source_revision, "rev-1");
    assert_eq!(second.source_revision, "rev-2");

    let mut updater = refresh_with_sigstore_tuf(&workspace, now()).unwrap();
    let targets = updater.trusted().targets().unwrap();
    assert_eq!(targets.targets.len(), 1, "the manifest is the sole target");

    let resolved = resolve_manifest(&mut updater, now());
    assert_eq!(resolved, manifest_bytes("rev-2"));
    assert_eq!(manifest::generation_id(&resolved), second.generation_id);
}

#[test]
fn a_publish_interrupted_at_every_step_leaves_the_previous_generation_resolvable() {
    let (dir, workspace) = seeded_workspace();
    publish(&workspace, &write_manifest(dir.path(), "rev-1"), now()).unwrap();

    // Complete the second publish in a clone to learn its exact write sequence.
    let complete_dir = TempDir::new().unwrap();
    let complete = Workspace::new(complete_dir.path().join("channel"));
    copy_dir(workspace.path(), complete.path());
    let report = publish(&complete, &write_manifest(dir.path(), "rev-2"), now()).unwrap();
    assert_eq!(
        report.written.last().unwrap().file_name().unwrap(),
        "timestamp.json",
        "the atomic timestamp write must be the commit point"
    );

    // Simulate a crash after each write: replay the first `k` writes onto a
    // fresh clone of the rev-1 channel, then resolve as a client.
    for k in 0..=report.written.len() {
        let scratch = TempDir::new().unwrap();
        let interrupted = Workspace::new(scratch.path().join("channel"));
        copy_dir(workspace.path(), interrupted.path());
        for path in &report.written[..k] {
            let relative = path.strip_prefix(complete.path()).unwrap();
            std::fs::copy(path, interrupted.path().join(relative)).unwrap();
        }

        let mut updater = refresh_with_sigstore_tuf(&interrupted, now())
            .unwrap_or_else(|e| panic!("refresh failed after {k} writes: {e}"));
        let expected = if k == report.written.len() {
            "rev-2"
        } else {
            "rev-1"
        };
        assert_eq!(
            resolve_manifest(&mut updater, now()),
            manifest_bytes(expected),
            "after {k} writes"
        );
    }
}

#[test]
fn republishing_an_identical_manifest_reproduces_the_generation_id() {
    let (dir, workspace) = seeded_workspace();
    let path = write_manifest(dir.path(), "rev-1");
    let first = publish(&workspace, &path, now()).unwrap();
    let second = publish(&workspace, &path, now()).unwrap();
    assert_eq!(first.generation_id, second.generation_id);

    let mut updater = refresh_with_sigstore_tuf(&workspace, now()).unwrap();
    assert_eq!(
        updater.trusted().timestamp().unwrap().version,
        3,
        "metadata versions still advance"
    );
    assert_eq!(
        resolve_manifest(&mut updater, now()),
        manifest_bytes("rev-1")
    );
}

#[test]
fn a_manifest_missing_source_revision_is_rejected_before_anything_is_written() {
    let (dir, workspace) = seeded_workspace();
    let path = dir.path().join("manifest.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": 1,
            "published": "2026-08-15T12:00:00Z",
            "artifacts": [],
        }))
        .unwrap(),
    )
    .unwrap();

    let err = publish(&workspace, &path, now()).unwrap_err();
    assert!(matches!(
        err,
        retrovert_publish::Error::Tuf(retrovert_tuf::Error::Manifest(m))
            if m.contains("source_revision")
    ));

    let updater = refresh_with_sigstore_tuf(&workspace, now()).unwrap();
    assert_eq!(
        updater.trusted().timestamp().unwrap().version,
        1,
        "the rejected publish must not advance the channel"
    );
}

#[test]
fn the_cli_binary_publishes_a_manifest_that_validates() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));
    let manifest_path = write_manifest(dir.path(), "rev-1");

    let run = |args: &[&std::ffi::OsStr]| {
        let output = Command::new(env!("CARGO_BIN_EXE_retrovert-publish"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    run(&["init".as_ref(), workspace.path().as_os_str()]);
    let output = run(&[
        "publish".as_ref(),
        workspace.path().as_os_str(),
        manifest_path.as_os_str(),
    ]);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&manifest::generation_id(&manifest_bytes("rev-1")))
    );

    let mut updater = refresh_with_sigstore_tuf(&workspace, Timestamp::now()).unwrap();
    assert_eq!(
        resolve_manifest(&mut updater, Timestamp::now()),
        manifest_bytes("rev-1")
    );
}
