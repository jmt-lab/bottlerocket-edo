//! Grant/revoke launch permissions on registered AMIs (and matching
//! `createVolumePermission` on their backing snapshots). Also
//! provides the shared helpers used by the register-and-copy
//! pipeline for granting copy-source access to target-region
//! accounts.
//!
//! Ported from `old-twoliter/tools/pubsys/src/aws/publish_ami/mod.rs`
//! with the systematic `log::` → `tracing::` swap and
//! `pubsys_config::InfraConfig::from_path_or_lock` →
//! `from_path_or_default`. The `Image` type (originally in
//! `aws/ami/mod.rs`) moved here because it is the on-disk JSON schema
//! consumed by grant/revoke.
use super::client::build_client_config;
use super::launch_permissions::{LaunchPermissionDef, get_launch_permissions};
use super::region_from_string;
use super::wait::{self, wait_for_ami};
use aws_sdk_ec2::error::ProvideErrorMetadata;
use aws_sdk_ec2::operation::{
    modify_image_attribute::{ModifyImageAttributeError, ModifyImageAttributeOutput},
    modify_snapshot_attribute::{ModifySnapshotAttributeError, ModifySnapshotAttributeOutput},
};
use aws_sdk_ec2::types::{
    ImageAttributeName, OperationType, PermissionGroup, SnapshotAttributeName,
};
use aws_sdk_ec2::{Client as Ec2Client, config::Region};
use clap::{Args as ClapArgs, Parser};
use error_utils::AwsSdkError;
use futures::TryFutureExt;
use futures::future::{join, ready};
use futures::stream::{self, StreamExt};
use pubsys_config::InfraConfig;
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, ensure};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::fs;

/// Regional-AMI entry emitted by the `register` sub-subcommand and
/// consumed by the `permissions` sub-subcommand. The JSON shape
/// (region-string keys mapping to an `Image`) matches old-twoliter's
/// `ami --ami-output` file so downstream tooling keeps working.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq, Hash)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub public: Option<bool>,
    pub launch_permissions: Option<Vec<LaunchPermissionDef>>,
}

impl Image {
    pub(crate) fn new(
        id: &str,
        name: &str,
        public: Option<bool>,
        launch_permissions: Option<Vec<LaunchPermissionDef>>,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            public,
            launch_permissions,
        }
    }
}

/// The four "who" flags for a grant/revoke call. Flattened into
/// [`Permissions`] so users pass them at the top level of the command.
#[derive(Clone, Debug, Parser)]
#[group(id = "who", required = true, multiple = true)]
pub struct ModifyOptions {
    /// User IDs to give/remove access.
    #[arg(long, value_delimiter = ',', group = "who")]
    pub user_ids: Vec<String>,
    /// Group names to give/remove access.
    #[arg(long, value_delimiter = ',', group = "who")]
    pub group_names: Vec<String>,
    /// Organization ARNs to give/remove access.
    #[arg(long, value_delimiter = ',', group = "who")]
    pub organization_arns: Vec<String>,
    /// Organizational-unit ARNs to give/remove access.
    #[arg(long, value_delimiter = ',', group = "who")]
    pub organizational_unit_arns: Vec<String>,
}

/// Grant or revoke launch permissions on the AMIs and their backing
/// snapshots described by `--ami-input`. `--grant` and `--revoke` are
/// mutually exclusive and one is required (`group = "mode"`).
#[derive(Clone, Debug, ClapArgs)]
#[group(id = "mode", required = true, multiple = false)]
pub struct Permissions {
    /// Path to the JSON file mapping regions → registered AMI info
    /// (as emitted by `twoliter publish ami register --ami-output`).
    #[arg(long)]
    pub ami_input: PathBuf,

    /// Comma-separated list of regions to modify, overriding
    /// Infra.toml. Must be a subset of the regions in `--ami-input`.
    #[arg(long, value_delimiter = ',')]
    pub regions: Vec<String>,

    /// Grant access to the given users/groups.
    #[arg(long, group = "mode")]
    pub grant: bool,

    /// Revoke access from the given users/groups.
    #[arg(long, group = "mode")]
    pub revoke: bool,

    #[command(flatten)]
    pub modify_opts: ModifyOptions,
}

impl Permissions {
    pub async fn run(&self, infra_config_path: PathBuf) -> Result<()> {
        run(self, infra_config_path).await
    }
}

/// The sub-subcommand entrypoint. `infra_config_path` is resolved by
/// the caller (see `variant/publish/mod.rs`), which lets the register
/// pipeline share the same helpers without going through the CLI.
async fn run(publish_args: &Permissions, infra_config_path: PathBuf) -> Result<()> {
    let (operation, description) = if publish_args.grant {
        (OperationType::Add, "granting access")
    } else if publish_args.revoke {
        (OperationType::Remove, "revoking access")
    } else {
        // Clap's `group = "mode"` guarantees one of `grant`/`revoke`
        // was set. If both are false we've violated that invariant.
        unreachable!("clap invariant: --grant xor --revoke required");
    };

    tracing::info!(
        "Using AMI data from path: {}",
        publish_args.ami_input.display()
    );

    let ami_input_bytes =
        fs::read(&publish_args.ami_input)
            .await
            .context(error::FileSnafu {
                op: "open",
                path: &publish_args.ami_input,
            })?;

    let mut ami_input: HashMap<String, Image> =
        serde_json::from_slice(&ami_input_bytes).context(error::DeserializeSnafu {
            path: &publish_args.ami_input,
        })?;
    tracing::trace!("Parsed AMI input: {ami_input:?}");

    // `register --ami-output` never creates an empty file; an empty
    // file is only reachable via manual editing and always indicates
    // an operator mistake.
    ensure!(
        !ami_input.is_empty(),
        error::InputSnafu {
            path: &publish_args.ami_input
        }
    );

    let infra_config =
        InfraConfig::from_path_or_default(&infra_config_path).context(error::ConfigSnafu)?;
    tracing::trace!("Using infra config: {infra_config:?}");

    let aws = infra_config.aws.unwrap_or_default();

    // Prefer CLI regions if given; otherwise fall back to Infra.toml.
    let regions = if !publish_args.regions.is_empty() {
        publish_args.regions.clone()
    } else {
        aws.regions.iter().cloned().collect()
    };
    ensure!(
        !regions.is_empty(),
        error::MissingConfigSnafu {
            missing: "aws.regions"
        }
    );
    let base_region = region_from_string(&regions[0]);

    // The requested-regions set must be a subset of the ones in
    // `--ami-input`; otherwise we'd silently no-op in an unknown region.
    let requested_regions: HashSet<&String> = regions.iter().collect();
    let known_regions: HashSet<&String> = ami_input.keys().collect();
    ensure!(
        requested_regions.is_subset(&known_regions),
        error::UnknownRegionsSnafu {
            regions: requested_regions
                .difference(&known_regions)
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        }
    );

    let mut amis = HashMap::with_capacity(regions.len());
    for name in regions {
        let image = ami_input.remove(&name).with_context(|| {
            error::UnknownRegionsSnafu {
                regions: vec![name.clone()],
            }
        })?;
        let region = region_from_string(&name);
        amis.insert(region, image);
    }

    // Build regional clients up-front — they're used inside futures
    // and need to outlive them.
    let mut ec2_clients = HashMap::with_capacity(amis.len());
    for region in amis.keys() {
        let client_config = build_client_config(region, &base_region, &aws).await;
        let ec2_client = Ec2Client::new(&client_config);
        ec2_clients.insert(region.clone(), ec2_client);
    }

    // Wait for every AMI to be `available` before touching any of
    // them: a not-yet-registered AMI won't report block-device
    // mappings, and we need those to compute the snapshot IDs.
    if amis.len() == 1 {
        tracing::info!("Waiting for AMI to be available before changing its permissions");
    } else {
        tracing::info!(
            "Waiting for all {} AMIs to be available before changing any of their permissions",
            amis.len(),
        );
    }
    let mut wait_requests = Vec::with_capacity(amis.len());
    for (region, image) in &amis {
        let wait_future = wait_for_ami(&image.id, region, &base_region, "available", 1, &aws);
        let info_future = ready((region.clone(), image.id.clone()));
        wait_requests.push(join(info_future, wait_future));
    }
    let request_stream = stream::iter(wait_requests).buffer_unordered(4);
    let wait_responses: Vec<((Region, String), std::result::Result<(), wait::Error>)> =
        request_stream.collect().await;

    for ((region, image_id), wait_response) in wait_responses {
        wait_response.context(error::WaitAmiSnafu {
            id: &image_id,
            region: region.as_ref(),
        })?;
    }

    let snapshots = get_regional_snapshots(&amis, &ec2_clients).await?;
    tracing::trace!("Found snapshots: {snapshots:?}");

    tracing::info!(
        "Updating all snapshot permissions before changing any AMI permissions - {description}"
    );
    modify_regional_snapshots(
        &publish_args.modify_opts,
        &operation,
        &snapshots,
        &ec2_clients,
    )
    .await?;

    tracing::info!("Updating AMI permissions - {description}");
    modify_regional_images(
        &publish_args.modify_opts,
        &operation,
        &mut amis,
        &ec2_clients,
    )
    .await?;

    write_amis(
        &publish_args.ami_input,
        &amis
            .into_iter()
            .map(|(region, image)| (region.to_string(), image))
            .collect::<HashMap<String, Image>>(),
    )
    .await?;

    Ok(())
}

/// Serialize the regional-AMI map to `path` as pretty JSON.
pub(crate) async fn write_amis(path: &PathBuf, amis: &HashMap<String, Image>) -> Result<()> {
    let json = serde_json::to_string_pretty(&amis).context(error::SerializeSnafu { path })?;
    fs::write(path, &json).await.context(error::FileSnafu {
        op: "write AMIs to file",
        path,
    })?;
    tracing::info!("Wrote AMI data to {}", path.display());
    Ok(())
}

/// Return the snapshot IDs backing `image_id` in `region`.
pub(crate) async fn get_snapshots(
    image_id: &str,
    region: &Region,
    ec2_client: &Ec2Client,
) -> Result<Vec<String>> {
    let describe_response = ec2_client
        .describe_images()
        .set_image_ids(Some(vec![image_id.to_string()]))
        .send()
        .await
        .map_err(AwsSdkError::from)
        .context(error::DescribeImagesSnafu {
            region: region.as_ref(),
        })?;

    let mut images = describe_response
        .images
        .context(error::MissingInResponseSnafu {
            request_type: "DescribeImages",
            missing: "images",
        })?;
    ensure!(
        !images.is_empty(),
        error::MissingImageSnafu {
            region: region.as_ref(),
            image_id: image_id.to_string(),
        }
    );
    ensure!(
        images.len() == 1,
        error::MultipleImagesSnafu {
            region: region.as_ref(),
            images: images
                .iter()
                .map(|i| i.image_id.clone().unwrap_or_else(|| "<missing>".to_string()))
                .collect::<Vec<_>>()
        }
    );
    let image = images.remove(0);

    let bdms = image
        .block_device_mappings
        .context(error::MissingInResponseSnafu {
            request_type: "DescribeImages",
            missing: "block_device_mappings",
        })?;
    ensure!(
        !bdms.is_empty(),
        error::MissingInResponseSnafu {
            request_type: "DescribeImages",
            missing: "non-empty block_device_mappings"
        }
    );
    let mut snapshot_ids = Vec::with_capacity(bdms.len());
    for bdm in bdms {
        let ebs = bdm.ebs.context(error::MissingInResponseSnafu {
            request_type: "DescribeImages",
            missing: "ebs in block_device_mappings",
        })?;
        let snapshot_id = ebs.snapshot_id.context(error::MissingInResponseSnafu {
            request_type: "DescribeImages",
            missing: "snapshot_id in block_device_mappings.ebs",
        })?;
        snapshot_ids.push(snapshot_id);
    }
    Ok(snapshot_ids)
}

/// Fan out `get_snapshots` across all regions.
async fn get_regional_snapshots(
    amis: &HashMap<Region, Image>,
    clients: &HashMap<Region, Ec2Client>,
) -> Result<HashMap<Region, Vec<String>>> {
    let mut requests = Vec::with_capacity(amis.len());
    for (region, image) in amis {
        let ec2_client = &clients[region];
        let snapshots_future = get_snapshots(&image.id, region, ec2_client);
        let info_future = ready(region.clone());
        requests.push(join(info_future, snapshots_future));
    }
    let request_stream = stream::iter(requests).buffer_unordered(4);
    let responses: Vec<(Region, Result<Vec<String>>)> = request_stream.collect().await;

    let mut snapshots = HashMap::with_capacity(amis.len());
    for (region, snapshot_ids) in responses {
        snapshots.insert(region, snapshot_ids?);
    }
    Ok(snapshots)
}

/// Grant or revoke `createVolumePermission` on `snapshot_ids`.
pub(crate) async fn modify_snapshots(
    modify_opts: &ModifyOptions,
    operation: &OperationType,
    snapshot_ids: &[String],
    ec2_client: &Ec2Client,
    region: &Region,
) -> Result<()> {
    let mut requests = Vec::new();
    for snapshot_id in snapshot_ids {
        let response_future = ec2_client
            .modify_snapshot_attribute()
            .set_attribute(Some(SnapshotAttributeName::CreateVolumePermission))
            .set_user_ids(
                (!modify_opts.user_ids.is_empty()).then_some(modify_opts.user_ids.clone()),
            )
            .set_group_names(
                (!modify_opts.group_names.is_empty()).then_some(modify_opts.group_names.clone()),
            )
            .set_operation_type(Some(operation.clone()))
            .set_snapshot_id(Some(snapshot_id.clone()))
            .send()
            .map_err(AwsSdkError::from);
        let info_future = ready(snapshot_id.to_string());
        requests.push(join(info_future, response_future));
    }

    let request_stream = stream::iter(requests).buffer_unordered(4);
    let responses: Vec<(
        String,
        std::result::Result<
            ModifySnapshotAttributeOutput,
            AwsSdkError<ModifySnapshotAttributeError>,
        >,
    )> = request_stream.collect().await;

    for (snapshot_id, response) in responses {
        response.context(error::ModifySnapshotAttributeSnafu {
            snapshot_id,
            region: region.as_ref(),
        })?;
    }
    Ok(())
}

/// Grant/revoke `createVolumePermission` on every snapshot in every
/// region. Errors are counted (rather than fail-fast) so the top-level
/// error message can report the number of successful modifications.
async fn modify_regional_snapshots(
    modify_opts: &ModifyOptions,
    operation: &OperationType,
    snapshots: &HashMap<Region, Vec<String>>,
    clients: &HashMap<Region, Ec2Client>,
) -> Result<()> {
    let mut requests = Vec::new();
    for (region, snapshot_ids) in snapshots {
        let ec2_client = &clients[region];
        let modify_snapshot_future =
            modify_snapshots(modify_opts, operation, snapshot_ids, ec2_client, region);
        let info_future = ready((region.clone(), snapshot_ids.clone()));
        requests.push(join(info_future, modify_snapshot_future));
    }
    let request_stream = stream::iter(requests).buffer_unordered(4);
    #[allow(clippy::type_complexity)]
    let responses: Vec<((Region, Vec<String>), Result<()>)> = request_stream.collect().await;

    let mut error_count = 0u16;
    let mut success_count = 0u16;
    for ((region, snapshot_ids), response) in responses {
        match response {
            Ok(()) => {
                success_count += 1;
                tracing::debug!(
                    "Modified permissions in {} for snapshots [{}]",
                    region.as_ref(),
                    snapshot_ids.join(", "),
                );
            }
            Err(e) => {
                error_count += 1;
                tracing::error!(
                    "Failed to modify snapshot permissions in {}: {}",
                    region.as_ref(),
                    e,
                );
            }
        }
    }
    ensure!(
        error_count == 0,
        error::ModifySnapshotAttributesSnafu {
            error_count,
            success_count,
        }
    );
    Ok(())
}

/// Grant/revoke `launchPermission` on a single AMI.
pub(crate) async fn modify_image(
    modify_opts: &ModifyOptions,
    operation: &OperationType,
    image_id: &str,
    ec2_client: &Ec2Client,
) -> std::result::Result<ModifyImageAttributeOutput, Box<AwsSdkError<ModifyImageAttributeError>>> {
    ec2_client
        .modify_image_attribute()
        .set_attribute(Some(
            ImageAttributeName::LaunchPermission.as_ref().to_string(),
        ))
        .set_user_ids((!modify_opts.user_ids.is_empty()).then_some(modify_opts.user_ids.clone()))
        .set_user_groups(
            (!modify_opts.group_names.is_empty()).then_some(modify_opts.group_names.clone()),
        )
        .set_organization_arns(
            (!modify_opts.organization_arns.is_empty())
                .then_some(modify_opts.organization_arns.clone()),
        )
        .set_organizational_unit_arns(
            (!modify_opts.organizational_unit_arns.is_empty())
                .then_some(modify_opts.organizational_unit_arns.clone()),
        )
        .set_operation_type(Some(operation.clone()))
        .set_image_id(Some(image_id.to_string()))
        .send()
        .await
        .map_err(AwsSdkError::from)
        .map_err(Box::new)
}

/// Grant/revoke `launchPermission` on every AMI in the map, then
/// refresh the `public` and `launch_permissions` fields on each
/// `Image` in place so `write_amis` produces an up-to-date file.
async fn modify_regional_images(
    modify_opts: &ModifyOptions,
    operation: &OperationType,
    images: &mut HashMap<Region, Image>,
    clients: &HashMap<Region, Ec2Client>,
) -> Result<()> {
    let mut requests = Vec::new();
    for (region, image) in &mut *images {
        let image_id = &image.id;
        let ec2_client = &clients[region];
        let modify_image_future = modify_image(modify_opts, operation, image_id, ec2_client);
        let info_future = ready((region.as_ref().to_string(), image_id.clone()));
        requests.push(join(info_future, modify_image_future));
    }

    let request_stream = stream::iter(requests).buffer_unordered(4);
    #[allow(clippy::type_complexity)]
    let responses: Vec<(
        (String, String),
        std::result::Result<
            ModifyImageAttributeOutput,
            Box<AwsSdkError<ModifyImageAttributeError>>,
        >,
    )> = request_stream.collect().await;

    let mut error_count = 0u16;
    let mut success_count = 0u16;
    for ((region, image_id), modify_image_response) in responses {
        match modify_image_response {
            Ok(_) => {
                success_count += 1;
                tracing::info!("Modified permissions of image {image_id} in {region}");

                let region_key = Region::new(region.clone());
                let launch_permissions = get_launch_permissions(
                    &clients[&region_key],
                    region.as_ref(),
                    &image_id,
                )
                .await
                .context(error::DescribeImageAttributeSnafu {
                    image_id: image_id.clone(),
                    region: region.to_string(),
                })?;

                let image =
                    images
                        .get_mut(&region_key)
                        .ok_or_else(|| error::Error::MissingRegion {
                            region: region.clone(),
                        })?;
                image.public = Some(launch_permissions.iter().any(|lp| {
                    lp == &LaunchPermissionDef::Group(PermissionGroup::All.as_str().to_string())
                }));
                image.launch_permissions = Some(launch_permissions);
            }
            Err(e) => {
                error_count += 1;
                tracing::error!(
                    "Modifying permissions of {} in {} failed: {}",
                    image_id,
                    region,
                    e.as_service_error()
                        .and_then(|err| err.code())
                        .unwrap_or("unknown"),
                );
            }
        }
    }
    ensure!(
        error_count == 0,
        error::ModifyImagesAttributesSnafu {
            error_count,
            success_count,
        }
    );
    Ok(())
}

pub mod error {
    use aws_sdk_ec2::operation::{
        describe_images::DescribeImagesError,
        modify_snapshot_attribute::ModifySnapshotAttributeError,
    };
    use error_utils::AwsSdkError;
    use snafu::Snafu;
    use std::io;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    #[allow(clippy::large_enum_variant)]
    pub enum Error {
        #[snafu(display("Error reading config: {source}"))]
        Config { source: pubsys_config::Error },

        #[snafu(display(
            "Failed to describe image attributes for image {image_id} in region {region}: {source}"
        ))]
        DescribeImageAttribute {
            image_id: String,
            region: String,
            #[snafu(source(from(super::super::launch_permissions::Error, Box::new)))]
            source: Box<super::super::launch_permissions::Error>,
        },

        #[snafu(display("Failed to describe images in {region}: {source}"))]
        DescribeImages {
            region: String,
            #[snafu(source(from(AwsSdkError<DescribeImagesError>, Box::new)))]
            source: Box<AwsSdkError<DescribeImagesError>>,
        },

        #[snafu(display("Failed to deserialize input from '{}': {source}", path.display()))]
        Deserialize {
            path: PathBuf,
            source: serde_json::Error,
        },

        #[snafu(display("Failed to {op} '{}': {source}", path.display()))]
        File {
            op: String,
            path: PathBuf,
            source: io::Error,
        },

        #[snafu(display("Input '{}' is empty", path.display()))]
        Input { path: PathBuf },

        #[snafu(display("Infra.toml is missing {missing}"))]
        MissingConfig { missing: String },

        #[snafu(display("Failed to find given AMI ID {image_id} in {region}"))]
        MissingImage { region: String, image_id: String },

        #[snafu(display("Response to {request_type} was missing {missing}"))]
        MissingInResponse {
            request_type: String,
            missing: String,
        },

        #[snafu(display("Failed to find region {region} in AMI map"))]
        MissingRegion { region: String },

        #[snafu(display(
            "Failed to modify permissions of snapshot {snapshot_id} in {region}: {source}"
        ))]
        ModifySnapshotAttribute {
            snapshot_id: String,
            region: String,
            #[snafu(source(from(AwsSdkError<ModifySnapshotAttributeError>, Box::new)))]
            source: Box<AwsSdkError<ModifySnapshotAttributeError>>,
        },

        #[snafu(display(
            "Failed to modify permissions of {error_count} of {} images",
            error_count + success_count,
        ))]
        ModifyImagesAttributes {
            error_count: u16,
            success_count: u16,
        },

        #[snafu(display(
            "Failed to modify permissions of {error_count} of {} snapshots",
            error_count + success_count,
        ))]
        ModifySnapshotAttributes {
            error_count: u16,
            success_count: u16,
        },

        #[snafu(display(
            "DescribeImages in {region} with unique filters returned multiple results: {}",
            images.join(", ")
        ))]
        MultipleImages { region: String, images: Vec<String> },

        #[snafu(display("Failed to serialize output to '{}': {source}", path.display()))]
        Serialize {
            path: PathBuf,
            source: serde_json::Error,
        },

        #[snafu(display(
            "Given region(s) not in --ami-input file: {}",
            regions.join(", ")
        ))]
        UnknownRegions { regions: Vec<String> },

        #[snafu(display("AMI '{id}' in {region} did not become available: {source}"))]
        WaitAmi {
            id: String,
            region: String,
            #[snafu(source(from(super::super::wait::Error, Box::new)))]
            source: Box<super::super::wait::Error>,
        },
    }
}
pub use error::Error;
pub type Result<T> = std::result::Result<T, error::Error>;
