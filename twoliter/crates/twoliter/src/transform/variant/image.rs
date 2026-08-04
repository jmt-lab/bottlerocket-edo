//! `variant-image` finalizer: compress every stage's on-disk artifact
//! into an `.lz4` layer, emit `manifest.json`, and (on VMDK builds)
//! assemble the OVA sidecar.
//!
//! Fourth and last stage of the four-transform variant pipeline. This
//! transform has no build logic beyond compression + manifest + OVA:
//! it takes the finalised `os.img` and the partition scratches
//! produced upstream and re-emits them, unchanged, in the doc-
//! specified eight-layer artifact shape. Downstream `twoliter publish
//! ami` / `twoliter publish repo` reads exactly the same media types
//! this transform emitted before the decomposition.
//!
//! **Inputs** (edo `depends`): the paired `variant-grub` transform.
//! Grub passes through every intermediate layer the finalizer needs
//! (`os.img`, `boot-config`, `root.erofs`, `verity.raw`, `boot.ext4`,
//! optional `data.raw`, `disk-layout.json`, rootfs SBOM), so the
//! finalizer takes a single dep edge rather than three.
//!
//! **Source scopes**: `ovf-template` (optional, required when
//! `image_format = vmdk`).
//!
//! **Hash inputs**: grub stage's unique-id digest, `image_format`,
//! `os_image_publish_size_gib`, `data_image_publish_size_gib`,
//! `ovf_template_digest`, twoliter binary digest.
//!
//! **Outputs** (unchanged from pre-decomposition monolith):
//! - `os.img.lz4`, optional `data.img.lz4`
//! - `boot.ext4.lz4`, `root.ext4.lz4`, `verity.lz4`
//! - `boot-config`, `manifest.json`, `sbom.spdx.json`
//!
//! **Notes on data-image size.** The finalizer decides `has_data`
//! from `disk-layout.json.has_data_disk` (produced by the disk
//! stage), not by re-deriving from `image_features` — this is the
//! single source of truth for whether a `data.img.lz4` layer appears
//! in the artifact.

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
    ArtifactSuffix, DiskLayoutManifest, ImageFormat, VariantManifest, compress::compress_image,
};

use super::error;
use super::hash::{HashInputs, hash_image_inputs, twoliter_binary_digest};
use super::layout::{image_layout, parse_arch};
use super::media_types::final_ as output_media_types;
use super::options::VariantImageOptions;
use super::scope::{pick_first_in, resolve_optional};
use super::staging::write_layer;

/// Transform state for the `variant-image` finalizer.
pub struct VariantImageTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantImageOptions,
    /// The `variant-grub` transform this finalizer sits on top of.
    pub grub_addr: Addr,
    /// Optional OVF template (`ovf-template` scope). Required when
    /// `image_format = vmdk`.
    pub ovf_template_source: Option<Source>,
}

#[async_trait]
impl FromElement for VariantImageTransform {
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

        let grub_addr: Addr = element
            .config
            .get("grub-addr")
            .and_then(|v| v.as_str())
            .context(error::BuildStepSnafu {
                step: "variant-image::from-element".to_string(),
                reason: "missing `grub-addr` in element config".to_string(),
            })
            .and_then(|s| Addr::parse(s).map_err(error::Error::from))?;

        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();
        let ovf_template_source = resolve_optional(&scope_table, "ovf-template", ctx).await?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            grub_addr,
            ovf_template_source,
        })
    }
}

#[async_trait]
impl TransformImpl for VariantImageTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let grub_id = ctx
            .get(&self.grub_addr)
            .context(error::DependSnafu {
                addr: self.grub_addr.clone(),
            })?
            .get_unique_id(ctx)
            .await?;
        let ovf_template_digest = match &self.ovf_template_source {
            Some(s) => Some(s.get_unique_id().await?.digest().to_string()),
            None => None,
        };
        let binary_digest = twoliter_binary_digest();

        let inputs = HashInputs {
            arch: self.options.arch.as_deref(),
            sdk_digest: "",
            kit_digests: vec![],
            package_digests: vec![],
            guest_image_digests: vec![],
            ca_bundle_digest: None,
            tuf_root_digest: None,
            sbkeys_digest: None,
            ovf_template_digest,
            image_features: self.options.image_features.as_set(),
            resolved_layout: image_layout(&self.options),
            image_format: self.options.image_format,
            partition_plan: self.options.partition_plan,
            kernel_parameters: self.options.kernel_parameters.clone(),
            uefi_secure_boot: self.options.image_features.uefi_secure_boot,
            first_party_stack: self.options.image_features.first_party_stack,
            os_image_publish_size_gib: self.options.os_image_publish_size_gib,
            data_image_publish_size_gib: self.options.data_image_publish_size_gib,
            upstream_stage_digest: Some(grub_id.digest().to_string()),
            twoliter_binary_digest: binary_digest,
        };

        let digest = hash_image_inputs(&inputs)?;
        let id = Id::builder()
            .name(self.options.name.clone())
            .digest(digest)
            .version(self.options.version.clone())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(
            component = "transform",
            type = "variant-image",
            "id is calculated to be {id}"
        );
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(vec![self.grub_addr.clone()])
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        if let Some(s) = &self.ovf_template_source {
            s.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        if let Some(s) = &self.ovf_template_source {
            if !s.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Stage the grub dep. Its layers land at `deps/grub/<hint>`.
        let grub_dep_dir = Path::new("deps/grub");
        env.create_dir(grub_dep_dir).await?;
        let grub_dep = ctx.get(&self.grub_addr).context(error::DependSnafu {
            addr: self.grub_addr.clone(),
        })?;
        let grub_id = grub_dep.get_unique_id(ctx).await?;
        env.stage(
            ctx,
            ArtifactStageOptions::builder()
                .id(grub_id)
                .path(grub_dep_dir)
                .build(),
        )
        .await?;

        if let Some(source) = &self.ovf_template_source {
            let dir = Path::new("deps/ovf-template");
            env.create_dir(dir).await?;
            let id = source.get_unique_id().await?;
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

            let vfs = Vfs::new(&id, env, log).await?;

            let out = vfs.create_dir("out").await?;
            let out_path = out.path().to_path_buf();
            drop(out);

            // Read the disk-layout manifest to recover the resolved
            // layout + feature set + has_data flag. Single source of
            // truth for whether a `data.img.lz4` layer is emitted.
            let disk_manifest_bytes = vfs.read(Path::new("deps/grub/disk-layout.json")).await?;
            let disk_manifest: DiskLayoutManifest = serde_json::from_slice(&disk_manifest_bytes)
                .map_err(|source| error::Error::BuildStep {
                    step: "variant-image::stage".into(),
                    reason: format!("disk-layout manifest JSON: {source}"),
                })?;
            let resolved = disk_manifest.resolved_layout;
            let feature_set = disk_manifest.features.to_set();
            let has_data = disk_manifest.has_data_disk;

            // ---------- (i) compression ----------
            let os_disk = Path::new("deps/grub/os.img");
            let os_out = out_path.join("os.img.lz4");
            compress_image(&vfs, os_disk, &os_out, ArtifactSuffix::ImgLz4)
                .await
                .context(error::BrImageSnafu)?;

            // The disk stage never emitted a standalone `data.img`;
            // the data partition lives inside `os.img` under a
            // Unified plan, and under Split has a separate scratch we
            // (currently) don't ship as an `.lz4` — the plan says
            // "optional `data.img.lz4`" and today's disk stage does
            // not produce a full `data.img` for Split (only the
            // `data.raw` scratch, which is the DATA-A partition on
            // the OS disk). Preserve that behavior: no `data.img.lz4`
            // is emitted post-decomposition, matching pre-decomp
            // monolith which also fed `data_disk = None` on Split.
            //
            // If future work wants an `.lz4` of the DATA-B partition
            // on Split, add it here from `deps/grub/data.raw`.
            let _ = has_data;

            let boot_scratch = Path::new("deps/grub/boot.ext4");
            let boot_out = out_path.join("boot.ext4.lz4");
            compress_image(&vfs, boot_scratch, &boot_out, ArtifactSuffix::Ext4Lz4)
                .await
                .context(error::BrImageSnafu)?;

            let root_image = Path::new("deps/grub/root.erofs");
            let root_out = out_path.join("root.ext4.lz4");
            compress_image(&vfs, root_image, &root_out, ArtifactSuffix::Ext4Lz4)
                .await
                .context(error::BrImageSnafu)?;

            let verity_image = Path::new("deps/grub/verity.raw");
            let verity_out = out_path.join("verity.lz4");
            compress_image(&vfs, verity_image, &verity_out, ArtifactSuffix::VerityLz4)
                .await
                .context(error::BrImageSnafu)?;

            if matches!(self.options.image_format, ImageFormat::Vmdk) {
                let os_vmdk_path = out_path.join("os.img.vmdk");
                compress_image(&vfs, os_disk, &os_vmdk_path, ArtifactSuffix::Vmdk)
                    .await
                    .context(error::BrImageSnafu)?;
            }

            // ---------- boot-config passthrough ----------
            let bootconfig_src = Path::new("deps/grub/boot-config");
            let bootconfig_out = out_path.join("boot-config");
            vfs.copy(bootconfig_src, &bootconfig_out).await?;

            // ---------- SBOM passthrough ----------
            let sbom_src = Path::new("deps/grub/sbom.spdx.json");
            let sbom_out = out_path.join("sbom.spdx.json");
            if vfs.try_exists(sbom_src).await? {
                vfs.copy(sbom_src, &sbom_out).await?;
            } else {
                return Err(error::Error::BuildStep {
                    step: "variant-image::sbom".into(),
                    reason: "grub stage did not pass through sbom.spdx.json; check that \
                             the rootfs stage produced an SBOM layer"
                        .into(),
                }
                .into());
            }

            // ---------- (k) manifest.json ----------
            let (os_publish, data_publish) = resolved.publish_image_sizes_gib();
            let manifest = VariantManifest {
                name: self.options.name.clone(),
                version: self.options.version.clone(),
                arch,
                image_format: self.options.image_format,
                partition_plan: self.options.partition_plan,
                image_layout: resolved,
                image_features: feature_set,
                os_image_publish_size_gib: Some(os_publish),
                data_image_publish_size_gib: Some(data_publish),
                kernel_parameters: self.options.kernel_parameters.clone(),
            };
            let manifest_bytes = manifest.to_json_bytes().context(error::BrImageSnafu)?;
            let manifest_path = out_path.join("manifest.json");
            vfs.write(&manifest_path, &manifest_bytes).await?;

            // ---------- (l) OVA (vmdk only) ----------
            if matches!(self.options.image_format, ImageFormat::Vmdk) {
                if self.ovf_template_source.is_none() {
                    return Err(error::Error::BuildStep {
                        step: "ova".into(),
                        reason: "image-format=vmdk requires an `ovf-template` source scope \
                                 on the transform element; declare `source = { ovf-template \
                                 = [\"//addr\"] }` alongside the SDK addr."
                            .into(),
                    }
                    .into());
                }
                let ovf_template_path = pick_first_in(&vfs, "deps/ovf-template").await?.context(
                    error::BuildStepSnafu {
                        step: "ova".to_string(),
                        reason: "`ovf-template` source scope resolved but produced no file \
                                 under deps/ovf-template/ (empty source?)"
                            .to_string(),
                    },
                )?;
                let template_bytes = vfs.read(&ovf_template_path).await?;
                let template_text =
                    String::from_utf8(template_bytes).map_err(|e| error::Error::BuildStep {
                        step: "ova".into(),
                        reason: format!("OVF template is not valid UTF-8: {e}"),
                    })?;
                let os_vmdk = out_path.join("os.img.vmdk");
                // Pre-decomp monolith never emitted a `data.img.vmdk`
                // (since `data_disk` is `None` under Split at the
                // compress step): preserve that.
                let data_vmdk_opt: Option<std::path::PathBuf> = None;
                let ovf_out = out_path.join("bottlerocket.ovf");
                let rendered = crate::image::ovf::render_ovf(
                    &template_text,
                    "os.img.vmdk",
                    "",
                    os_publish as u32,
                    data_publish.max(0) as u32,
                    None,
                )
                .context(error::BrImageSnafu)?;
                vfs.write(&ovf_out, rendered.as_bytes()).await?;

                let ova_out = out_path.join("bottlerocket.ova");
                crate::image::ovf::assemble_ova(
                    &vfs,
                    &out_path,
                    &os_vmdk,
                    data_vmdk_opt.as_deref(),
                    &ovf_out,
                    &ova_out,
                )
                .await
                .context(error::BrImageSnafu)?;
            }

            // ---------- Emit the eight-layer artifact ----------
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            let mut layer_specs: Vec<(&str, &str)> =
                vec![("os.img.lz4", output_media_types::OS_IMG_LZ4)];
            if has_data {
                // Reserved: when the disk stage grows to emit a
                // standalone data.img.lz4 for Split, `has_data`
                // gates it here.
                let _ = output_media_types::DATA_IMG_LZ4;
            }
            layer_specs.extend([
                ("boot.ext4.lz4", output_media_types::BOOT_EXT4_LZ4),
                ("root.ext4.lz4", output_media_types::ROOT_EXT4_LZ4),
                ("verity.lz4", output_media_types::VERITY_LZ4),
                ("boot-config", output_media_types::BOOTCONFIG),
                ("manifest.json", output_media_types::MANIFEST_JSON),
                ("sbom.spdx.json", output_media_types::SBOM_SPDX),
            ]);

            for (file, media) in layer_specs {
                let entry = out_path.join(file);
                write_layer(
                    ctx,
                    env,
                    &entry,
                    media,
                    file,
                    "variant-image",
                    &mut artifact,
                )
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
