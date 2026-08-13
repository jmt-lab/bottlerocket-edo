//! `RegisterImage`-oriented helpers: create EBS snapshots, register
//! the AMI, and clean up on failure. Also the `get_ami_id` lookup
//! used both here (for the base region) and in the orchestrator (for
//! target regions).
//!
//! Ported from
//! `old-twoliter/tools/pubsys/src/aws/ami/register/mod.rs`.
use super::mk_amispec::{self, AmispecInputs, BottlerocketSnapshots, VIRT_TYPE};
use crate::variant::publish::aws::snapshot::{build_progress_bar, snapshot_from_image};
use amispec::AmiSpec;
use aws_sdk_ebs::Client as EbsClient;
use aws_sdk_ec2::types::Filter;
use aws_sdk_ec2::{Client as Ec2Client, config::Region};
use coldsnap::{SnapshotUploader, SnapshotWaiter};
use indicatif::{MultiProgress, ProgressBar};
use snafu::{OptionExt, ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Snapshot + image IDs of a successfully-registered AMI.
#[derive(Debug)]
pub(crate) struct RegisteredIds {
    pub image_id: String,
    pub snapshot_ids: Vec<String>,
}

/// Register an AMI from `os_image_path` (and optionally
/// `data_image_path`): upload as EBS snapshots, wait for them,
/// materialize the amispec, call `RegisterImage`. On any failure the
/// created snapshots are deleted best-effort.
pub(crate) async fn register_image(
    inputs: &AmispecInputs<'_>,
    os_image_path: &Path,
    data_image_path: Option<&Path>,
    no_progress: bool,
    region: &Region,
    ebs_client: EbsClient,
    ec2_client: &Ec2Client,
) -> Result<RegisteredIds> {
    let cleanup_snapshot_ids = Arc::new(Mutex::new(Vec::new()));
    let result = _register_image(
        inputs,
        os_image_path,
        data_image_path,
        no_progress,
        region,
        ebs_client,
        ec2_client,
        Arc::clone(&cleanup_snapshot_ids),
    )
    .await;

    if result.is_err() {
        // Best-effort cleanup. We keep the original error and just
        // warn on cleanup failure — the caller will see the real
        // problem in the returned Err.
        for snapshot_id in cleanup_snapshot_ids.lock().await.iter() {
            if let Err(e) = ec2_client
                .delete_snapshot()
                .set_snapshot_id(Some(snapshot_id.clone()))
                .send()
                .await
            {
                tracing::warn!(
                    "While cleaning up, failed to delete snapshot {snapshot_id}: {e}"
                );
            }
        }
    }
    result
}

async fn _register_image(
    inputs: &AmispecInputs<'_>,
    os_image_path: &Path,
    data_image_path: Option<&Path>,
    no_progress: bool,
    region: &Region,
    ebs_client: EbsClient,
    ec2_client: &Ec2Client,
    cleanup_snapshot_ids: Arc<Mutex<Vec<String>>>,
) -> Result<RegisteredIds> {
    let snapshots = create_snapshots(
        os_image_path,
        data_image_path,
        no_progress,
        region,
        ebs_client,
        ec2_client,
        Arc::clone(&cleanup_snapshot_ids),
    )
    .await?;

    // Rebuild the amispec inputs with concrete snapshot IDs.
    let mut inputs_with_snapshots = inputs.clone_shape();
    inputs_with_snapshots.snapshots = snapshots.clone();

    let amispec = mk_amispec::create_amispec(&inputs_with_snapshots)
        .context(AmispecSnafu)?;

    tracing::info!("Registering AMI in {region}");
    let register_response = amispec
        .as_register_image_call()
        .send_with(ec2_client)
        .await
        .map_err(error_utils::AwsSdkError::from)
        .context(RegisterImageCallSnafu {
            region: region.as_ref(),
        })?;

    let image_id = register_response.image_id.context(MissingImageIdSnafu {
        region: region.as_ref(),
    })?;

    Ok(RegisteredIds {
        image_id,
        snapshot_ids: snapshots.snapshot_ids(),
    })
}

impl<'a> AmispecInputs<'a> {
    /// Same borrow of `variant` / same `arch`, but a placeholder
    /// snapshot bundle. Used before snapshots exist so we can render
    /// a minimal amispec and look up existing AMIs by name.
    pub(crate) fn clone_shape(&self) -> AmispecInputs<'a> {
        AmispecInputs {
            name: self.name.clone(),
            description: self.description.clone(),
            arch: self.arch,
            snapshots: BottlerocketSnapshots {
                os_snapshot: self.snapshots.os_snapshot.clone(),
                data_snapshot: self.snapshots.data_snapshot.clone(),
            },
            variant: self.variant,
            user_overlay: self.user_overlay.clone(),
        }
    }
}

/// Create EBS snapshots for the OS (and optional data) image. Every
/// created snapshot ID is pushed onto `cleanup_snapshot_ids` so the
/// caller can delete them if a later step fails.
async fn create_snapshots(
    os_image_path: &Path,
    data_image_path: Option<&Path>,
    no_progress: bool,
    region: &Region,
    ebs_client: EbsClient,
    ec2_client: &Ec2Client,
    cleanup_snapshot_ids: Arc<Mutex<Vec<String>>>,
) -> Result<BottlerocketSnapshots> {
    tracing::debug!("Uploading images into EBS snapshots in {region}");

    let multi_progress = MultiProgress::new();

    let upload_one = |image_path: PathBuf, label: &'static str| {
        let progress_bar = if no_progress {
            None
        } else {
            let pb = build_progress_bar(&format!("Upload {label} snapshot"))
                .context(ProgressBarSnafu)?;
            Some(multi_progress.add(pb))
        };
        let uploader = SnapshotUploader::new(ebs_client.clone());
        let waiter = SnapshotWaiter::new(ec2_client.clone());
        let cleanup = Arc::clone(&cleanup_snapshot_ids);
        let path = image_path.clone();
        Ok::<_, Error>(async move {
            create_snapshot(&uploader, &waiter, &path, cleanup, progress_bar).await
        })
    };

    let os_fut = upload_one(os_image_path.to_owned(), "root")?;

    match data_image_path {
        Some(data_path) => {
            let data_fut = upload_one(data_path.to_owned(), "data")?;
            let (os_snapshot, data_snapshot) = tokio::join!(os_fut, data_fut);
            let _ = multi_progress.clear();
            Ok(BottlerocketSnapshots {
                os_snapshot: os_snapshot?,
                data_snapshot: Some(data_snapshot?),
            })
        }
        None => {
            let os_snapshot = os_fut.await?;
            let _ = multi_progress.clear();
            Ok(BottlerocketSnapshots {
                os_snapshot,
                data_snapshot: None,
            })
        }
    }
}

async fn create_snapshot(
    uploader: &SnapshotUploader,
    waiter: &SnapshotWaiter,
    image_path: &Path,
    cleanup_snapshot_ids: Arc<Mutex<Vec<String>>>,
    progress_bar: Option<ProgressBar>,
) -> Result<String> {
    let snapshot_id = snapshot_from_image(image_path, uploader, None, progress_bar)
        .await
        .context(SnapshotUploadSnafu {
            path: image_path.to_owned(),
        })?;

    cleanup_snapshot_ids.lock().await.push(snapshot_id.clone());

    tracing::debug!("Waiting for snapshot {snapshot_id} to become available");
    waiter
        .wait(&snapshot_id, Default::default())
        .await
        .context(SnapshotWaitSnafu)?;

    Ok(snapshot_id)
}

/// Look up an AMI ID by name + arch. Returns `Ok(None)` if no image
/// matches. `Ok(Some(id))` if exactly one matches, error if
/// multiple match (which should never happen for uniquely-named AMIs).
pub(crate) async fn get_ami_id(
    name: impl Into<String>,
    arch: impl Into<String>,
    region: &Region,
    ec2_client: &Ec2Client,
) -> Result<Option<String>> {
    let describe_response = ec2_client
        .describe_images()
        .set_owners(Some(vec!["self".to_string()]))
        .set_filters(Some(vec![
            Filter::builder()
                .set_name(Some("name".to_string()))
                .set_values(Some(vec![name.into()]))
                .build(),
            Filter::builder()
                .set_name(Some("architecture".to_string()))
                .set_values(Some(vec![arch.into()]))
                .build(),
            Filter::builder()
                .set_name(Some("image-type".to_string()))
                .set_values(Some(vec!["machine".to_string()]))
                .build(),
            Filter::builder()
                .set_name(Some("virtualization-type".to_string()))
                .set_values(Some(vec![VIRT_TYPE.to_string()]))
                .build(),
        ]))
        .send()
        .await
        .map_err(error_utils::AwsSdkError::from)
        .context(DescribeImagesSnafu {
            region: region.as_ref(),
        })?;

    let Some(mut images) = describe_response.images else {
        return Ok(None);
    };
    if images.is_empty() {
        return Ok(None);
    }
    snafu::ensure!(
        images.len() == 1,
        MultipleImagesSnafu {
            images: images
                .iter()
                .map(|i| i.image_id.clone().unwrap_or_else(|| "<missing>".to_string()))
                .collect::<Vec<_>>()
        }
    );
    let image = images.remove(0);
    let id = image.image_id.context(MissingImageIdSnafu {
        region: region.as_ref(),
    })?;
    Ok(Some(id))
}

/// Wrap an `AmiSpec` for tests that need to peek at derived state.
///
/// Kept in the crate root so downstream `orchestrator.rs` can shove
/// the amispec through it in test builds.
#[allow(dead_code)]
pub(crate) fn amispec_name(spec: &AmiSpec) -> &str {
    &spec.name
}

#[derive(Debug, Snafu)]
#[snafu(module(register_error), visibility(pub(crate)))]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Error {
    #[snafu(display("Failed to build amispec: {source}"))]
    Amispec { source: super::mk_amispec::AmispecError },

    #[snafu(display("Failed to describe images in {region}: {source}"))]
    DescribeImages {
        region: String,
        #[snafu(source(from(
            error_utils::AwsSdkError<aws_sdk_ec2::operation::describe_images::DescribeImagesError>,
            Box::new
        )))]
        source: Box<
            error_utils::AwsSdkError<aws_sdk_ec2::operation::describe_images::DescribeImagesError>,
        >,
    },

    #[snafu(display("Image response in {region} did not include an image ID"))]
    MissingImageId { region: String },

    #[snafu(display("DescribeImages with unique filters returned multiple results: {}", images.join(", ")))]
    MultipleImages { images: Vec<String> },

    #[snafu(display("Failed to create progress bar: {source}"))]
    ProgressBar {
        source: crate::variant::publish::aws::snapshot::Error,
    },

    #[snafu(display("Failed to RegisterImage in {region}: {source}"))]
    RegisterImageCall {
        region: String,
        #[snafu(source(from(
            error_utils::AwsSdkError<aws_sdk_ec2::operation::register_image::RegisterImageError>,
            Box::new
        )))]
        source: Box<
            error_utils::AwsSdkError<aws_sdk_ec2::operation::register_image::RegisterImageError>,
        >,
    },

    #[snafu(display("Failed to upload EBS snapshot from '{}': {source}", path.display()))]
    SnapshotUpload {
        path: PathBuf,
        #[snafu(source(from(crate::variant::publish::aws::snapshot::Error, Box::new)))]
        source: Box<crate::variant::publish::aws::snapshot::Error>,
    },

    #[snafu(display("EBS snapshot did not become available: {source}"))]
    SnapshotWait {
        #[snafu(source(from(coldsnap::WaitError, Box::new)))]
        source: Box<coldsnap::WaitError>,
    },
}

// Re-export snafu selectors for the file above.
pub(crate) use register_error::*;

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
