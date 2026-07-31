//! `variant-disk` transform: turn a staged rootfs tree into on-disk
//! partition images.
//!
//! Second stage of the four-transform variant pipeline. Consumes the
//! [`VariantRootfsTransform`] artifact plus the shared
//! [`VariantImageOptions`] block, and emits every partition scratch
//! image plus a fully-populated `os.img.partial` — the OS disk with
//! ROOT-A + HASH-A + BOOT-A + DATA-A filled but *without* grub / ESP
//! contents.
//!
//! **Inputs** (edo `depends`): the paired `variant-rootfs` transform.
//!
//! **Source scopes** (element `source`): none. The rootfs is staged
//! from the dep artifact; there are no fresh SDK sources on this
//! stage.
//!
//! **Environment**: the same SDK the rootfs stage used (passed
//! through via `element.environment`).
//!
//! **Hash inputs**: rootfs stage's unique-id digest, `resolved_layout`,
//! `image_features`, `partition_plan`, `image_format`, twoliter binary
//! digest.
//!
//! **Outputs**:
//! | Layer | Media type | Contents |
//! |---|---|---|
//! | 0 | `.disk.partial` | `os.img.partial` (partitions filled except grub/ESP) |
//! | 1 | `.disk.root.erofs` | raw erofs root partition scratch |
//! | 2 | `.disk.verity.raw` | raw verity hash-tree scratch |
//! | 3 | `.disk.boot.ext4` | raw ext4 boot partition scratch |
//! | 4 | `.disk.data.raw` | (optional) raw data partition scratch, Split plan only |
//! | 5 | `.disk.metadata+json` | serialized [`DiskLayoutManifest`] |

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    storage::{
        Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType,
    },
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use snafu::{OptionExt, ResultExt};
use std::path::Path;
use tracing::trace;

use crate::image::{
    DiskLayoutManifest, ImageFeatures, RootfsMetadata,
    image::{mkfs_data_ext4, mkfs_data_xfs, mkfs_root_erofs},
    layout::{resolved_image_layout, validate_image_features},
    partition::{
        BOOT_SCALE_FACTOR, DiskLayout, HASH_SCALE_FACTOR, Part, apply_partition_layout,
        compute_partition_layout,
    },
    verity::{VERITY_DATA_BLOCK_SIZE, generate_verity_root},
    vfs_ext::size_bytes,
};

use super::error;
use super::hash::{HashInputs, hash_disk_inputs, twoliter_binary_digest};
use super::layout::{image_layout, partition_entry};
use super::media_types::disk as disk_media;
use super::options::VariantImageOptions;
use super::scope::copy_partition;

/// Transform state for `variant-disk`.
///
/// `rootfs_addr` is the address of the paired `variant-rootfs`
/// transform whose output this stage consumes.
pub struct VariantDiskTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantImageOptions,
    /// The `variant-rootfs` transform this disk stage sits on top of.
    pub rootfs_addr: Addr,
}

#[async_trait]
impl FromElement for VariantDiskTransform {
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

        // Locate the rootfs dep from the element's config. Emitted by
        // the config-generation layer as `rootfs-addr`.
        let rootfs_addr: Addr = element
            .config
            .get("rootfs-addr")
            .and_then(|v| v.as_str())
            .context(error::BuildStepSnafu {
                step: "variant-disk::from-element".to_string(),
                reason: "missing `rootfs-addr` in element config".to_string(),
            })
            .and_then(|s| Addr::parse(s).map_err(error::Error::from))?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            rootfs_addr,
        })
    }
}

#[async_trait]
impl TransformImpl for VariantDiskTransform {
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

        let binary_digest = twoliter_binary_digest();
        let feature_set = self.options.image_features.as_set();
        let resolved = resolved_image_layout(
            &image_layout(&self.options),
            &feature_set,
            Some(&self.options.image_format),
        );

        let inputs = HashInputs {
            arch: self.options.arch.as_deref(),
            sdk_digest: "",
            kit_digests: vec![],
            package_digests: vec![],
            guest_image_digests: vec![],
            ca_bundle_digest: None,
            tuf_root_digest: None,
            sbkeys_digest: None,
            ovf_template_digest: None,
            image_features: feature_set,
            resolved_layout: resolved,
            image_format: self.options.image_format,
            partition_plan: self.options.partition_plan,
            kernel_parameters: self.options.kernel_parameters.clone(),
            uefi_secure_boot: self.options.image_features.uefi_secure_boot,
            first_party_stack: self.options.image_features.first_party_stack,
            os_image_publish_size_gib: self.options.os_image_publish_size_gib,
            data_image_publish_size_gib: self.options.data_image_publish_size_gib,
            upstream_stage_digest: Some(rootfs_id.digest().to_string()),
            twoliter_binary_digest: binary_digest,
        };

        let digest = hash_disk_inputs(&inputs)?;
        let id = Id::builder()
            .name(format!("{}-disk", self.options.name))
            .digest(digest)
            .version(self.options.version.clone())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(
            component = "transform",
            type = "variant-disk",
            "id is calculated to be {id}"
        );
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(vec![self.rootfs_addr.clone()])
    }

    async fn prepare(&self, _log: &Log, _ctx: &Handle) -> TransformResult<()> {
        // No source-scope preparation needed; the rootfs dep is
        // handled by the scheduler.
        Ok(())
    }

    async fn needs_prepare(&self, _ctx: &Handle) -> TransformResult<bool> {
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Stage the rootfs dep's two layers into `deps/rootfs/`:
        //   deps/rootfs/rootfs.tar     (via path_hint)
        //   deps/rootfs/metadata.json  (via path_hint)
        let rootfs_dep_dir = Path::new("deps/rootfs");
        env.create_dir(rootfs_dep_dir).await?;
        let rootfs_dep = ctx.get(&self.rootfs_addr).context(error::DependSnafu {
            addr: self.rootfs_addr.clone(),
        })?;
        let rootfs_id = rootfs_dep.get_unique_id(ctx).await?;
        trace!(
            component = "transform",
            type = "variant-disk",
            "staging rootfs dep {addr} with id {id}",
            addr = &self.rootfs_addr,
            id = rootfs_id
        );
        env.stage(
            ctx,
            ArtifactStageOptions::builder()
                .id(rootfs_id)
                .path(rootfs_dep_dir)
                .build(),
        )
        .await?;
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let feature_set = self.options.image_features.as_set();
            let features_bitset: ImageFeatures = (&self.options.image_features).into();
            let resolved = resolved_image_layout(
                &image_layout(&self.options),
                &feature_set,
                Some(&self.options.image_format),
            );
            validate_image_features(
                &feature_set,
                &resolved,
                Some(&self.options.image_format),
            )
            .context(error::BrImageSnafu)?;

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

            // Extract the rootfs tar. Staged at deps/rootfs/rootfs.tar
            // by our own `stage()`.
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

            // Recover rootfs metadata (file_contexts path).
            let metadata_path = Path::new("deps/rootfs/metadata.json");
            let metadata_bytes = vfs.read(metadata_path).await?;
            let rootfs_metadata: RootfsMetadata =
                serde_json::from_slice(&metadata_bytes).map_err(|source| {
                    error::Error::BuildStep {
                        step: "variant-disk::stage".into(),
                        reason: format!("rootfs metadata JSON: {source}"),
                    }
                })?;
            let file_contexts_path = rootfs_metadata.file_contexts_path.clone();

            // (a) partition math.
            let disk_layout: DiskLayout =
                compute_partition_layout(&resolved, &features_bitset)
                    .context(error::BrImageSnafu)?;

            // (b) truncate + sgdisk on OS + optional data disk.
            let os_total_mib =
                (resolved.os_image_size_gib().0 as u32).saturating_mul(1024);
            let os_disk = work_path.join("os.img");
            apply_partition_layout(
                &vfs,
                &os_disk,
                &disk_layout.os_disk,
                self.options.partition_plan,
                os_total_mib,
            )
            .await
            .context(error::BrImageSnafu)?;

            let (data_disk, has_data) = if let Some(data_layout) = &disk_layout.data_disk {
                let data_total_mib =
                    (resolved.data_image_size_gib.0 as u32).saturating_mul(1024);
                let path = work_path.join("data.img");
                apply_partition_layout(
                    &vfs,
                    &path,
                    data_layout,
                    self.options.partition_plan,
                    data_total_mib,
                )
                .await
                .context(error::BrImageSnafu)?;
                (Some(path), true)
            } else {
                (None, false)
            };

            // (c) + (f): mkfs on each partition + verity.
            let empty_file_contexts = scratch_path.join("empty-file-contexts");
            let file_contexts_arg: &Path = match &file_contexts_path {
                Some(p) => p.as_path(),
                None => {
                    vfs.write(&empty_file_contexts, b"").await?;
                    empty_file_contexts.as_path()
                }
            };

            let root_image = work_path.join("root.erofs");
            mkfs_root_erofs(&vfs, &root_mount_path, &root_image, file_contexts_arg)
                .await
                .context(error::BrImageSnafu)?;

            let root_bytes = size_bytes(&vfs, &root_image)
                .await
                .context(error::BrImageSnafu)?;
            let block = VERITY_DATA_BLOCK_SIZE as u64;
            let root_content_bytes = (root_bytes / block) * block;
            let resolved_os_gib = resolved.os_image_size_gib().0 as u32;
            let hash_mib = resolved_os_gib * HASH_SCALE_FACTOR;
            let verity_image = work_path.join("verity.raw");
            let verity_info = generate_verity_root(
                &vfs,
                &root_image,
                &verity_image,
                hash_mib,
                root_content_bytes,
            )
            .await
            .context(error::BrImageSnafu)?;

            let boot_mib = resolved_os_gib * BOOT_SCALE_FACTOR;
            let boot_scratch = scratch_path.join("boot.ext4");
            let boot_mount = root_mount_path.join("boot");
            let boot_entry = partition_entry(&disk_layout, Part::BootA)?;
            mkfs_data_ext4(
                &vfs,
                &os_disk,
                boot_mib,
                boot_entry.offset_mib,
                &boot_scratch,
                &boot_mount,
                "",
            )
            .await
            .context(error::BrImageSnafu)?;

            let data_mount = scratch_path.join("empty-data-mount");
            vfs.create_dir(&data_mount).await?;
            let data_a_entry = partition_entry(&disk_layout, Part::DataA)?;
            let data_scratch = scratch_path.join("data.raw");
            if features_bitset.xfs_data_partition {
                mkfs_data_xfs(
                    &vfs,
                    &os_disk,
                    data_a_entry.size_mib,
                    data_a_entry.offset_mib,
                    &data_scratch,
                )
                .await
                .context(error::BrImageSnafu)?;
            } else {
                mkfs_data_ext4(
                    &vfs,
                    &os_disk,
                    data_a_entry.size_mib,
                    data_a_entry.offset_mib,
                    &data_scratch,
                    &data_mount,
                    "",
                )
                .await
                .context(error::BrImageSnafu)?;
            }

            // dd root + verity into their offsets on the OS disk.
            let root_entry = partition_entry(&disk_layout, Part::RootA)?;
            copy_partition(&vfs, &root_image, &os_disk, root_entry.offset_mib).await?;
            let hash_entry = partition_entry(&disk_layout, Part::HashA)?;
            copy_partition(&vfs, &verity_image, &os_disk, hash_entry.offset_mib).await?;

            // -----------------------------------------------------
            // Emit layers.
            // -----------------------------------------------------
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            // Collect every emitted layer's (source path, media type,
            // hint filename) into a Vec so we can push_layer them in a
            // loop and register path hints uniformly.
            let os_partial_path = out_path.join("os.img.partial");
            vfs.copy(&os_disk, &os_partial_path).await?;
            let root_scratch_out = out_path.join("root.erofs");
            vfs.copy(&root_image, &root_scratch_out).await?;
            let verity_scratch_out = out_path.join("verity.raw");
            vfs.copy(&verity_image, &verity_scratch_out).await?;
            let boot_scratch_out = out_path.join("boot.ext4");
            vfs.copy(&boot_scratch, &boot_scratch_out).await?;

            let mut layer_specs: Vec<(std::path::PathBuf, &str, &str)> = vec![
                (os_partial_path, disk_media::OS_IMG_PARTIAL, "os.img.partial"),
                (root_scratch_out, disk_media::ROOT_EROFS_RAW, "root.erofs"),
                (verity_scratch_out, disk_media::VERITY_RAW, "verity.raw"),
                (boot_scratch_out, disk_media::BOOT_EXT4_RAW, "boot.ext4"),
            ];
            if has_data {
                let data_scratch_out = out_path.join("data.raw");
                vfs.copy(&data_scratch, &data_scratch_out).await?;
                layer_specs.push((data_scratch_out, disk_media::DATA_RAW, "data.raw"));
            }
            let _ = data_disk; // kept for future consumers; the raw image is captured by the scratch

            // Metadata JSON last. Carries the fully-computed disk
            // layout plus `verity_info` so the grub stage can render
            // `grub.cfg`'s `DM_VERITY_ROOT` row without re-running
            // `veritysetup` over the same inputs.
            let manifest = DiskLayoutManifest {
                disk_layout,
                resolved_layout: resolved,
                features: features_bitset,
                partition_plan: self.options.partition_plan,
                has_data_disk: has_data,
                verity_info,
            };
            let manifest_bytes = manifest.to_json_bytes().context(error::BrImageSnafu)?;
            let manifest_out = out_path.join("disk-layout.json");
            vfs.write(&manifest_out, &manifest_bytes).await?;
            layer_specs.push((
                manifest_out,
                disk_media::METADATA_JSON,
                "disk-layout.json",
            ));

            for (src, media, hint) in layer_specs {
                let writer = ctx.storage().safe_start_layer().await?;
                env.read_stream(&src, writer.clone()).await?;
                let layer = ctx
                    .storage()
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder()
                            .media_type(MediaType::Custom(
                                media.to_string(),
                                Compression::None,
                            ))
                            .build(),
                    )
                    .await?;
                let digest = layer.digest().digest();
                artifact.layers_mut().push(layer);
                artifact
                    .config_mut()
                    .path_hints_mut()
                    .insert(digest, std::path::PathBuf::from(hint));
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
