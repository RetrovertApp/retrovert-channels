//! The `retrovert-publish` command-line entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use jiff::Timestamp;
use retrovert_publish::{
    GitHubReleases, KeySet, Repo, Result, SignedRole, Workspace, init, publish, pull, remote,
    resign, verify,
};

#[derive(Debug, Parser)]
#[command(name = "retrovert-publish", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a channel and a fresh disposable test root in an empty directory.
    Init(InitArgs),

    /// Publish a release-set manifest as the channel's next generation.
    Publish(PublishArgs),

    /// Re-sign the channel's expiries with its online keys, naming the same
    /// generation.
    Resign(ResignArgs),

    /// Replace a workspace's channel metadata with what the live channel
    /// serves, verified against the root the workspace pins.
    Pull(PullArgs),

    /// Fetch and verify a live channel's current generation over HTTPS.
    Verify(VerifyArgs),
}

/// Where a workspace's channel is served from. Both flags or neither: without
/// them a command only touches the local workspace.
#[derive(Debug, Args)]
struct HostArgs {
    /// GitHub repository hosting the channel.
    #[arg(long, value_name = "OWNER/NAME", requires = "channel")]
    repo: Option<Repo>,

    /// Channel name the releases are tagged under, e.g. `dev`.
    #[arg(long, value_name = "NAME", requires = "repo")]
    channel: Option<String>,
}

impl HostArgs {
    /// The host and channel to publish to, if this run publishes at all.
    fn resolve(&self) -> Result<Option<(GitHubReleases, &str)>> {
        let (Some(repo), Some(channel)) = (&self.repo, &self.channel) else {
            return Ok(None);
        };
        Ok(Some((GitHubReleases::from_env(repo.clone())?, channel)))
    }
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Workspace to create: `repository/` is publishable, `keys/` is not.
    dir: PathBuf,

    /// Initialize even if the directory already has contents.
    #[arg(long)]
    force: bool,

    #[command(flatten)]
    host: HostArgs,
}

#[derive(Debug, Args)]
struct PublishArgs {
    /// Workspace holding the channel and its signing keys.
    dir: PathBuf,

    /// The release-set manifest to publish.
    manifest: PathBuf,

    #[command(flatten)]
    host: HostArgs,

    /// Stop after publishing this many assets, short of the commit point. Runs
    /// the channel's failed-publish drill against the real host.
    #[arg(long, value_name = "N")]
    stop_after: Option<usize>,
}

#[derive(Debug, Args)]
struct ResignArgs {
    /// Workspace holding the channel and its online signing keys. The root key
    /// takes no part in a re-sign and need not be present.
    dir: PathBuf,

    #[command(flatten)]
    host: HostArgs,
}

/// Which channel to read, for a command that only reads and so needs no
/// credential.
#[derive(Debug, Args)]
struct ChannelArgs {
    /// GitHub repository hosting the channel.
    #[arg(long, value_name = "OWNER/NAME")]
    repo: Repo,

    /// Channel name the releases are tagged under, e.g. `dev`.
    #[arg(long, value_name = "NAME")]
    channel: String,
}

#[derive(Debug, Args)]
struct PullArgs {
    /// Workspace whose channel metadata is replaced. Its `root.json` is what
    /// the pulled chain has to verify against.
    dir: PathBuf,

    #[command(flatten)]
    channel: ChannelArgs,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// The channel's base URL.
    base_url: String,

    /// The root metadata to trust, and nothing beyond it.
    root: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("retrovert-publish: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Init(args) => run_init(args),
        Command::Publish(args) => run_publish(args),
        Command::Resign(args) => run_resign(args),
        Command::Pull(args) => run_pull(args),
        Command::Verify(args) => run_verify(args),
    }
}

fn run_publish(args: &PublishArgs) -> Result<()> {
    let workspace = Workspace::new(&args.dir);
    let report = publish(&workspace, &args.manifest, Timestamp::now())?;

    println!("channel:  {}", workspace.channel().path().display());
    for path in &report.written {
        println!("  wrote   {}", path.display());
    }
    println!(
        "release:  v{} (revision {})",
        report.version, report.source_revision
    );
    println!("generation: {}", report.generation_id);

    if let Some((host, channel)) = args.host.resolve()? {
        let manifest_bytes = read(&args.manifest)?;
        let mut pushed = Vec::new();
        let outcome = remote::push_generation(
            &host,
            channel,
            &report,
            &manifest_bytes,
            args.stop_after,
            &mut pushed,
        );
        report_push(host.repo(), channel, &pushed, &outcome);
        outcome?;
    }
    Ok(())
}

fn run_resign(args: &ResignArgs) -> Result<()> {
    let workspace = Workspace::new(&args.dir);
    let report = resign(&workspace, Timestamp::now())?;

    println!("channel:  {}", workspace.channel().path().display());
    for path in &report.written {
        println!("  wrote   {}", path.display());
    }
    report_expiries(&report.roles);

    if let Some((host, channel)) = args.host.resolve()? {
        let mut pushed = Vec::new();
        let outcome = remote::push_metadata(&host, channel, &report.written, &mut pushed);
        report_push(host.repo(), channel, &pushed, &outcome);
        outcome?;
    }
    Ok(())
}

fn run_pull(args: &PullArgs) -> Result<()> {
    let workspace = Workspace::new(&args.dir);
    let base_url = remote::base_url(&args.channel.repo, &args.channel.channel);
    let report = pull(&workspace, &base_url, Timestamp::now())?;

    println!("channel:  {base_url}");
    for path in &report.written {
        println!("  wrote   {}", path.display());
    }
    report_expiries(&report.roles);
    Ok(())
}

fn run_init(args: &InitArgs) -> Result<()> {
    let workspace = Workspace::new(&args.dir);
    let keys = KeySet::generate()?;
    let report = init(&workspace, &keys, Timestamp::now(), args.force)?;

    println!("channel:  {}", workspace.channel().path().display());
    for path in &report.metadata {
        println!("  wrote   {}", path.display());
    }
    println!("root key id: {}", report.root_key_id);
    println!(
        "keys:     {} — do not publish; the root key belongs in offline storage",
        workspace.keys().path().display()
    );
    #[cfg(not(unix))]
    println!(
        "warning:  this platform has no owner-only enforcement; restrict that directory yourself"
    );

    if let Some((host, channel)) = args.host.resolve()? {
        let mut pushed = Vec::new();
        let outcome = remote::push_metadata(&host, channel, &report.metadata, &mut pushed);
        report_push(host.repo(), channel, &pushed, &outcome);
        outcome?;
    }
    Ok(())
}

fn run_verify(args: &VerifyArgs) -> Result<()> {
    let root = read(&args.root)?;
    let generation = verify::verify(&args.base_url, &root, Timestamp::now())?;

    println!("channel:  {}", args.base_url);
    println!("root:     {}", args.root.display());
    println!(
        "release:  v{} published {} (revision {})",
        generation.version, generation.published, generation.source_revision
    );
    println!("artifacts: {}", generation.artifacts);
    println!("generation: {}", generation.generation_id);
    Ok(())
}

/// Report when each online role lapses. A re-sign's whole purpose is to move
/// these, so a scheduled run's log has to show where they landed.
fn report_expiries(roles: &[SignedRole]) {
    for role in roles {
        println!(
            "{:<10} v{} expires {}",
            role.role, role.version, role.expires
        );
    }
}

/// Report what reached the host, including on the runs that ended early — a
/// partial publish is exactly when the operator needs to see how far it got.
fn report_push(repo: &Repo, channel: &str, pushed: &[String], outcome: &Result<()>) {
    let verdict = match outcome {
        Ok(()) => "published",
        Err(retrovert_publish::Error::PublishStopped { .. }) => "stopped short",
        Err(_) => "failed",
    };
    println!("host:     {repo} ({verdict})");
    for asset in pushed {
        println!("  pushed  {asset}");
    }
    println!("base url: {}", remote::base_url(repo, channel));
}

fn read(path: &PathBuf) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| retrovert_publish::Error::Io {
        path: path.clone(),
        source: e,
    })
}
