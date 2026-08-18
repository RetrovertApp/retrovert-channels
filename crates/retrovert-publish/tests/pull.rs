//! `pull` is how a machine that holds only the online keys learns what the
//! channel currently says.
//!
//! What it must not do is take the host's word for it: the bytes it writes are
//! the bytes the next `resign` signs over, so a host that could substitute them
//! could put anything into metadata clients accept. Every test here is either
//! that round trip working or that substitution being refused.

mod common;

use std::collections::BTreeMap;

use common::{
    fetch_manifest, manifest_bytes, now, online_only_workspace, read, refresh_with_sigstore_tuf,
    seeded_workspace, serve_channel, serve_dirs, write_manifest,
};
use jiff::{Span, Timestamp, ToSpan, tz::TimeZone};
use retrovert_publish::{Error, Workspace, publish, pull, resign};
use retrovert_tuf::RoleName;
use tempfile::TempDir;

fn after(base: Timestamp, span: Span) -> Timestamp {
    base.to_zoned(TimeZone::UTC)
        .checked_add(span)
        .unwrap()
        .timestamp()
}

/// A published channel and a job workspace holding only its root and the online
/// keys — what the scheduled re-sign job starts from.
fn channel_and_job(revision: &str) -> (TempDir, Workspace, TempDir, Workspace) {
    let (dir, published) = seeded_workspace();
    publish(&published, &write_manifest(dir.path(), revision), now()).unwrap();
    let (job_dir, job) = online_only_workspace(&read(&published, "root.json"));
    (dir, published, job_dir, job)
}

fn metadata_names(workspace: &Workspace) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(workspace.channel().metadata_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_pulled_workspace_resigns_into_a_channel_a_client_accepts() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);
    let when = after(now(), 7.days());

    pull(&job, server.base_url(), when).unwrap();
    let report = resign(&job, when).unwrap();
    assert_eq!(
        report.roles[2].version, 3,
        "timestamp advances past the pull"
    );

    // The re-signed metadata is the job's; the generation's own assets never
    // left the release they were published to.
    let published_channel = published.channel();
    let live = serve_dirs(
        vec![
            job.channel().metadata_dir(),
            published_channel.targets_dir(),
        ],
        BTreeMap::new(),
    );
    let generation = retrovert_publish::verify(live.base_url(), &read(&job, "root.json"), when)
        .expect("the re-signed channel verifies from the root alone");

    assert_eq!(generation.source_revision, "rev-1");
    assert_eq!(generation.version, 1);
}

#[test]
fn a_pull_takes_the_generation_the_channel_names_now() {
    let (dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);

    pull(&job, server.base_url(), now()).unwrap();
    publish(&published, &write_manifest(dir.path(), "rev-2"), now()).unwrap();
    pull(&job, server.base_url(), now()).unwrap();

    let updater = refresh_with_sigstore_tuf(&job, now()).unwrap();
    let targets = updater.trusted().targets().unwrap();
    assert_eq!(targets.version, 3, "the second publish's targets");

    // The manifest itself lives with the publisher, so resolve it against a
    // namespace that serves the job's metadata over the published targets.
    let published_channel = published.channel();
    let live = serve_dirs(
        vec![
            job.channel().metadata_dir(),
            published_channel.targets_dir(),
        ],
        BTreeMap::new(),
    );
    let generation =
        retrovert_publish::verify(live.base_url(), &read(&job, "root.json"), now()).unwrap();
    assert_eq!(generation.source_revision, "rev-2");
}

#[test]
fn a_pull_writes_nothing_when_the_channel_does_not_authenticate() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");

    // A host serving a timestamp whose signature no longer checks out is
    // indistinguishable, to everything but the root, from one that does.
    let mut tampered = read(&published, "timestamp.json");
    let flip = tampered
        .windows(5)
        .position(|w| w == b"\"sig\"")
        .expect("timestamp carries a signature")
        + 10;
    tampered[flip] ^= 0b0000_0001;
    let server = serve_dirs(
        vec![
            published.channel().metadata_dir(),
            published.channel().targets_dir(),
        ],
        BTreeMap::from([("timestamp.json".to_string(), tampered)]),
    );

    pull(&job, server.base_url(), now()).expect_err("a channel that fails to verify is not pulled");

    assert_eq!(
        metadata_names(&job),
        ["root.json"],
        "nothing a re-sign would sign over reaches the workspace"
    );
}

/// The job workspace outlives no run in CI, but it does within one: `pull`,
/// `resign`, and — if the publish is retried — `pull` again. By then the
/// workspace is a version ahead of the channel, and a pull that seeded its
/// trusted set from local state instead of the network would read that as the
/// channel rolling back and refuse.
#[test]
fn a_pull_reports_the_channel_not_what_the_workspace_last_signed() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);

    pull(&job, server.base_url(), now()).unwrap();
    resign(&job, now()).unwrap();

    pull(&job, server.base_url(), now()).expect("the channel is the authority, not the workspace");

    let updater = refresh_with_sigstore_tuf(&job, now()).unwrap();
    assert_eq!(
        updater.trusted().timestamp().unwrap().version,
        2,
        "the workspace is back on the version the channel actually serves"
    );
}

#[test]
fn the_cli_binary_refuses_to_pull_into_a_workspace_that_pins_no_root() {
    let dir = TempDir::new().unwrap();

    // Reaches no network: the root is read before the channel is contacted.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_retrovert-publish"))
        .arg("pull")
        .arg(dir.path())
        .args([
            "--repo",
            "RetrovertApp/playback_plugins",
            "--channel",
            "dev",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no root here"), "{stderr}");
}

#[test]
fn a_pull_refuses_a_channel_that_has_already_lapsed() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);

    let err = pull(&job, server.base_url(), after(now(), 90.days())).unwrap_err();

    assert!(
        matches!(err, Error::Client(sigstore_tuf::Error::Expired { .. })),
        "{err}"
    );
    assert_eq!(
        metadata_names(&job),
        ["root.json"],
        "a channel that no longer verifies is not one to re-sign unattended"
    );
}

#[test]
fn a_pull_needs_the_workspace_to_pin_a_root() {
    let (_dir, published) = seeded_workspace();
    let server = serve_channel(&published);
    let dir = TempDir::new().unwrap();
    let job = Workspace::new(dir.path().join("job"));

    let err = pull(&job, server.base_url(), now()).unwrap_err();

    assert!(
        matches!(&err, Error::MissingRoot(path)
            if path.ends_with(RoleName::Root.file_name())),
        "{err}"
    );
}

#[test]
fn a_pull_writes_the_chain_under_the_names_the_channel_publishes() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);

    let report = pull(&job, server.base_url(), now()).unwrap();

    assert_eq!(
        report
            .written
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["2.targets.json", "2.snapshot.json", "timestamp.json"],
    );
    assert_eq!(
        metadata_names(&job),
        [
            "2.snapshot.json",
            "2.targets.json",
            "root.json",
            "timestamp.json"
        ],
        "the root is what a pull verifies against, not something it replaces"
    );
    for (role, bytes) in [
        (RoleName::Targets, read(&published, "2.targets.json")),
        (RoleName::Snapshot, read(&published, "2.snapshot.json")),
        (RoleName::Timestamp, read(&published, "timestamp.json")),
    ] {
        let name = match role {
            RoleName::Timestamp => "timestamp.json".to_string(),
            _ => format!("2.{}", role.file_name()),
        };
        assert_eq!(read(&job, &name), bytes, "{name} is byte-exact");
    }
}

#[test]
fn a_manifest_is_never_fetched_by_a_pull() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);

    pull(&job, server.base_url(), now()).unwrap();

    let manifest_requests = server
        .requests()
        .into_iter()
        .filter(|r| r.contains("manifest.json"))
        .count();
    assert_eq!(
        manifest_requests, 0,
        "re-signing needs the chain, not the release set it names"
    );
}

#[test]
fn a_pulled_generation_survives_the_resign_that_follows_it() {
    let (_dir, published, _job_dir, job) = channel_and_job("rev-1");
    let server = serve_channel(&published);
    let when = after(now(), 7.days());

    pull(&job, server.base_url(), when).unwrap();
    resign(&job, when).unwrap();

    let mut updater = refresh_with_sigstore_tuf(&job, when).unwrap();
    let published_channel = published.channel();
    std::fs::create_dir_all(job.channel().targets_dir()).unwrap();
    for entry in std::fs::read_dir(published_channel.targets_dir()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(
            entry.path(),
            job.channel().targets_dir().join(entry.file_name()),
        )
        .unwrap();
    }
    assert_eq!(fetch_manifest(&mut updater, when), manifest_bytes("rev-1"));
}
