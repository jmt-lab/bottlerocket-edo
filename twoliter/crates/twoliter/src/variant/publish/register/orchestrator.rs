//! Top-level `twoliter publish ami register` orchestrator.
//!
//! 1. Load `Infra.toml` (AWS regions/profile/role).
//! 2. Resolve the local `variant-image` artifact for
//!    `(variant, version, arch)` from edo storage.
//! 3. Extract the OS disk image (and optionally the data image)
//!    from the artifact's tar layer(s), lz4-decompressing them into
//!    temp files that `coldsnap` can slurp by path.
//! 4. Compute default AMI name/description
//!    (`bottlerocket-<variant>-<arch>-<version>-<build_id>`).
//! 5. Register in the base region; if the AMI already exists, reuse.
//! 6. Wait, grant cross-account access, copy to remaining regions.
//! 7. Write `--ami-output` JSON (compatible with the grant/revoke
//!    subcommand's `--ami-input`).
//!
//! Adapted from `old-twoliter/tools/pubsys/src/aws/ami/mod.rs`. The
//! disk-image inputs come from an edo artifact instead of `--os-image`
//! / `--data-image` CLI flags; everything downstream of the extraction
//! step matches old-twoliter closely.

use super::mk_amispec::{self, AmispecInputs, BottlerocketSnapshots};
use super::register_image::{self, RegisteredIds, get_ami_id, register_image};
use crate::model::Arch;
use crate::variant::publish::aws::client::build_client_config;
use crate::variant::publish::aws::launch_permissions::{
    LaunchPermissionDef, get_launch_permissions,
};
use crate::variant::publish::aws::public::ami_is_public;
use crate::variant::publish::aws::publish_ami::{
    Image, ModifyOptions, get_snapshots, modify_image, modify_snapshots, write_amis,
};
use crate::variant::publish::aws::region_from_string;
use crate::variant::publish::aws::wait::{self, wait_for_ami};
use aws_sdk_ebs::Client as EbsClient;
use aws_sdk_ec2::error::ProvideErrorMetadata;
use aws_sdk_ec2::operation::copy_image::{CopyImageError, CopyImageOutput};
use aws_sdk_ec2::types::OperationType;
use aws_sdk_ec2::{Client as Ec2Client, config::Region};
use aws_sdk_sts::Client as StsClient;
use aws_sdk_sts::operation::get_caller_identity::{
    GetCallerIdentityError, GetCallerIdentityOutput,
};
use clap::Parser;
use edo::context::{Addr, Context};
use edo::storage::{Compression, Id, MediaType};
use error_utils::AwsSdkError;
use futures::FutureExt;
use futures::TryFutureExt;
use futures::future::{join, lazy, ready};
use futures::stream::{self, StreamExt};
use pubsys_config::InfraConfig;
use semver::Version;
use snafu::{OptionExt, ResultExt, Snafu, ensure};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};

const WARN_SEPARATOR: &str = "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!";

/// Register a Bottlerocket AMI from a local `variant-image` artifact
/// and (optionally) copy it to additional regions.
#[derive(Parser, Debug, Clone)]
#[clap(about = "register an AMI from a variant-image artifact", long_about = None, disable_version_flag = true)]
pub struct Register {
    /// Variant name (e.g. `aws-ecs-4`). Must match a local
    /// `variant-image` transform registered at `//variants/<name>`;
    /// run `twoliter build variants/<name>` first if not. The version
    /// is derived from the resolved artifact's `Id`.
    #[arg(long)]
    pub variant: String,

    /// Target architecture: `x86_64` or `aarch64`.
    #[arg(long)]
    pub arch: Arch,

    /// Regions to publish the AMI to, comma-separated. The first is
    /// the base region (where the AMI is registered from EBS
    /// snapshots); AMIs are then copied to the remaining regions.
    /// Overrides `[aws] regions` in `Infra.toml`.
    #[arg(long, value_delimiter = ',')]
    pub regions: Vec<String>,

    /// Path to `Infra.toml`. Defaults to `<project_dir>/Infra.toml`.
    #[arg(long)]
    pub infra_config: Option<PathBuf>,

    /// Override the derived AMI name
    /// (`bottlerocket-<variant>-<arch>-<version>-<build_id>` by default).
    #[arg(long)]
    pub name: Option<String>,

    /// Override the derived AMI description.
    #[arg(long)]
    pub description: Option<String>,

    /// Suppress the progress bar for snapshot uploads.
    #[arg(long)]
    pub no_progress: bool,

    /// Optional user-supplied amispec overlay in TOML. Merges over
    /// the built-in defaults; supports handlebars templates
    /// referencing `{{ ami.unique_name }}`, `{{ ami.arch }}`,
    /// `{{ ami.description }}`, `{{ block_devices.root.snapshot_id }}`,
    /// etc.
    #[arg(long)]
    pub amispec_file: Option<PathBuf>,

    /// If set, write the regional AMI info to this path as JSON. The
    /// format is compatible with `twoliter publish ami permissions
    /// --ami-input`.
    #[arg(long)]
    pub ami_output: Option<PathBuf>,
}

impl Register {
    /// Run the register-and-copy pipeline. `ctx` is an already-primed
    /// edo context (build the variant-image transform and register
    /// twoliter's handlers) supplied by the binary's `cmd/publish.rs`.
    pub async fn run(&self, ctx: &Context) -> Result<()> {
        // Locate the variant-image artifact by asking the transform
        // registered at `//variants/<name>` for its unique Id. This is
        // deterministic and avoids the (name, version, arch) filter
        // mismatch that made the old `safe_list` scan miss the artifact
        // (the transform's stored Id name is `{variant}-img`, not
        // `{variant}`; see `variant/rpm2img/mod.rs::get_unique_id`).
        let (id, artifact) = resolve_variant_image(ctx, &self.variant).await?;
        let build_id: String = id.digest().chars().take(8).collect();
        let version: Version = id.version().context(MissingVersionSnafu { id: id.clone() })?;

        // Derive default name/description; overridable from CLI.
        let default_name = format!(
            "bottlerocket-{variant}-{arch}-{version}-{build_id}",
            variant = self.variant,
            arch = self.arch,
        );
        let name = self.name.clone().unwrap_or(default_name);
        let default_description = format!(
            "Bottlerocket {variant} {version} ({arch}, build {build_id})",
            variant = self.variant,
            arch = self.arch,
        );
        let description = self.description.clone().or(Some(default_description));

        // Extract os.img (and optionally data.img) from the artifact's
        // tar layer(s). We land them in temp files so `coldsnap` can
        // consume them by path.
        let (os_temp, data_temp) = extract_disk_images(
            ctx,
            &artifact,
            &self.variant,
            &self.arch,
            &version,
            &build_id,
        )
        .await?;

        // Load Infra.toml.
        let infra_config_path = self
            .infra_config
            .clone()
            .unwrap_or_else(|| ctx.project_dir().join("Infra.toml"));
        let infra_config =
            InfraConfig::from_path_or_default(&infra_config_path).context(ConfigSnafu)?;
        let aws = infra_config.aws.unwrap_or_default();

        // Resolve region list: CLI wins, otherwise Infra.toml.
        let mut regions: Vec<Region> = if !self.regions.is_empty() {
            self.regions.iter().map(|s| region_from_string(s)).collect()
        } else {
            aws.regions.iter().map(|s| region_from_string(s)).collect()
        };
        ensure!(
            !regions.is_empty(),
            MissingConfigSnafu {
                missing: "aws.regions",
            }
        );
        let base_region = regions.remove(0);

        // Load user overlay text if given.
        let user_overlay = mk_amispec::load_amispec_file(self.amispec_file.as_deref())
            .await
            .context(AmispecSnafu)?;

        // Load VariantOptions from the artifact's stored config so
        // we get the same features/partition-plan/os-size the image
        // was built with — not whatever the local Twoliter.toml says
        // now (which may be a rebuild-in-progress).
        //
        // Simpler alternative: read from the current on-disk
        // Twoliter.toml. Correct in practice, less accurate for
        // "reproduce this exact build's AMI" workflows.
        //
        // For MVP we go with the current project config because it's
        // what's already loaded. Follow-on: pull it from artifact
        // config metadata.
        let variant_options = load_variant_options(ctx, &self.variant).await?;

        let base_inputs = AmispecInputs {
            name: name.clone(),
            description: description.clone(),
            arch: self.arch.as_ec2(),
            snapshots: BottlerocketSnapshots {
                // Placeholder — replaced with real IDs by
                // `register_image` before rendering the final amispec.
                os_snapshot: String::new(),
                data_snapshot: variant_options
                    .image
                    .partitions
                    .expects_data()
                    .then(String::new),
            },
            variant: &variant_options,
            user_overlay,
        };

        // Build EC2/EBS clients for the base region.
        let client_config = build_client_config(&base_region, &base_region, &aws).await;
        let base_ebs_client = EbsClient::new(&client_config);
        let base_ec2_client = Ec2Client::new(&client_config);

        // If an AMI with our name+arch already exists in the base
        // region, adopt it; otherwise register a new one.
        let existing = get_ami_id(&name, self.arch.as_ec2(), &base_region, &base_ec2_client)
            .await
            .context(RegisterSnafu)?;

        let mut public = false;
        let mut launch_permissions: Vec<LaunchPermissionDef> = vec![];
        let (ids_of_image, already_registered) = if let Some(found_id) = existing {
            tracing::warn!(
                "\n{sep}\n\nFound '{name}' already registered in {region}: {found_id}\n\n{sep}",
                sep = WARN_SEPARATOR,
                region = base_region.as_ref(),
            );
            let snapshot_ids = get_snapshots(&found_id, &base_region, &base_ec2_client)
                .await
                .context(PermissionsSnafu)?;
            public = ami_is_public(&base_ec2_client, base_region.as_ref(), &found_id)
                .await
                .context(PublicSnafu)?;
            launch_permissions =
                get_launch_permissions(&base_ec2_client, base_region.as_ref(), &found_id)
                    .await
                    .context(LaunchPermissionsSnafu)?;
            (
                RegisteredIds {
                    image_id: found_id,
                    snapshot_ids,
                },
                true,
            )
        } else {
            tracing::info!("Registering '{name}' in {}", base_region.as_ref());
            let new_ids = register_image(
                &base_inputs,
                os_temp.path(),
                data_temp.as_ref().map(|f| f.path()),
                self.no_progress,
                &base_region,
                base_ebs_client,
                &base_ec2_client,
            )
            .await
            .context(RegisterSnafu)?;
            tracing::info!(
                "Registered AMI '{name}' in {}: {}",
                base_region.as_ref(),
                new_ids.image_id,
            );
            (new_ids, false)
        };

        let mut amis: HashMap<String, Image> = HashMap::new();
        amis.insert(
            base_region.as_ref().to_string(),
            Image::new(
                &ids_of_image.image_id,
                &name,
                Some(public),
                Some(launch_permissions),
            ),
        );

        if !regions.is_empty() {
            // Wait for the base AMI to be available (multiple
            // consecutive success reads for a fresh registration; a
            // single read is sufficient for an existing one).
            let successes_required = if already_registered { 1 } else { 3 };
            wait_for_ami(
                &ids_of_image.image_id,
                &base_region,
                &base_region,
                "available",
                successes_required,
                &aws,
            )
            .await
            .context(WaitSnafu {
                id: &ids_of_image.image_id,
                region: base_region.as_ref(),
            })?;

            // Grant target-region accounts access to the source AMI +
            // its snapshots so `CopyImage` works cross-account.
            let mut account_ids = get_account_ids(&regions, &base_region, &aws).await?;
            let sts_config = build_client_config(&base_region, &base_region, &aws).await;
            let base_sts = StsClient::new(&sts_config);
            let response = base_sts
                .get_caller_identity()
                .send()
                .await
                .map_err(AwsSdkError::from)
                .context(GetCallerIdentitySnafu {
                    region: base_region.as_ref().to_string(),
                })?;
            let base_account_id = response.account.context(MissingCallerAccountSnafu)?;
            account_ids.remove(&base_account_id);

            if !account_ids.is_empty() {
                tracing::info!(
                    "Granting {} target accounts access to copy source AMI + snapshots",
                    account_ids.len()
                );
                let modify_options = ModifyOptions {
                    user_ids: account_ids.into_iter().collect(),
                    group_names: vec![],
                    organization_arns: vec![],
                    organizational_unit_arns: vec![],
                };
                modify_snapshots(
                    &modify_options,
                    &OperationType::Add,
                    &ids_of_image.snapshot_ids,
                    &base_ec2_client,
                    &base_region,
                )
                .await
                .context(PermissionsSnafu)?;
                modify_image(
                    &modify_options,
                    &OperationType::Add,
                    &ids_of_image.image_id,
                    &base_ec2_client,
                )
                .await
                .map_err(|source| Error::ModifyImage { source })?;
            }

            // Build regional clients.
            let mut ec2_clients = HashMap::with_capacity(regions.len());
            for region in &regions {
                let client_config = build_client_config(region, &base_region, &aws).await;
                ec2_clients.insert(region.clone(), Ec2Client::new(&client_config));
            }

            // Skip regions where an AMI with the same name already
            // exists; kick off `CopyImage` for the rest.
            let mut get_requests = Vec::with_capacity(regions.len());
            for region in &regions {
                let ec2_client = &ec2_clients[region];
                let get_request = get_ami_id(&name, self.arch.as_ec2(), region, ec2_client);
                let info_future = ready(region.clone());
                get_requests.push(join(info_future, get_request));
            }
            let get_stream = stream::iter(get_requests).buffer_unordered(4);
            let get_responses: Vec<(
                Region,
                std::result::Result<Option<String>, register_image::Error>,
            )> = get_stream.collect().await;

            let mut copy_requests = Vec::with_capacity(regions.len());
            for (region, get_response) in get_responses {
                let existing = get_response.context(RegisterSnafu)?;
                if let Some(id) = existing {
                    tracing::info!(
                        "Found '{name}' already registered in {}: {id}",
                        region.as_ref()
                    );
                    let public =
                        ami_is_public(&ec2_clients[&region], region.as_ref(), &id)
                            .await
                            .context(PublicSnafu)?;
                    let lp =
                        get_launch_permissions(&ec2_clients[&region], region.as_ref(), &id)
                            .await
                            .context(LaunchPermissionsSnafu)?;
                    amis.insert(
                        region.as_ref().to_string(),
                        Image::new(&id, &name, Some(public), Some(lp)),
                    );
                    continue;
                }

                let ec2_client = &ec2_clients[&region];
                let base_region_owned = base_region.clone();
                let region_for_msg = region.clone();
                let copy_future = ec2_client
                    .copy_image()
                    .set_description(description.clone())
                    .set_name(Some(name.clone()))
                    .set_source_image_id(Some(ids_of_image.image_id.clone()))
                    .set_source_region(Some(base_region_owned.as_ref().to_string()))
                    .set_copy_image_tags(Some(true))
                    .send()
                    .map_err(AwsSdkError::from);
                let region_future = ready(region.clone());
                let message_future = lazy(move |_| {
                    tracing::info!(
                        "Starting copy from {} to {}",
                        base_region_owned.as_ref(),
                        region_for_msg.as_ref(),
                    );
                });
                copy_requests.push(message_future.then(|_| join(region_future, copy_future)));
            }

            if !copy_requests.is_empty() {
                let copy_stream = stream::iter(copy_requests).buffer_unordered(4);
                let copy_responses: Vec<(
                    Region,
                    std::result::Result<CopyImageOutput, AwsSdkError<CopyImageError>>,
                )> = copy_stream.collect().await;

                let mut saw_error = false;
                for (region, copy_response) in copy_responses {
                    match copy_response {
                        Ok(success) => {
                            if let Some(image_id) = success.image_id {
                                tracing::info!(
                                    "Registered AMI '{name}' in {}: {image_id}",
                                    region.as_ref()
                                );
                                amis.insert(
                                    region.as_ref().to_string(),
                                    Image::new(&image_id, &name, Some(false), Some(vec![])),
                                );
                            } else {
                                saw_error = true;
                                tracing::error!(
                                    "Registered AMI '{name}' in {} but didn't receive an AMI ID",
                                    region.as_ref()
                                );
                            }
                        }
                        Err(e) => {
                            saw_error = true;
                            tracing::error!(
                                "Copy to {} failed: {}",
                                region.as_ref(),
                                e.as_service_error().and_then(|s| s.code()).unwrap_or("unknown"),
                            );
                        }
                    }
                }
                ensure!(!saw_error, AmiCopySnafu);
            }
        }

        if let Some(ref path) = self.ami_output {
            write_amis(path, &amis).await.context(WriteAmisSnafu)?;
        }

        Ok(())
    }
}

/// Public entry-point that mirrors old-twoliter's `aws::ami::run` for
/// completeness of the module's exported surface, but the real work
/// hangs off `Register::run` above.
pub async fn run(register: &Register, ctx: &Context) -> Result<()> {
    register.run(ctx).await
}

impl Arch {
    /// EC2 arch string: `x86_64` or `arm64`.
    fn as_ec2(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "arm64",
        }
    }
}

impl crate::variant::image::PartitionPlan {
    fn expects_data(&self) -> bool {
        matches!(self, Self::Split)
    }
}

/// Resolve the local `variant-image` artifact by asking the transform
/// at `//variants/<variant>` for its `Id`, then opening it from
/// storage.
///
/// This mirrors how `twoliter publish kit` (`cmd/publish.rs`) and
/// `twoliter checkout` (`cmd/checkout.rs`) locate their artifacts.
/// The address is the source of truth for which artifact belongs to
/// which variant name — filtering by `Id::name()` would need to match
/// `{variant}-img` (per `variant/rpm2img/mod.rs::get_unique_id`), which
/// the older `safe_list` scan got wrong.
async fn resolve_variant_image(
    ctx: &Context,
    variant: &str,
) -> Result<(Id, edo::storage::Artifact)> {
    let addr = Addr::parse(&format!("//variants/{variant}"))
        .map_err(|source| Error::AddrParse { source })?;
    let transform = ctx
        .get_transform(&addr)
        .ok_or_else(|| Error::NoVariantTransform { addr: addr.clone() })?;
    let handle = ctx.get_handle();
    let id = transform
        .get_unique_id(&handle)
        .await
        .map_err(|source| Error::TransformId {
            addr: addr.clone(),
            source: Box::new(source),
        })?;
    let artifact = ctx.storage().safe_open(&id).await.map_err(|source| {
        Error::StorageOpen {
            id: id.clone(),
            source: Box::new(source),
        }
    })?;
    Ok((id, artifact))
}

/// Extract the OS disk image (and, if present, the data image) from
/// the `variant-image` artifact's tar layer(s), lz4-decompressing
/// each into a `NamedTempFile` that `coldsnap` can consume by path.
///
/// ## What the artifact actually contains
///
/// `RPM2ImageTransform` emits a single `Tar(Compression::None)` layer
/// that snapshots rpm2img's `OUTPUT_DIR`. Entries live under
/// `${VERSION_ID}-${BUILD_ID}/` (rpm2img.sh:83) with filenames built
/// from `FILENAME_PREFIX =
/// <image_name>-<variant>-<arch>-<version_id>-<build_id>`
/// (imghelper.sh:6,44-48). For `image.format = "raw"`:
///
///   - `<prefix>.img.lz4`       — full-disk OS image (lz4-frame)
///   - `<prefix>-data.img.lz4`  — data image (only for `partitions = split`)
///   - `<prefix>-root.ext4.lz4`, `<prefix>-boot.ext4.lz4`,
///     `<prefix>-root.verity.lz4` — partition pieces (ignored)
///
/// plus a handful of shorter-name aliases (`<image_name>-<variant>-
/// <arch>.img.lz4`, `-<version_id>.img.lz4`, `-v<version_id>.img.lz4`)
/// produced by `imghelper.sh::symlink_image`. Old-twoliter's
/// `publish-ami` consumed a bare uncompressed `os.img`, so we un-lz4
/// the payload before handing off to `coldsnap`.
///
/// ## Matching rule
///
/// We reconstruct `FILENAME_PREFIX` from the caller's `(variant, arch,
/// version, build_id)` — every piece is already available in `run()`
/// (variant/arch from the CLI, version/build_id from the resolved
/// artifact `Id`) — and match tar entries by **exact** filename:
///
///   - OS image  = `bottlerocket-<variant>-<arch>-<version>-<build_id>.img.lz4`
///   - data image = `bottlerocket-<variant>-<arch>-<version>-<build_id>-data.img.lz4`
///
/// This nails down the concrete versioned file rather than one of the
/// three symlink aliases, and refuses to guess if two entries in one
/// artifact match the same target (`AmbiguousDiskImage`). Symlinks
/// and other non-regular entries are skipped first, since in some tar
/// implementations they'd otherwise show up as zero-byte regular
/// entries and silently poison the extract.
///
/// `IMAGE_NAME` is hard-coded to `"bottlerocket"` — that's how
/// `rpm2img/mod.rs::stage()` (`vfs.set_env("IMAGE_NAME", "bottlerocket")`)
/// sets it, and it's not user-configurable today. If that ever
/// changes, thread the value through from the transform's stored
/// config instead of duplicating the string here.
async fn extract_disk_images(
    ctx: &Context,
    artifact: &edo::storage::Artifact,
    variant: &str,
    arch: &Arch,
    version: &Version,
    build_id: &str,
) -> Result<(NamedTempFile, Option<NamedTempFile>)> {
    use tokio_tar::{Archive, EntryType};

    let filename_prefix =
        format!("bottlerocket-{variant}-{arch}-{version}-{build_id}");
    let os_name = format!("{filename_prefix}.img.lz4");
    let data_name = format!("{filename_prefix}-data.img.lz4");

    let mut os_temp: Option<NamedTempFile> = None;
    let mut os_source: Option<PathBuf> = None;
    let mut data_temp: Option<NamedTempFile> = None;
    let mut data_source: Option<PathBuf> = None;

    for layer in artifact.layers() {
        let MediaType::Tar(compression) = layer.media_type() else {
            // Only tar layers can carry image files. Skip json/config
            // and any other typed layers.
            continue;
        };
        let reader = BufReader::new(
            ctx.storage()
                .safe_read(layer)
                .await
                .map_err(|source| Error::StorageRead {
                    source: Box::new(source),
                })?,
        );
        let reader: std::pin::Pin<Box<dyn AsyncRead + Send>> = match compression {
            Compression::None => Box::pin(reader),
            Compression::Gzip => Box::pin(
                async_compression::tokio::bufread::GzipDecoder::new(reader),
            ),
            Compression::Zstd => Box::pin(
                async_compression::tokio::bufread::ZstdDecoder::new(reader),
            ),
            Compression::Bzip2 => Box::pin(
                async_compression::tokio::bufread::BzDecoder::new(reader),
            ),
            Compression::Xz => Box::pin(
                async_compression::tokio::bufread::XzDecoder::new(reader),
            ),
            Compression::Lz4 => Box::pin(
                async_compression::tokio::bufread::Lz4Decoder::new(reader),
            ),
            Compression::Lzma => Box::pin(
                async_compression::tokio::bufread::LzmaDecoder::new(reader),
            ),
        };

        let mut archive = Archive::new(reader);
        let mut entries = archive.entries().map_err(|source| Error::ExtractTar {
            source: Box::new(source),
        })?;
        while let Some(entry) = futures::StreamExt::next(&mut entries).await {
            let mut entry = entry.map_err(|source| Error::ExtractTar {
                source: Box::new(source),
            })?;

            if entry.header().entry_type() != EntryType::Regular {
                continue;
            }

            let path = entry
                .path()
                .map_err(|source| Error::ExtractTar {
                    source: Box::new(source),
                })?
                .into_owned();
            let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };

            let (slot, source_slot, label, expected) = if filename == os_name {
                (&mut os_temp, &mut os_source, "os.img", &os_name)
            } else if filename == data_name {
                (&mut data_temp, &mut data_source, "data.img", &data_name)
            } else {
                continue;
            };

            if let Some(prev) = source_slot.as_ref() {
                return Err(Error::AmbiguousDiskImage {
                    expected: expected.clone(),
                    first: prev.clone(),
                    second: path.clone(),
                });
            }

            tracing::debug!(
                "extracting {label} from artifact tar entry {}",
                path.display()
            );

            let tmp = tempfile::NamedTempFile::new().context(TempFileSnafu)?;
            let tmp_path = tmp.path().to_path_buf();

            // Decompress the lz4-frame stream from the tar entry
            // into the temp file. `coldsnap` needs an uncompressed
            // raw disk image on disk to slice into EBS snapshots.
            let mut decoder =
                async_compression::tokio::bufread::Lz4Decoder::new(
                    BufReader::new(&mut entry),
                );
            let mut file =
                std::fs::File::create(&tmp_path).context(TempFileSnafu)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = decoder
                    .read(&mut buf)
                    .await
                    .map_err(|source| Error::DecompressImage {
                        filename: filename.to_string(),
                        source,
                    })?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).context(TempFileSnafu)?;
            }
            file.flush().context(TempFileSnafu)?;
            *slot = Some(tmp);
            *source_slot = Some(path);
        }
    }

    let os_temp = os_temp.ok_or_else(|| Error::MissingDiskImage {
        filename: os_name.clone(),
    })?;
    Ok((os_temp, data_temp))
}

/// Load a slimmed `VariantOptions` off the on-disk
/// `variants/<name>/variant.toml`.
///
/// We deliberately re-parse the current project file rather than
/// pulling from the artifact's stored metadata: the artifact's
/// `Config::metadata()` block does not yet capture image features
/// (rpm2img/mod.rs digests them but doesn't stash a decodable copy),
/// and the on-disk toml is the same source that produced the
/// artifact under normal workflows (the user hasn't edited it
/// between `build` and `publish ami register` in the common case).
///
/// Follow-on: once `RPM2ImageTransform` records `VariantOptions` in
/// its artifact metadata, prefer that source for reproducibility.
async fn load_variant_options(
    ctx: &Context,
    variant: &str,
) -> Result<crate::variant::options::VariantOptions> {
    let path = ctx
        .project_dir()
        .join("variants")
        .join(variant)
        .join("variant.toml");
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|source| Error::ReadVariantToml {
            path: path.clone(),
            source,
        })?;
    // The on-disk file is a `config::variant::Variant`; convert it
    // to `VariantOptions` by emitting the same JSON blob
    // `config::variant::Variant::to_edo` would emit and then
    // deserializing that. Reuses the transform side's
    // canonical parser.
    let cfg: crate::config::Config =
        toml::from_str(&text).map_err(|source| Error::ParseVariantToml {
            path: path.clone(),
            source,
        })?;
    let crate::config::Config::V1(crate::config::ConfigType::Variant(config_variant)) = cfg else {
        return Err(Error::VariantConfigWrongKind {
            path: path.clone(),
        });
    };
    let options = variant_options_from_config(&config_variant);
    Ok(options)
}

/// Best-effort projection of `config::variant::Variant` (on-disk
/// TOML shape) onto the fields of `VariantOptions` the publish path
/// actually reads. Fields we don't consume are stubbed with zero
/// values — they won't be inspected by `mk_amispec`.
fn variant_options_from_config(
    cfg: &crate::config::variant::Variant,
) -> crate::variant::options::VariantOptions {
    use crate::model::Arch;
    use crate::variant::features::{ImageFeature, ImageFeatures};
    use crate::variant::image::{ImageFormat, PartitionPlan};
    use crate::variant::options::{ImageOptions, OsOptions, VariantOptions};

    let arch = cfg
        .arch
        .as_ref()
        .and_then(|a| Arch::parse(a).ok())
        .unwrap_or_default();

    let format: ImageFormat = cfg.image.format.clone().unwrap_or(ImageFormat::Raw);

    let partitions = cfg
        .image
        .partition_plan
        .as_deref()
        .and_then(|s| match s {
            "split" => Some(PartitionPlan::Split),
            "unified" => Some(PartitionPlan::Unified),
            _ => None,
        })
        .unwrap_or(PartitionPlan::Unified);

    // Rehydrate features from the on-disk `BTreeMap<String, bool>`
    // by round-tripping through the `ImageFeatures` deserializer,
    // which knows every feature name.
    let features_toml =
        toml::to_string(&cfg.image.features).unwrap_or_default();
    let features: ImageFeatures = toml::from_str(&features_toml).unwrap_or_default();
    // Silence the unused-import warning when features is empty.
    let _ = ImageFeature::ALL;

    VariantOptions {
        name: crate::variant::Variant::new(&cfg.name).expect("variant.toml name is valid"),
        version: cfg.version.clone(),
        arch,
        depends: Vec::new(),
        guest_images: Vec::new(),
        image: ImageOptions {
            format,
            partitions,
            os_size: cfg.image.os_image_size_gib,
            data_size: cfg.image.data_image_size_gib,
            features,
            ovf_template: None,
        },
        os: OsOptions {
            kernel_parameters: cfg.kernel_parameters.clone(),
            packages: Vec::new(),
        },
    }
}

/// STS `GetCallerIdentity` fan-out to discover which account owns
/// each target region.
async fn get_account_ids(
    regions: &[Region],
    base_region: &Region,
    aws: &pubsys_config::AwsConfig,
) -> Result<HashSet<String>> {
    let mut sts_clients = HashMap::with_capacity(regions.len());
    for region in regions {
        let client_config = build_client_config(region, base_region, aws).await;
        sts_clients.insert(region.clone(), StsClient::new(&client_config));
    }
    let mut requests = Vec::with_capacity(regions.len());
    for region in regions {
        let sts_client = &sts_clients[region];
        let response_future = sts_client
            .get_caller_identity()
            .send()
            .map_err(AwsSdkError::from);
        let region_future = ready(region.clone());
        requests.push(join(region_future, response_future));
    }
    let request_stream = stream::iter(requests).buffer_unordered(4);
    let responses: Vec<(
        Region,
        std::result::Result<GetCallerIdentityOutput, AwsSdkError<GetCallerIdentityError>>,
    )> = request_stream.collect().await;

    let mut ids = HashSet::new();
    for (region, response) in responses {
        let response = response.context(GetCallerIdentitySnafu {
            region: region.as_ref().to_string(),
        })?;
        let account_id = response.account.context(MissingCallerAccountSnafu)?;
        ids.insert(account_id);
    }
    Ok(ids)
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) mod error {
    use aws_sdk_ec2::operation::modify_image_attribute::ModifyImageAttributeError;
    use aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityError;
    use edo::context::Addr;
    use error_utils::AwsSdkError;
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    #[allow(clippy::large_enum_variant)]
    pub enum Error {
        #[snafu(display("Some AMIs failed to copy — see logs above"))]
        AmiCopy,

        #[snafu(display("Failed to parse edo addr: {source}"))]
        AddrParse { source: edo::context::ContextError },

        #[snafu(display("Failed to build amispec: {source}"))]
        Amispec {
            source: super::super::mk_amispec::AmispecError,
        },

        #[snafu(display("Error reading Infra.toml: {source}"))]
        Config { source: pubsys_config::Error },

        #[snafu(display("Failed to extract disk image from variant-image tar: {source}"))]
        ExtractTar {
            source: Box<dyn std::error::Error + Send + Sync>,
        },

        #[snafu(display("Failed to lz4-decompress '{filename}': {source}"))]
        DecompressImage {
            filename: String,
            source: std::io::Error,
        },

        #[snafu(display("GetCallerIdentity in {region} failed: {source}"))]
        GetCallerIdentity {
            region: String,
            #[snafu(source(from(AwsSdkError<GetCallerIdentityError>, Box::new)))]
            source: Box<AwsSdkError<GetCallerIdentityError>>,
        },

        #[snafu(display("get_launch_permissions failed: {source}"))]
        LaunchPermissions {
            source: crate::variant::publish::aws::launch_permissions::Error,
        },

        #[snafu(display("Infra.toml is missing {missing}"))]
        MissingConfig { missing: String },
        #[snafu(display("GetCallerIdentity response missing account"))]
        MissingCallerAccount,
        #[snafu(display(
            "Variant-image tar did not contain any entry named '{filename}'. \
             Check that `twoliter build variants/<name>` produced a raw disk \
             image (image.format=\"raw\") and that the variant/arch/version \
             match the artifact."
        ))]
        MissingDiskImage { filename: String },

        #[snafu(display(
            "Variant-image tar contained multiple entries named '{expected}': \
             {} and {} — refusing to guess which is authoritative",
            first.display(), second.display()
        ))]
        AmbiguousDiskImage {
            expected: String,
            first: std::path::PathBuf,
            second: std::path::PathBuf,
        },

        #[snafu(display(
            "no variant-image transform registered at {addr}; run \
             `twoliter build variants/<name>` first"
        ))]
        NoVariantTransform { addr: Addr },

        #[snafu(display("failed to compute unique id for transform at {addr}: {source}"))]
        TransformId {
            addr: Addr,
            source: Box<edo::transform::TransformError>,
        },

        #[snafu(display(
            "variant-image artifact {id} has no semver version; \
             cannot derive AMI name/description"
        ))]
        MissingVersion { id: edo::storage::Id },

        #[snafu(display("ModifyImageAttribute failed: {source}"))]
        ModifyImage {
            source: Box<AwsSdkError<ModifyImageAttributeError>>,
        },

        #[snafu(display("Permissions helper failed: {source}"))]
        Permissions {
            source: crate::variant::publish::aws::publish_ami::Error,
        },

        #[snafu(display("ami_is_public failed: {source}"))]
        Public {
            source: crate::variant::publish::aws::public::Error,
        },

        #[snafu(display("register_image failed: {source}"))]
        Register {
            source: super::super::register_image::Error,
        },

        #[snafu(display("Failed to open artifact {id}: {source}"))]
        StorageOpen {
            id: edo::storage::Id,
            source: Box<edo::storage::StorageError>,
        },
        #[snafu(display("Failed to read artifact layer: {source}"))]
        StorageRead {
            source: Box<edo::storage::StorageError>,
        },

        #[snafu(display("Failed to create tempfile: {source}"))]
        TempFile { source: std::io::Error },

        #[snafu(display("Failed to read variant.toml at '{}': {source}", path.display()))]
        ReadVariantToml {
            path: std::path::PathBuf,
            source: std::io::Error,
        },
        #[snafu(display("Failed to parse variant.toml at '{}': {source}", path.display()))]
        ParseVariantToml {
            path: std::path::PathBuf,
            source: toml::de::Error,
        },
        #[snafu(display(
            "File at '{}' is not a variant.toml (schema-version/kind mismatch)",
            path.display()
        ))]
        VariantConfigWrongKind { path: std::path::PathBuf },

        #[snafu(display("wait_for_ami on {id} in {region} failed: {source}"))]
        Wait {
            id: String,
            region: String,
            #[snafu(source(from(crate::variant::publish::aws::wait::Error, Box::new)))]
            source: Box<crate::variant::publish::aws::wait::Error>,
        },

        #[snafu(display("Failed to write regional AMI JSON: {source}"))]
        WriteAmis {
            source: crate::variant::publish::aws::publish_ami::Error,
        },
    }
}
pub use error::Error;

// Snafu selectors used above.
use error::*;
