//! Publishing logic for AMIs, kits and ssm parameters
//!
//! twoliter 0.x: tools/pubsys*

mod amispec;
mod aws;
mod config;
mod frompath;
mod kit;
mod repo;
mod vmware;

use bytes::Bytes;
use clap::Parser;
use futures::{Stream, TryStreamExt};
use semver::Version;
use snafu::ResultExt;
use std::path::PathBuf;

/// Automates publishing of Bottlerocket updates
#[derive(Debug, Parser)]
pub struct Args {
    #[arg(long)]
    /// Path to Infra.toml (NOTE: must be specified before subcommand)
    infra_config_path: PathBuf,

    #[command(subcommand)]
    subcommand: SubCommands,
}

#[derive(Debug, Parser)]
enum SubCommands {
    Repo(Box<repo::RepoArgs>),
    ValidateRepo(Box<repo::validate_repo::ValidateRepoArgs>),
    CheckRepoExpirations(Box<repo::check_expirations::CheckExpirationsArgs>),
    RefreshRepo(Box<repo::refresh_repo::RefreshRepoArgs>),
    FetchVariant(Box<repo::fetch_variant::FetchVariantArgs>),
    Ami(Box<aws::ami::AmiArgs>),
    PublishAmi(Box<aws::publish_ami::Who>),
    ValidateAmi(Box<aws::validate_ami::ValidateAmiArgs>),

    Ssm(Box<aws::ssm::SsmArgs>),
    PromoteSsm(Box<aws::promote_ssm::PromoteArgs>),
    ValidateSsm(Box<aws::validate_ssm::ValidateSsmArgs>),

    UploadOva(Box<vmware::upload_ova::UploadArgs>),

    PublishKit(Box<kit::publish_kit::PublishKitArgs>),
}

pub async fn run(cli_args: &crate::Args, args: &Args) -> Result<()> {
    match args.subcommand {
        SubCommands::Repo(ref repo_args) => {
            repo::run(&args, repo_args).await.context(error::RepoSnafu)
        }
        SubCommands::ValidateRepo(ref validate_repo_args) => {
            repo::validate_repo::run(&args, validate_repo_args)
                .await
                .context(error::ValidateRepoSnafu)
        }
        SubCommands::CheckRepoExpirations(ref check_expirations_args) => {
            repo::check_expirations::run(&args, check_expirations_args)
                .await
                .context(error::CheckExpirationsSnafu)
        }
        SubCommands::RefreshRepo(ref refresh_repo_args) => {
            repo::refresh_repo::run(&args, refresh_repo_args)
                .await
                .context(error::RefreshRepoSnafu)
        }
        SubCommands::FetchVariant(ref fetch_variant_args) => {
            repo::fetch_variant::run(&args, fetch_variant_args)
                .await
                .context(error::FetchVariantSnafu)
        }
        SubCommands::Ami(ref ami_args) => aws::ami::run(&args, ami_args)
            .await
            .context(error::AmiSnafu),
        SubCommands::PublishAmi(ref publish_args) => aws::publish_ami::run(&args, publish_args)
            .await
            .context(error::PublishAmiSnafu),
        SubCommands::Ssm(ref ssm_args) => aws::ssm::run(&args, ssm_args)
            .await
            .context(error::SsmSnafu),
        SubCommands::PromoteSsm(ref promote_args) => aws::promote_ssm::run(&args, promote_args)
            .await
            .context(error::PromoteSsmSnafu),
        SubCommands::ValidateSsm(ref validate_ssm_args) => {
            aws::validate_ssm::run(&args, validate_ssm_args)
                .await
                .context(error::ValidateSsmSnafu)
        }
        SubCommands::ValidateAmi(ref validate_ami_args) => {
            aws::validate_ami::run(&args, validate_ami_args)
                .await
                .context(error::ValidateAmiSnafu)
        }
        SubCommands::UploadOva(ref upload_args) => vmware::upload_ova::run(&args, upload_args)
            .await
            .context(error::UploadOvaSnafu),
        SubCommands::PublishKit(ref publish_kit_args) => {
            kit::publish_kit::run(cli_args, &args, publish_kit_args)
                .await
                .context(error::PublishKitSnafu)
        }
    }
}

type BytesResult = std::result::Result<Bytes, tough::error::Error>;

/// Parses a SemVer, stripping a leading 'v' if present
pub(crate) fn friendly_version(
    mut version_str: &str,
) -> std::result::Result<Version, semver::Error> {
    if version_str.starts_with('v') {
        version_str = &version_str[1..];
    };

    Version::parse(version_str)
}

pub(crate) async fn read_stream(
    stream: impl Stream<Item = BytesResult> + Send,
) -> std::result::Result<Vec<u8>, Box<tough::error::Error>> {
    stream
        .try_fold(Vec::new(), |mut acc, bytes| {
            acc.extend(bytes.as_ref());
            std::future::ready(Ok(acc))
        })
        .await
        .map_err(Box::new)
}

pub(crate) mod error {
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub(crate) enum Error {
        #[snafu(display("Failed to build AMI: {}", source))]
        Ami {
            source: crate::publish::aws::ami::Error,
        },

        #[snafu(display("Failed to fetch variant: {}", source))]
        FetchVariant {
            source: crate::publish::repo::fetch_variant::Error,
        },

        #[snafu(display(
            "Error during publish-ami command: {}: {}",
            publish_ami_message(source),
            source
        ))]
        PublishAmi {
            source: crate::publish::aws::publish_ami::Error,
        },

        #[snafu(display("Failed to promote SSM: {}", source))]
        PromoteSsm {
            source: crate::publish::aws::promote_ssm::Error,
        },

        #[snafu(display("Failed to build repo: {}", source))]
        Repo { source: crate::publish::repo::Error },

        #[snafu(display("Failed to validate repository: {}", source))]
        ValidateRepo {
            source: crate::publish::repo::validate_repo::Error,
        },

        #[snafu(display("Check expirations error: {}", source))]
        CheckExpirations {
            source: crate::publish::repo::check_expirations::Error,
        },

        #[snafu(display("Failed to refresh repository metadata: {}", source))]
        RefreshRepo {
            source: crate::publish::repo::refresh_repo::Error,
        },

        #[snafu(display("Failed to update SSM: {}", source))]
        Ssm {
            source: crate::publish::aws::ssm::Error,
        },

        #[snafu(display("Failed to upload OVA: {}", source))]
        UploadOva {
            source: crate::publish::vmware::upload_ova::Error,
        },

        #[snafu(display("Failed to validate SSM parameters: {}", source))]
        ValidateSsm {
            source: crate::publish::aws::validate_ssm::Error,
        },

        #[snafu(display("Failed to validate EC2 images: {}", source))]
        ValidateAmi {
            source: crate::publish::aws::validate_ami::Error,
        },

        #[snafu(display("Failed to publish kit: {}", source))]
        PublishKit {
            source: crate::publish::kit::publish_kit::Error,
        },
    }

    fn publish_ami_message(error: &crate::publish::aws::publish_ami::Error) -> String {
        match error.amis_affected() {
            0 => String::from("No AMI permissions were updated"),
            1 => String::from("Permissions for 1 AMI were updated, the rest failed"),
            n => format!("Permissions for {n} AMIs were updated, the rest failed"),
        }
    }
}
type Result<T> = std::result::Result<T, error::Error>;
