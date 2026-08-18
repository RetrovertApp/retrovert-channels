//! `resign` keeps a quiet channel verifiable.
//!
//! The scheduled job that runs it has one job and two constraints: the expiries
//! have to move, and what the channel names must not. It also runs somewhere
//! the root key is not, so the whole path has to work with the online keys
//! alone.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use common::{
    copy_dir, empty_channel, fetch_manifest, manifest_bytes, metadata_tag, now, push_generation,
    read, refresh_pushed_channel, refresh_with_sigstore_tuf, resolve_pushed, seeded_workspace,
    write_manifest,
};
use jiff::{Span, Timestamp, ToSpan, tz::TimeZone};
use retrovert_publish::{ResignReport, Workspace, publish, remote, resign};
use retrovert_tuf::RoleName;
use tempfile::TempDir;

fn after(base: Timestamp, span: Span) -> Timestamp {
    base.to_zoned(TimeZone::UTC)
        .checked_add(span)
        .unwrap()
        .timestamp()
}

fn expiry_of(base: Timestamp, span: Span) -> String {
    after(base, span).strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn file_names(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn roles(report: &ResignReport) -> Vec<(RoleName, u64, String)> {
    report
        .roles
        .iter()
        .map(|r| (r.role, r.version, r.expires.clone()))
        .collect()
}

/// A channel with one generation published, ready to be re-signed.
fn published_channel(revision: &str) -> (TempDir, Workspace) {
    let (dir, workspace) = seeded_workspace();
    publish(&workspace, &write_manifest(dir.path(), revision), now()).unwrap();
    (dir, workspace)
}

#[test]
fn a_resign_advances_every_online_role_to_a_full_lifetime() {
    let (_dir, workspace) = published_channel("rev-1");
    let when = after(now(), 30.days());

    let report = resign(&workspace, when).unwrap();

    assert_eq!(
        roles(&report),
        [
            (RoleName::Targets, 3, expiry_of(when, 60.days())),
            (RoleName::Snapshot, 3, expiry_of(when, 60.days())),
            (RoleName::Timestamp, 3, expiry_of(when, 14.days())),
        ],
        "every online role takes a fresh lifetime dated from the re-sign"
    );
    assert_eq!(
        file_names(&report.written),
        ["3.targets.json", "3.snapshot.json", "timestamp.json"],
        "the timestamp write is what makes the refreshed chain current"
    );
}

#[test]
fn a_client_resolves_the_same_generation_after_a_resign() {
    let (_dir, workspace) = published_channel("rev-1");
    let before = refresh_with_sigstore_tuf(&workspace, now()).unwrap();
    let targets_before = before.trusted().targets().unwrap().targets.clone();

    let when = after(now(), 30.days());
    resign(&workspace, when).unwrap();

    let mut updater = refresh_with_sigstore_tuf(&workspace, when).unwrap();
    assert_eq!(
        updater.trusted().targets().unwrap().targets,
        targets_before,
        "a re-sign changes when the channel expires, not what it names"
    );
    assert_eq!(
        fetch_manifest(&mut updater, when),
        manifest_bytes("rev-1"),
        "the generation a client resolves is untouched"
    );
}

#[test]
fn a_resign_rewrites_nothing_in_targets_but_its_version_and_expiry() {
    let (_dir, workspace) = published_channel("rev-1");
    let before: serde_json::Value =
        serde_json::from_slice(&read(&workspace, "2.targets.json")).unwrap();

    resign(&workspace, after(now(), 30.days())).unwrap();

    let after_resign: serde_json::Value =
        serde_json::from_slice(&read(&workspace, "3.targets.json")).unwrap();
    let (before, after_resign) = (&before["signed"], &after_resign["signed"]);

    let changed: BTreeSet<&str> = before
        .as_object()
        .unwrap()
        .keys()
        .chain(after_resign.as_object().unwrap().keys())
        .map(String::as_str)
        .filter(|key| before[key] != after_resign[key])
        .collect();
    assert_eq!(
        changed,
        BTreeSet::from(["version", "expires"]),
        "the role is carried through, not rebuilt — fields this build does not \
         know about are fields it must not drop"
    );
}

#[test]
fn a_channel_with_no_generation_yet_is_still_a_channel_to_keep_alive() {
    let (_dir, workspace) = seeded_workspace();
    let when = after(now(), 30.days());

    let report = resign(&workspace, when).unwrap();

    assert_eq!(
        report.roles.iter().map(|r| r.version).collect::<Vec<_>>(),
        [2, 2, 2]
    );
    let updater = refresh_with_sigstore_tuf(&workspace, when).unwrap();
    assert!(
        updater.trusted().targets().unwrap().targets.is_empty(),
        "an empty channel re-signs to an empty channel"
    );
}

/// The scheduled job assembles its workspace with `mkdir` and `cp` rather than
/// through these types, so nothing but this test stops the two drifting apart.
#[test]
fn the_layout_the_scheduled_job_assembles_by_hand_is_the_one_the_tools_read() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path());

    let relative = |path: &Path| -> Vec<String> {
        path.strip_prefix(dir.path())
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect()
    };

    // Written literally by .github/workflows/resign-dev.yml.
    for role in [RoleName::Targets, RoleName::Snapshot, RoleName::Timestamp] {
        assert_eq!(
            relative(&workspace.keys().key_path(role)),
            ["keys", "online", &format!("{role}.pem")]
        );
    }
    assert_eq!(
        relative(
            &workspace
                .channel()
                .metadata_dir()
                .join(RoleName::Root.file_name())
        ),
        ["repository", "metadata", "root.json"]
    );
}

#[test]
fn a_resign_recovers_a_channel_whose_metadata_has_already_lapsed() {
    let (_dir, workspace) = published_channel("rev-1");
    let lapsed = after(now(), 90.days());

    refresh_with_sigstore_tuf(&workspace, lapsed)
        .expect_err("90 days out, the 14-day timestamp is long gone");

    resign(&workspace, lapsed).unwrap();

    let mut updater = refresh_with_sigstore_tuf(&workspace, lapsed).unwrap();
    assert_eq!(
        fetch_manifest(&mut updater, lapsed),
        manifest_bytes("rev-1")
    );
}

#[test]
fn a_resign_never_reaches_for_the_root_key() {
    let (_dir, workspace) = published_channel("rev-1");
    let offline = workspace.keys().offline_dir();
    std::fs::remove_dir_all(&offline).unwrap();

    let when = after(now(), 30.days());
    resign(&workspace, when).unwrap();

    assert!(
        !offline.exists(),
        "the online keys are the whole of what a scheduled re-sign is given"
    );
    refresh_with_sigstore_tuf(&workspace, when).unwrap();
}

#[test]
fn a_resign_leaves_root_alone() {
    let (_dir, workspace) = published_channel("rev-1");
    let before = read(&workspace, "root.json");

    resign(&workspace, after(now(), 30.days())).unwrap();

    assert_eq!(
        read(&workspace, "root.json"),
        before,
        "root outlives the online roles by a year and is signed offline"
    );
}

#[test]
fn a_resign_interrupted_at_every_step_leaves_the_channel_resolvable() {
    let (_dir, workspace) = published_channel("rev-1");

    // Complete the re-sign in a clone to learn its exact write sequence.
    let complete_dir = TempDir::new().unwrap();
    let complete = Workspace::new(complete_dir.path().join("channel"));
    copy_dir(workspace.path(), complete.path());
    let report = resign(&complete, now()).unwrap();

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
        assert_eq!(
            fetch_manifest(&mut updater, now()),
            manifest_bytes("rev-1"),
            "after {k} writes"
        );
    }
}

#[test]
fn resigned_metadata_reaches_the_host_timestamp_last() {
    let (dir, workspace, empty) = empty_channel();
    push_generation(
        &workspace,
        &empty,
        &write_manifest(dir.path(), "rev-1"),
        None,
    )
    .unwrap();

    // Forked so the log holds the re-sign's assets and nothing before them.
    let host = Arc::new(empty.fork());
    let when = after(now(), 30.days());
    let report = resign(&workspace, when).unwrap();
    let mut pushed = Vec::new();
    remote::push_metadata(&*host, "dev", &report.written, &mut pushed).unwrap();

    assert_eq!(
        host.log(),
        [
            "dev/channel-metadata/3.targets.json",
            "dev/channel-metadata/3.snapshot.json",
            "dev/channel-metadata/timestamp.json",
        ],
        "no generation assets move; only the metadata that dates them"
    );
    assert_eq!(
        resolve_pushed(&host, &workspace, when),
        manifest_bytes("rev-1"),
        "the live channel still names the generation it did before"
    );
}

#[test]
fn a_resign_pushed_onto_a_live_channel_supersedes_the_expiring_metadata() {
    let (dir, workspace, host) = empty_channel();
    push_generation(
        &workspace,
        &host,
        &write_manifest(dir.path(), "rev-1"),
        None,
    )
    .unwrap();

    let when = after(now(), 30.days());
    let report = resign(&workspace, when).unwrap();
    remote::push_metadata(&*host, "dev", &report.written, &mut Vec::new()).unwrap();

    let root = read(&workspace, "root.json");
    let updater = refresh_pushed_channel(&host, &metadata_tag(), &root, when).unwrap();
    assert_eq!(updater.trusted().timestamp().unwrap().version, 3);
    assert_eq!(
        updater.trusted().timestamp().unwrap().expires,
        expiry_of(when, 14.days())
    );
}

#[test]
fn the_cli_binary_resigns_a_channel_and_reports_where_its_expiries_land() {
    let dir = TempDir::new().unwrap();
    let workspace = Workspace::new(dir.path().join("channel"));
    let manifest_path = write_manifest(dir.path(), "rev-1");

    run(&["init".as_ref(), workspace.path().as_os_str()]);
    run(&[
        "publish".as_ref(),
        workspace.path().as_os_str(),
        manifest_path.as_os_str(),
    ]);
    let stdout = run(&["resign".as_ref(), workspace.path().as_os_str()]);

    for role in [RoleName::Targets, RoleName::Snapshot, RoleName::Timestamp] {
        assert!(
            stdout.contains(&format!("{role:<10} v3 expires ")),
            "{role} expiry missing from:\n{stdout}"
        );
    }

    let at = Timestamp::now();
    let mut updater = refresh_with_sigstore_tuf(&workspace, at).unwrap();
    assert_eq!(fetch_manifest(&mut updater, at), manifest_bytes("rev-1"));
}

fn run(args: &[&std::ffi::OsStr]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_retrovert-publish"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
