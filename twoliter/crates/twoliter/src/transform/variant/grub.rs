//! `variant-grub` transform: install grub (BIOS + UEFI) into a
//! partitioned OS disk, render `grub.cfg`, and emit a finalised
//! `os.img` ready for compression.
//!
//! Third stage of the four-transform variant pipeline. Consumes:
//!
//! - the paired [`VariantRootfsTransform`] artifact (for `root-mount/`
//!   — grub-install writes into `/boot/grub/`, ESP staging reads
//!   BIOS-grub files from the rootfs);
//! - the paired [`VariantDiskTransform`] artifact (for
//!   `os.img.partial`, the `boot.ext4` scratch, and every partition
//!   scratch we pass through to the finalizer + the
//!   [`DiskLayoutManifest`] carrying partition offsets);
//! - the optional `sbkeys` source scope (required when
//!   `image_features.uefi_secure_boot = true`).
//!
//! **Environment**: the same SDK the rootfs stage used.
//!
//! **Hash inputs**: rootfs digest, disk digest (folded together into
//! `upstream_stage_digest`), kernel-parameters (order-sensitive),
//! `uefi_secure_boot` + `first_party_stack` toggles, arch, `sbkeys`
//! digest, twoliter binary digest.
//!
//! **Outputs**: two new layers (finalised `os.img` + `boot-config`)
//! plus a full pass-through of the disk stage's raw partition
//! scratches and metadata JSON — so the finalizer takes a single dep
//! edge (`variant-grub`) rather than two (`variant-grub` +
//! `variant-disk`).

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Config, Id, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use snafu::{OptionExt, ResultExt};
use std::path::Path;
use tracing::trace;

use crate::image::{
    DiskLayoutManifest, ImageFeatures,
    bootconfig::assemble_bootconfig,
    grub::{GrubCfgOptions, GrubInstall, install_grub_bios, render_grub_cfg},
    layout::resolved_image_layout,
    partition::Part,
};

use super::error;
use super::hash::{HashInputs, hash_grub_inputs, twoliter_binary_digest};
use super::layout::{image_layout, parse_arch, partition_entry};
use super::media_types::grub as grub_media;
use super::options::VariantImageOptions;
use super::scope::resolve_optional;
use super::secure_boot::install_uefi_with_secure_boot;
use super::staging::write_layer;

/// Transform state for `variant-grub`.
pub struct VariantGrubTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantImageOptions,
    pub rootfs_addr: Addr,
    pub disk_addr: Addr,
    /// Optional secure-boot key material. Required at
    /// `transform()` time when `image_features.uefi_secure_boot` is
    /// on; a missing scope in that case surfaces as a typed
    /// [`error::Error::BuildStep`].
    pub sbkeys_source: Option<Source>,
}

#[async_trait]
impl FromElement for VariantGrubTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut options: VariantImageOptions = element.get()?;
        options.arch = Some(
            ctx.args()
                .get("arch")
                .cloned()
                .or(options.arch)
                .unwrap_or(std::env::consts::ARCH.to_string()),
        );

        let rootfs_addr: Addr = element
            .config
            .get("rootfs-addr")
            .and_then(|v| v.as_str())
            .context(error::BuildStepSnafu {
                step: "variant-grub::from-element".to_string(),
                reason: "missing `rootfs-addr` in element config".to_string(),
            })
            .and_then(|s| Addr::parse(s).map_err(error::Error::from))?;
        let disk_addr: Addr = element
            .config
            .get("disk-addr")
            .and_then(|v| v.as_str())
            .context(error::BuildStepSnafu {
                step: "variant-grub::from-element".to_string(),
                reason: "missing `disk-addr` in element config".to_string(),
            })
            .and_then(|s| Addr::parse(s).map_err(error::Error::from))?;

        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();
        let sbkeys_source = resolve_optional(&scope_table, "sbkeys", ctx).await?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            rootfs_addr,
            disk_addr,
            sbkeys_source,
        })
    }
}

#[async_trait]
impl TransformImpl for VariantGrubTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let rootfs_id = ctx
            .get(&self.rootfs_addr)
            .context(error::DependSnafu {
                addr: self.rootfs_addr.clone(),
            })?
            .get_unique_id(ctx)
            .await?;
        let disk_id = ctx
            .get(&self.disk_addr)
            .context(error::DependSnafu {
                addr: self.disk_addr.clone(),
            })?
            .get_unique_id(ctx)
            .await?;

        let sbkeys_digest = match &self.sbkeys_source {
            Some(s) => Some(s.get_unique_id().await?.digest().to_string()),
            None => None,
        };

        let binary_digest = twoliter_binary_digest();

        // Fold both upstream digests into a single upstream digest via
        // a stable concat with a separator.
        let combined_upstream = format!(
            "rootfs={}\0disk={}",
            rootfs_id.digest(),
            disk_id.digest()
        );

        let inputs = HashInputs {
            arch: self.options.arch.as_deref(),
            sdk_digest: "",
            kit_digests: vec![],
            package_digests: vec![],
            guest_image_digests: vec![],
            ca_bundle_digest: None,
            tuf_root_digest: None,
            sbkeys_digest,
            ovf_template_digest: None,
            image_features: self.options.image_features.as_set(),
            resolved_layout: image_layout(&self.options),
            image_format: self.options.image_format,
            partition_plan: self.options.partition_plan,
            kernel_parameters: self.options.kernel_parameters.clone(),
            uefi_secure_boot: self.options.image_features.uefi_secure_boot,
            first_party_stack: self.options.image_features.first_party_stack,
            os_image_publish_size_gib: self.options.os_image_publish_size_gib,
            data_image_publish_size_gib: self.options.data_image_publish_size_gib,
            upstream_stage_digest: Some(combined_upstream),
            twoliter_binary_digest: binary_digest,
        };

        let digest = hash_grub_inputs(&inputs)?;
        let id = Id::builder()
            .name(format!("{}-grub", self.options.name))
            .digest(digest)
            .version(self.options.version.clone())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(
            component = "transform",
            type = "variant-grub",
            "id is calculated to be {id}"
        );
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(vec![self.rootfs_addr.clone(), self.disk_addr.clone()])
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        if let Some(s) = &self.sbkeys_source {
            s.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        if let Some(s) = &self.sbkeys_source {
            if !s.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Stage rootfs dep into deps/rootfs/.
        let rootfs_dep_dir = Path::new("deps/rootfs");
        env.create_dir(rootfs_dep_dir).await?;
        let rootfs_dep = ctx.get(&self.rootfs_addr).context(error::DependSnafu {
            addr: self.rootfs_addr.clone(),
        })?;
        let rootfs_id = rootfs_dep.get_unique_id(ctx).await?;
        env.stage(
            ctx,
            ArtifactStageOptions::builder()
                .id(rootfs_id)
                .path(rootfs_dep_dir)
                .build(),
        )
        .await?;

        // Stage disk dep into deps/disk/.
        let disk_dep_dir = Path::new("deps/disk");
        env.create_dir(disk_dep_dir).await?;
        let disk_dep = ctx.get(&self.disk_addr).context(error::DependSnafu {
            addr: self.disk_addr.clone(),
        })?;
        let disk_id = disk_dep.get_unique_id(ctx).await?;
        env.stage(
            ctx,
            ArtifactStageOptions::builder()
                .id(disk_id)
                .path(disk_dep_dir)
                .build(),
        )
        .await?;

        // Stage sbkeys if declared.
        if let Some(source) = &self.sbkeys_source {
            let dir = Path::new("deps/sbkeys");
            env.create_dir(dir).await?;
            let id = source.get_unique_id().await?;
            trace!(
                component = "transform",
                type = "variant-grub",
                "staging sbkeys with id {id}"
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder().id(id).path(dir).build(),
            )
            .await?;
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let arch = parse_arch(self.options.arch.as_deref())?;
            let features_bitset: ImageFeatures = (&self.options.image_features).into();
            let feature_set = self.options.image_features.as_set();
            let resolved = resolved_image_layout(
                &image_layout(&self.options),
                &feature_set,
                Some(&self.options.image_format),
            );

            let vfs = Vfs::new(&id, env, log).await?;

            let work = vfs.create_dir("work").await?;
            let out = vfs.create_dir("out").await?;
            let root_mount = vfs.create_dir("root-mount").await?;
            let scratch = vfs.create_dir("scratch").await?;

            let work_path = work.path().to_path_buf();
            let out_path = out.path().to_path_buf();
            let root_mount_path = root_mount.path().to_path_buf();
            let scratch_path = scratch.path().to_path_buf();
            drop(work);
            drop(out);
            drop(root_mount);
            drop(scratch);

            // Extract the rootfs tar into root-mount/.
            let rootfs_tar = Path::new("deps/rootfs/rootfs.tar");
            vfs.command(
                "extract-rootfs",
                "tar",
                &[
                    "-xf",
                    rootfs_tar.to_string_lossy().as_ref(),
                    "--numeric-owner",
                    "-C",
                    root_mount_path.to_string_lossy().as_ref(),
                ],
            )
            .await?;

            // Read the disk-layout manifest.
            let disk_manifest_bytes = vfs.read(Path::new("deps/disk/disk-layout.json")).await?;
            let disk_manifest: DiskLayoutManifest = serde_json::from_slice(&disk_manifest_bytes)
                .map_err(|source| error::Error::BuildStep {
                    step: "variant-grub::stage".into(),
                    reason: format!("disk-layout manifest JSON: {source}"),
                })?;
            let disk_layout = &disk_manifest.disk_layout;

            // Move the disk stage's `os.img.partial` into our working
            // area as `os.img` (we're about to write to it).
            let os_disk = work_path.join("os.img");
            vfs.copy(Path::new("deps/disk/os.img.partial"), &os_disk).await?;

            // Verity row for grub.cfg comes from the disk stage's
            // manifest (`verity_info` field): we don't re-run
            // `veritysetup` here since the disk stage already
            // computed it deterministically and serialized the
            // result. This is what makes the grub stage cheap even
            // for large rootfs images.
            let verity = disk_manifest.verity_info.clone();

            // The boot ext4 scratch is unused past this point (grub-
            // install writes into the ESP inside `os.img`), but we
            // still pass it through unchanged as a layer. We only
            // copy `root.erofs` and `verity.raw` if a future step
            // needs them locally; today we don't.
            let _ = work_path;

            // (g) boot-config.
            let bootconfig_dir = root_mount_path.join("boot/boot-config.d");
            let bootconfig_text = assemble_bootconfig(&vfs, &bootconfig_dir)
                .await
                .context(error::BrImageSnafu)?;
            let bootconfig_path = out_path.join("boot-config");
            vfs.write(&bootconfig_path, bootconfig_text.as_bytes()).await?;

            // (h) grub install.
            install_grub_bios(
                &vfs,
                &GrubInstall {
                    root_mount: &root_mount_path,
                    os_image: &os_disk,
                    arch,
                },
            )
            .await
            .context(error::BrImageSnafu)?;

            let efi_a_entry = partition_entry(disk_layout, Part::EfiA)?;
            let efi_mount = scratch_path.join("efi-mount");
            let efi_scratch = scratch_path.join("efi.img");
            install_uefi_with_secure_boot(
                &vfs,
                &features_bitset,
                self.sbkeys_source.is_some(),
                &root_mount_path,
                &os_disk,
                &efi_mount,
                &efi_scratch,
                efi_a_entry.size_mib,
                efi_a_entry.offset_mib,
                arch,
            )
            .await?;

            let kernel_params = self.options.kernel_parameters.join(" ");
            let grub_cfg = render_grub_cfg(&GrubCfgOptions {
                pretty_name: &self.options.name,
                version_id: &self.options.version.to_string(),
                kernel_parameters: &kernel_params,
                dm_verity_root: &verity.dm_verity_root_row(),
                uefi_secure_boot: features_bitset.uefi_secure_boot,
                first_party_stack: features_bitset.first_party_stack,
            });
            let grub_cfg_path = root_mount_path.join("boot/grub/grub.cfg");
            vfs.create_dir(root_mount_path.join("boot/grub")).await?;
            vfs.write(&grub_cfg_path, grub_cfg.as_bytes()).await?;

            // -----------------------------------------------------
            // Emit layers.
            //
            // Two new layers (os.img raw + boot-config) plus a full
            // pass-through of the disk stage's raw partition scratches
            // and disk-layout metadata so the finalizer takes exactly
            // one dep edge on us.
            // -----------------------------------------------------
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            // New layer 0: finalised os.img.
            let os_img_out = out_path.join("os.img");
            vfs.copy(&os_disk, &os_img_out).await?;

            // Emit a table of (source, media type, hint) tuples in
            // the exact order downstream consumers expect. Every
            // path-hint is set so `env.stage` restores each file at
            // the same relative name for the finalizer.
            //
            // Ordering: new outputs first (`os.img`, `boot-config`),
            // then disk-stage pass-throughs (in the same order the
            // disk stage emitted them), then rootfs-stage pass-
            // throughs (SBOM + metadata).
            let mut layers: Vec<(std::path::PathBuf, &str, &str)> = vec![
                (os_img_out, grub_media::OS_IMG_RAW, "os.img"),
                (bootconfig_path.clone(), grub_media::BOOTCONFIG, "boot-config"),
                (
                    std::path::PathBuf::from("deps/disk/root.erofs"),
                    super::media_types::disk::ROOT_EROFS_RAW,
                    "root.erofs",
                ),
                (
                    std::path::PathBuf::from("deps/disk/verity.raw"),
                    super::media_types::disk::VERITY_RAW,
                    "verity.raw",
                ),
                (
                    std::path::PathBuf::from("deps/disk/boot.ext4"),
                    super::media_types::disk::BOOT_EXT4_RAW,
                    "boot.ext4",
                ),
            ];
            // Optional pass-throughs.
            let data_src = std::path::PathBuf::from("deps/disk/data.raw");
            if vfs.try_exists(&data_src).await? {
                layers.push((data_src, super::media_types::disk::DATA_RAW, "data.raw"));
            }
            layers.push((
                std::path::PathBuf::from("deps/disk/disk-layout.json"),
                super::media_types::disk::METADATA_JSON,
                "disk-layout.json",
            ));
            let rootfs_sbom = std::path::PathBuf::from("deps/rootfs/sbom.spdx.json");
            if vfs.try_exists(&rootfs_sbom).await? {
                layers.push((
                    rootfs_sbom,
                    super::media_types::rootfs::SBOM_SPDX,
                    "sbom.spdx.json",
                ));
            }
            let rootfs_metadata_layer =
                std::path::PathBuf::from("deps/rootfs/metadata.json");
            if vfs.try_exists(&rootfs_metadata_layer).await? {
                layers.push((
                    rootfs_metadata_layer,
                    super::media_types::rootfs::METADATA_JSON,
                    "rootfs-metadata.json",
                ));
            }

            for (src, media, hint) in &layers {
                write_layer(ctx, env, src, media, hint, "variant-grub", &mut artifact)
                    .await?;
            }

            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e),
        }
    }

    fn can_shell(&self) -> bool {
        true
    }

    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("."))?;
        Ok(())
    }
}


