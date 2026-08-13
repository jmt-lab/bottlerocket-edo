use clap::Parser;
use cmd::{Checkout, Fetch, List, Prune, Run, Update};
use std::{path::PathBuf, str::FromStr};

use crate::cmd::{Build, Publish};

mod cmd;

pub type Result<T> = std::result::Result<T, error::Error>;

pub mod error {
    use edo::context::Addr;
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("io error: {source}"))]
        Io { source: std::io::Error },
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
        #[snafu(transparent)]
        Environment {
            source: edo::environment::EnvironmentError,
        },
        #[snafu(transparent)]
        Oci { source: ocilot::error::Error },
        #[snafu(display("no kit located at address {addr}"))]
        NoKit { addr: Addr },
        #[snafu(display(
            "cannot publish artifact layer with media type {media_type}: only `Tar(_)` layers are supported"
        ))]
        UnsupportedPublishLayerMediaType { media_type: String },
        #[snafu(display(
            "cannot checkout artifact layer with media type {media_type} (digest {digest}): \
             layer has no path_hint recorded on the artifact manifest, so we cannot pick a \
             filename for it. This is a producer bug — the transform that emitted this layer \
             should record `config.path_hints[digest] = <filename>`."
        ))]
        CheckoutLayerMissingPathHint { media_type: String, digest: String },
        #[snafu(transparent)]
        Source { source: edo::source::SourceError },
        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },
        #[snafu(transparent)]
        Core { source: edo_core::error::Error },
        #[snafu(transparent)]
        Twoliter {
            source: twoliter::config::error::Error,
        },
        #[snafu(display("no variant located at address {addr}"))]
        NoVariant { addr: edo::context::Addr },
        #[snafu(display(
            "no variant-image artifact for {name}/{version} (arch={arch}) in storage; run `twoliter build variants/{name}` first"
        ))]
        MissingVariantArtifact {
            name: String,
            version: String,
            arch: String,
        },
        #[snafu(display("failed to publish AMI: {source}"))]
        PublishAmi {
            source: twoliter::variant::publish::Error,
        },
    }
}

#[derive(Parser, Debug, Clone)]
#[command(version, about = "Edo build tool", long_about = None)]
pub struct Args {
    #[arg(short, long, default_value = "false")]
    debug: bool,
    #[arg(short, long, default_value = "false")]
    trace: bool,
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[arg(short, long)]
    storage: Option<PathBuf>,
    /// Path to the JSONL build-event log; pass `none` to disable.
    /// Defaults to `<storage>/events.jsonl`.
    #[arg(long, default_value = "default")]
    event_log: String,
    /// Path to a `Twoliter.override.toml` overlay file. Repeatable; later
    /// files override earlier ones. Merged on top of any
    /// `Twoliter.override.toml` auto-discovered next to `Twoliter.toml`.
    #[arg(long = "override", value_name = "PATH")]
    overrides: Vec<PathBuf>,
    /// Inline vendor-registry override, `<name>=<registry>`. Repeatable.
    /// Equivalent to a `[vendor.<name>]` entry in an override file.
    #[arg(
        long = "vendor",
        value_name = "NAME=REGISTRY",
        value_parser = crate::cmd::util::parse_key_val::<String, String>,
    )]
    vendor_overrides: Vec<(String, String)>,
    #[clap(subcommand)]
    command: Commands,
}

impl Args {
    /// Resolve the user-supplied `--event-log` argument to an absolute
    /// path (or `None` if disabled).
    pub fn resolve_event_log(&self) -> Option<PathBuf> {
        if self.event_log.eq_ignore_ascii_case("none") {
            return None;
        }
        if self.event_log != "default" {
            return Some(PathBuf::from(&self.event_log));
        }
        // Default: <storage>/events.jsonl. When `--storage` is not set,
        // fall back to ./.edo/events.jsonl.
        let base = self
            .storage
            .clone()
            .unwrap_or_else(|| PathBuf::from(".edo"));
        Some(base.join("events.jsonl"))
    }
}

#[derive(Parser, Debug, Clone)]
enum Commands {
    Build(Build),
    Checkout(Checkout),
    Fetch(Fetch),
    Run(Run),
    Prune(Prune),
    Publish(Publish),
    Update(Update),
    List(List),
}

#[tokio::main]
#[snafu::report]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.clone().command {
        Commands::Build(cmd) => cmd.run(args.clone()).await?,
        Commands::Checkout(cmd) => cmd.run(args.clone()).await?,
        Commands::Fetch(cmd) => cmd.run(args.clone()).await?,
        Commands::Run(cmd) => cmd.run(args.clone()).await?,
        Commands::Prune(cmd) => cmd.run(args.clone()).await?,
        Commands::Update(cmd) => cmd.run(args.clone()).await?,
        Commands::List(cmd) => cmd.run(args.clone()).await?,
        Commands::Publish(cmd) => cmd.run(args.clone()).await?,
    }
    Ok(())
}
