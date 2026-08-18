//! `init` produces a channel that the real client library accepts.
//!
//! Verification goes through `sigstore-tuf` — the crate the updater will use —
//! rather than a hand-rolled checker, so publisher and consumer are held to the
//! same reading of the metadata from the first commit.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{metadata_dir, now, read, refresh_with_sigstore_tuf, seeded_keys, seeded_workspace};
use jiff::{Timestamp, ToSpan, tz::TimeZone};
use retrovert_publish::{Workspace, init};
use retrovert_tuf::{KeyPair, RoleName};
use sigstore_tuf::Metadata;
use tempfile::TempDir;

fn expected_expiry(span: jiff::Span) -> String {
    now()
        .to_zoned(TimeZone::UTC)
        .checked_add(span)
        .unwrap()
        .timestamp()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn signed_field(workspace: &Workspace, file: &str, field: &str) -> String {
    let value: serde_json::Value = serde_json::from_slice(&read(workspace, file)).unwrap();
    value["signed"][field].as_str().unwrap().to_string()
}

#[test]
fn emitted_repository_validates_with_the_client_library() {
    let (_dir, workspace) = seeded_workspace();

    let updater = refresh_with_sigstore_tuf(&workspace, now()).unwrap();
    let trusted = updater.trusted();

    assert_eq!(trusted.root().version, 1);
    assert!(trusted.root().consistent_snapshot);
    assert_eq!(trusted.timestamp().unwrap().version, 1);
    assert_eq!(trusted.snapshot().unwrap().version, 1);

    let targets = trusted.targets().unwrap();
    assert_eq!(targets.version, 1);
    assert!(targets.targets.is_empty(), "a fresh channel has no targets");
}

#[test]
fn the_cli_binary_produces_a_repository_that_validates() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));

    let output = Command::new(env!("CARGO_BIN_EXE_retrovert-publish"))
        .arg("init")
        .arg(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    refresh_with_sigstore_tuf(&workspace, Timestamp::now()).unwrap();
}

#[test]
fn every_role_uses_a_distinct_ed25519_key_at_threshold_one() {
    let (_dir, workspace) = seeded_workspace();
    let root = Metadata::<sigstore_tuf::Root>::from_slice(&read(&workspace, "root.json"))
        .unwrap()
        .signed;

    assert_eq!(root.keys.len(), 4);
    for key in root.keys.values() {
        assert_eq!(key.keytype, "ed25519");
        assert_eq!(key.scheme, "ed25519");
    }

    let mut key_ids = BTreeSet::new();
    for role in RoleName::ALL {
        let entry = root.role(role.as_str()).expect("role is authorized");
        assert_eq!(entry.threshold, 1);
        assert_eq!(entry.keyids.len(), 1);
        assert!(root.keys.contains_key(&entry.keyids[0]));
        assert!(
            key_ids.insert(entry.keyids[0].clone()),
            "{role} reuses a key"
        );
    }
}

#[test]
fn key_ids_agree_with_the_client_librarys_computation() {
    let (_dir, workspace) = seeded_workspace();
    let root = Metadata::<sigstore_tuf::Root>::from_slice(&read(&workspace, "root.json"))
        .unwrap()
        .signed;

    for (declared, key) in &root.keys {
        assert_eq!(&key.key_id().unwrap(), declared);
    }
}

#[test]
fn expiries_match_the_decided_policy() {
    let (_dir, workspace) = seeded_workspace();

    assert_eq!(
        signed_field(&workspace, "root.json", "expires"),
        expected_expiry(12.months())
    );
    assert_eq!(
        signed_field(&workspace, "1.targets.json", "expires"),
        expected_expiry(60.days())
    );
    assert_eq!(
        signed_field(&workspace, "1.snapshot.json", "expires"),
        expected_expiry(60.days())
    );
    assert_eq!(
        signed_field(&workspace, "timestamp.json", "expires"),
        expected_expiry(14.days())
    );
}

#[test]
fn expiry_is_enforced_by_the_client() {
    let (_dir, workspace) = seeded_workspace();
    let at = |span: jiff::Span| {
        now()
            .to_zoned(TimeZone::UTC)
            .checked_add(span)
            .unwrap()
            .timestamp()
    };

    refresh_with_sigstore_tuf(&workspace, at(13.days())).expect("inside the timestamp lifetime");
    refresh_with_sigstore_tuf(&workspace, at(15.days()))
        .expect_err("past the 14-day timestamp expiry");
}

#[test]
fn metadata_is_published_under_consistent_snapshot_names() {
    let (_dir, workspace) = seeded_workspace();
    let dir = metadata_dir(&workspace);
    let exists = |name: &str| dir.join(name).exists();

    // Root is reachable both by version and at the fixed bootstrap name.
    assert!(exists("1.root.json") && exists("root.json"));
    assert!(exists("1.snapshot.json") && exists("1.targets.json"));
    // Timestamp is the one mutable name: a client polls it without knowing a
    // version, so it must not be version-prefixed.
    assert!(exists("timestamp.json"));
    assert!(!exists("snapshot.json") && !exists("targets.json"));

    assert_eq!(
        read(&workspace, "root.json"),
        read(&workspace, "1.root.json")
    );
}

#[test]
fn init_is_reproducible_from_the_same_keys_and_clock() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let a = Workspace::new(first.path().join("channel"));
    let b = Workspace::new(second.path().join("channel"));

    let report = init(&a, &seeded_keys(), now(), false).unwrap();
    init(&b, &seeded_keys(), now(), false).unwrap();

    for name in [
        "root.json",
        "1.root.json",
        "1.targets.json",
        "1.snapshot.json",
        "timestamp.json",
    ] {
        assert_eq!(read(&a, name), read(&b, name), "{name} differs");
    }
    for path in &report.keys {
        let mirrored = mirror(path, a.path(), b.path());
        assert_eq!(
            std::fs::read(path).unwrap(),
            std::fs::read(&mirrored).unwrap()
        );
    }
}

fn mirror(path: &Path, from: &Path, to: &Path) -> PathBuf {
    to.join(path.strip_prefix(from).unwrap())
}

#[test]
fn private_keys_are_split_by_trust_boundary_and_reload() {
    let (_dir, workspace) = seeded_workspace();
    let store = workspace.keys();
    let keys = seeded_keys();

    assert!(
        store
            .key_path(RoleName::Root)
            .starts_with(store.offline_dir())
    );
    for role in [RoleName::Targets, RoleName::Snapshot, RoleName::Timestamp] {
        assert!(store.key_path(role).starts_with(store.online_dir()));
    }

    for role in RoleName::ALL {
        let pem = std::fs::read_to_string(store.key_path(role)).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(store.read(role).unwrap().public(), keys.get(role).public());
    }
}

#[test]
fn force_re_init_clears_the_previous_channels_published_files() {
    let (dir, workspace) = seeded_workspace();
    let manifest_path = dir.path().join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": 1,
            "source_revision": "rev-1",
            "published": "2026-08-15T12:00:00Z",
            "artifacts": [],
        }))
        .unwrap(),
    )
    .unwrap();
    retrovert_publish::publish(&workspace, &manifest_path, now()).unwrap();

    init(&workspace, &seeded_keys(), now(), true).unwrap();

    let names = |dir: PathBuf| -> BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        names(metadata_dir(&workspace)),
        BTreeSet::from(
            [
                "root.json",
                "1.root.json",
                "1.targets.json",
                "1.snapshot.json",
                "timestamp.json"
            ]
            .map(String::from)
        ),
        "metadata from the replaced channel must not survive"
    );
    assert_eq!(
        names(workspace.channel().targets_dir()),
        BTreeSet::new(),
        "targets from the replaced channel must not survive"
    );
    refresh_with_sigstore_tuf(&workspace, now()).expect("the forced channel still validates");
}

#[test]
fn a_non_empty_workspace_is_refused_unless_forced() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));
    init(&workspace, &seeded_keys(), now(), false).unwrap();

    init(&workspace, &seeded_keys(), now(), false).expect_err("must not clobber existing keys");
    init(&workspace, &seeded_keys(), now(), true).expect("--force re-initializes");
    refresh_with_sigstore_tuf(&workspace, now()).expect("the forced channel still validates");
}

#[cfg(unix)]
#[test]
fn private_key_files_are_readable_only_by_their_owner() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, workspace) = seeded_workspace();
    let store = workspace.keys();
    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(store.path().to_path_buf()), 0o700);
    assert_eq!(mode(store.online_dir()), 0o700);
    assert_eq!(mode(store.offline_dir()), 0o700);
    for role in RoleName::ALL {
        assert_eq!(mode(store.key_path(role)), 0o600, "{role} key is too open");
    }
}

#[cfg(unix)]
#[test]
fn re_initializing_over_a_symlinked_key_path_does_not_write_the_key_through_it() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, workspace) = seeded_workspace();
    let store = workspace.keys();
    let elsewhere = dir.path().join("elsewhere.pem");
    let key_path = store.key_path(RoleName::Root);

    std::fs::remove_file(&key_path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &key_path).unwrap();

    init(&workspace, &seeded_keys(), now(), true).unwrap();

    assert!(!elsewhere.exists(), "the key followed the symlink");
    assert!(
        !std::fs::symlink_metadata(&key_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn re_initializing_over_a_world_readable_key_file_tightens_it() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, workspace) = seeded_workspace();
    let key_path = workspace.keys().key_path(RoleName::Timestamp);
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    init(&workspace, &seeded_keys(), now(), true).unwrap();

    assert_eq!(
        std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_key_directory_is_refused() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));
    let online = workspace.keys().online_dir();

    std::fs::create_dir_all(online.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside"), &online).unwrap();
    std::fs::create_dir_all(dir.path().join("outside")).unwrap();

    init(&workspace, &seeded_keys(), now(), true)
        .expect_err("keys must not land outside the store");
}

/// Overwrite `file`'s signature with one made by `key`, leaving the declared
/// `keyid` — and therefore the payload's canonical bytes — untouched.
fn resign_with(workspace: &Workspace, file: &str, key: &KeyPair) {
    let mut value: serde_json::Value = serde_json::from_slice(&read(workspace, file)).unwrap();
    let canonical = retrovert_tuf::canonical::to_bytes(&value["signed"]).unwrap();
    value["signatures"][0]["sig"] = key.sign_hex(&canonical).into();
    write(workspace, file, &serde_json::to_vec_pretty(&value).unwrap());
}

fn write(workspace: &Workspace, file: &str, bytes: &[u8]) {
    std::fs::write(metadata_dir(workspace).join(file), bytes).unwrap();
}

#[test]
fn metadata_signed_by_an_unauthorized_key_is_rejected() {
    let foreign = KeyPair::from_seed(&[99u8; 32]);

    // Timestamp is nothing's pinned child, so only its signature can reject it:
    // this isolates signature verification from the hash and version pins.
    let (_dir, workspace) = seeded_workspace();
    resign_with(&workspace, "timestamp.json", &foreign);
    assert!(matches!(
        refresh_with_sigstore_tuf(&workspace, now()),
        Err(sigstore_tuf::Error::ThresholdNotMet { role, .. }) if role == "timestamp"
    ));

    // Root is the trust anchor and self-signs, so a forged root must not even
    // bootstrap.
    let (_dir, workspace) = seeded_workspace();
    resign_with(&workspace, "root.json", &foreign);
    assert!(matches!(
        refresh_with_sigstore_tuf(&workspace, now()),
        Err(sigstore_tuf::Error::ThresholdNotMet { role, .. }) if role == "root"
    ));
}

#[test]
fn a_target_smuggled_into_targets_metadata_is_rejected() {
    let (_dir, workspace) = seeded_workspace();

    let mut value: serde_json::Value =
        serde_json::from_slice(&read(&workspace, "1.targets.json")).unwrap();
    value["signed"]["targets"]["evil.bin"] = serde_json::json!({
        "length": 1,
        "hashes": { "sha256": "0".repeat(64) },
    });
    write(
        &workspace,
        "1.targets.json",
        &serde_json::to_vec_pretty(&value).unwrap(),
    );

    assert!(
        matches!(
            refresh_with_sigstore_tuf(&workspace, now()),
            Err(sigstore_tuf::Error::IntegrityMismatch(_))
        ),
        "snapshot pins targets by hash, so an added target must not resolve"
    );
}
