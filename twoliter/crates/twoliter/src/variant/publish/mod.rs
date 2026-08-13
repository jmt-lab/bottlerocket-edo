//! `twoliter publish ami` — register/copy an AMI from a local
//! `variant-image` edo artifact, and grant/revoke launch permissions
//! on registered AMIs.
//!
//! Ported (adapted) from `old-twoliter/tools/pubsys/src/aws/`:
//! - `aws/client.rs`, `aws/mod.rs`, `aws/ami/{wait, snapshot,
//!   launch_permissions, public}.rs` land under `aws/` here.
//! - `aws/publish_ami/mod.rs` (grant/revoke plus shared helpers) is
//!   preserved as `aws/publish_ami.rs`.
//! - `aws/ami/{mod, register/{mod, mk_amispec, merge_toml}}.rs` land
//!   under `register/`, adapted for the new artifact-based input
//!   (variant-image tar layer instead of on-disk `os.img`/`data.img`).
//!
//! Naming: old-twoliter's `pubsys ami` subcommand mapped to the
//! register-and-copy flow, and `pubsys publish-ami` to the
//! grant/revoke flow. In this crate both live under
//! `twoliter publish ami`, as `register` and `permissions`
//! sub-subcommands.
pub mod aws;
pub mod register;

pub use register::Register;

use snafu::Snafu;

/// Result alias for the publish module's error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error surfaced to `cmd/publish.rs::PublishCommand::Ami`.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("failed to load Infra.toml: {source}"))]
    Config { source: pubsys_config::Error },

    #[snafu(display("failed to register ami: {source}"))]
    Register {
        #[snafu(source(from(register::Error, Box::new)))]
        source: Box<register::Error>,
    },

    #[snafu(display("failed to modify ami permissions: {source}"))]
    Permissions {
        #[snafu(source(from(aws::publish_ami::Error, Box::new)))]
        source: Box<aws::publish_ami::Error>,
    },
}
