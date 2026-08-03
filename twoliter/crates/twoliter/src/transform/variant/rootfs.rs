//! `variant-rootfs` transform: install RPMs into a fresh rootfs, apply
//! SELinux labels, and copy guest-image artifacts into place.
//!
//! First stage of the four-transform variant pipeline. Produces two
//! layers:
//!
//! | Layer | Media type | Contents |
//! |---|---|---|
//! | 0 | `application/vnd.bottlerocket.variant.rootfs.tar` | tar of the fully-populated `root-mount/` |
//! | 1 | `application/vnd.bottlerocket.variant.rootfs.metadata+json` | [`RootfsMetadata`] JSON |
//!
//! **Inputs** (edo `depends`): packages, guest images (as
//! transform-to-transform addresses on the element). `packages` is
//! the *filtered* subset of `[variant.included-packages].packages`
//! that resolve to real in-repo `//packages/<name>/rpm` transforms;
//! kit-supplied names are absent from the transform graph and arrive
//! via the `kits` source scope below.
//!
//! **Source scopes** (element `source`):
//! - `default`   — SDK image (exactly one)
//! - `kits`      — zero or more `kit-image` sources
//! - `ca-bundle` — optional CA bundle for RPM installs
//! - `tuf-root`  — optional TUF root JSON
//!
//! **Hash inputs**: SDK digest, sorted kit + package + guest-image
//! digests, `ca-bundle` + `tuf-root` digests, arch, twoliter binary
//! digest. Explicitly *not* included: feature flags, layout, kernel
//! parameters, `sbkeys`, `ovf-template` (all downstream concerns).
//!
//! **`included_packages` name list** (on
//! [`VariantImageOptions`](super::options::VariantImageOptions)):
//! the full unfiltered list of package short names, carrying both
//! in-repo and kit-supplied names. This transform stores it on
//! `self.options.included_packages` but does not consume it — the
//! intended future consumer is a switch of
//! [`crate::image::rpm_install::install_rpms`] from `rpm -iv
//! <every-rpm-in-dir>` to `dnf install <name-list>`.

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
    RootfsMetadata, StageRootInputs,
    image::stage_root,
};

use super::error;
use super::hash::{HashInputs, hash_rootfs_inputs, twoliter_binary_digest};
use super::media_types::rootfs as rootfs_media;
use super::options::VariantImageOptions;
use super::sbom::generate_sbom_spdx;
use super::scope::{pick_first_in, resolve_file_contexts, resolve_optional};
use super::staging::{copy_guest_trees, write_layer};

/// Transform state for `variant-rootfs`. Identical shape to the
/// finalizer for the two source scopes it consumes — the `sbkeys` and
/// `ovf-template` scopes are grub/finalizer concerns and don't appear
/// here.
pub struct VariantRootfsTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantImageOptions,
    pub sdk_source: Source,
    /// Project kits as `kit-image` sources, drawn from the element's
    /// `"kits"` source scope. Order matches the scope's declaration
    /// order in the config emitter; determinism-critical uses (unique
    /// id hashing) sort digests explicitly.
    pub kits: Vec<Source>,
    pub ca_bundle_source: Option<Source>,
    pub tuf_root_source: Option<Source>,
}

#[async_trait]
impl FromElement for VariantRootfsTransform {
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

        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();

        let sdk_element = {
            let default_scope: Vec<_> =
                scope_table.get("default").cloned().unwrap_or_default();
            match default_scope.as_slice() {
                [single] => single.clone(),
                [] => return error::MissingSdkSourceSnafu.fail(),
                _ => return error::TooManySdkSourcesSnafu.fail(),
            }
        };
        let sdk_source = ctx.add_source(&sdk_element).await?;

        // Project kits arrive as `kit-image` sources under the
        // element's `"kits"` scope (populated by `Variant::to_edo`).
        // They're sources, not transforms, so they don't appear in
        // `depends()` and don't participate in the transform graph
        // shape — same wire-up as `PackageBuildTransform::new`'s
        // `kit` scope. An empty / missing scope is a valid config.
        //
        // The vendor's `resolve` emits the element without a
        // `platform` field (the resolver operates on the multi-arch
        // index), so we inject one here derived from the caller's
        // target arch. Without this the source falls back to the
        // *host* arch, which is wrong on any cross-compile.
        let target_arch = options.arch.as_deref().unwrap_or(std::env::consts::ARCH);
        let mut kits = Vec::new();
        if let Some(kit_list) = scope_table.get("kits") {
            for kit_element in kit_list {
                let scoped = crate::source::apply_target_arch(kit_element, target_arch);
                kits.push(ctx.add_source(&scoped).await?);
            }
        }

        let ca_bundle_source = resolve_optional(&scope_table, "ca-bundle", ctx).await?;
        let tuf_root_source = resolve_optional(&scope_table, "tuf-root", ctx).await?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            sdk_source,
            kits,
            ca_bundle_source,
            tuf_root_source,
        })
    }
}

#[async_trait]
impl TransformImpl for VariantRootfsTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let sdk_id = self.sdk_source.get_unique_id().await?;

        // Kit digests come from the resolved `Source` handles, not
        // from a transform lookup — kits at `//kits/<vendor>/<name>`
        // are `kit-image` sources (see `Twoliter::to_edo`). Sort the
        // digests after collection to keep the hash deterministic
        // regardless of declaration order; the source list itself
        // has no `Addr` to sort by post-resolution.
        let mut kit_digests = Vec::with_capacity(self.kits.len());
        for source in &self.kits {
            kit_digests.push(source.get_unique_id().await?.digest().to_string());
        }
        kit_digests.sort();

        let mut packages = self.options.packages.clone();
        packages.sort();
        let mut package_digests = Vec::with_capacity(packages.len());
        for addr in &packages {
            let dep = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            package_digests.push(dep.get_unique_id(ctx).await?.digest().to_string());
        }

        let mut guests = self.options.guest_images.clone();
        guests.sort();
        let mut guest_digests = Vec::with_capacity(guests.len());
        for addr in &guests {
            let dep = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            guest_digests.push(dep.get_unique_id(ctx).await?.digest().to_string());
        }

        async fn digest_of(source: &Option<Source>) -> TransformResult<Option<String>> {
            match source {
                Some(s) => Ok(Some(s.get_unique_id().await?.digest().to_string())),
                None => Ok(None),
            }
        }
        let ca_bundle_digest = digest_of(&self.ca_bundle_source).await?;
        let tuf_root_digest = digest_of(&self.tuf_root_source).await?;

        let binary_digest = twoliter_binary_digest();

        // Union `HashInputs`; only the rootfs-selecting subset is
        // read by `hash_rootfs_inputs`. Fields unused by this stage
        // are still populated for shape completeness — they're free.
        let inputs = HashInputs {
            arch: self.options.arch.as_deref(),
            sdk_digest: sdk_id.digest().as_str(),
            kit_digests,
            package_digests,
            guest_image_digests: guest_digests,
            ca_bundle_digest,
            tuf_root_digest,
            sbkeys_digest: None,
            ovf_template_digest: None,
            image_features: self.options.image_features.as_set(),
            resolved_layout: super::layout::image_layout(&self.options),
            image_format: self.options.image_format,
            partition_plan: self.options.partition_plan,
            kernel_parameters: self.options.kernel_parameters.clone(),
            uefi_secure_boot: self.options.image_features.uefi_secure_boot,
            first_party_stack: self.options.image_features.first_party_stack,
            os_image_publish_size_gib: self.options.os_image_publish_size_gib,
            data_image_publish_size_gib: self.options.data_image_publish_size_gib,
            upstream_stage_digest: None,
            twoliter_binary_digest: binary_digest,
        };

        let digest = hash_rootfs_inputs(&inputs)?;
        let id = Id::builder()
            .name(format!("{}-rootfs", self.options.name))
            .digest(digest)
            .version(self.options.version.clone())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(
            component = "transform",
            type = "variant-rootfs",
            "id is calculated to be {id}"
        );
        Ok(id)
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        // Kits are sources (see `FromElement::new`), so they never
        // appear here — only genuine transform-to-transform edges do.
        Ok(self
            .options
            .packages
            .iter()
            .chain(self.options.guest_images.iter())
            .cloned()
            .collect())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        self.sdk_source.cache(log, ctx.storage()).await?;
        for kit in &self.kits {
            kit.cache(log, ctx.storage()).await?;
        }
        for opt in [&self.ca_bundle_source, &self.tuf_root_source] {
            if let Some(s) = opt {
                s.cache(log, ctx.storage()).await?;
            }
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        if !self.sdk_source.is_cached(ctx.storage()).await? {
            return Ok(true);
        }
        for kit in &self.kits {
            if !kit.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        for opt in [&self.ca_bundle_source, &self.tuf_root_source] {
            if let Some(s) = opt {
                if !s.is_cached(ctx.storage()).await? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let rpm_root = Path::new("deps/rpms");
        env.create_dir(rpm_root).await?;
        env.create_dir(Path::new("deps/guest-images")).await?;

        let arch = self
            .options
            .arch
            .clone()
            .unwrap_or_else(|| std::env::consts::ARCH.to_string());

        // Stage each project kit as a `kit-image` source. Kits carry
        // their RPMs inside the pulled OCI image; the source
        // implementation is responsible for extracting them into
        // `deps/rpms/` so `stage_root` sees them alongside
        // package-transform RPMs.
        for kit in self.kits.iter() {
            let id = kit.get_unique_id().await?;
            trace!(
                component = "transform",
                type = "variant-rootfs",
                "staging kit source with id {id}"
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(rpm_root)
                    .build(),
            )
            .await?;
        }

        for addr in self.options.packages.iter() {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(
                component = "transform",
                type = "variant-rootfs",
                "staging rpm dep {addr} with id {id}"
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(rpm_root)
                    .build(),
            )
            .await?;
        }

        for addr in self.options.guest_images.iter() {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            let name = addr.to_id();
            let version_str = id
                .version()
                .map(|v| v.to_string())
                .unwrap_or_else(|| self.options.version.to_string());
            let guest_dir_str = format!("deps/guest-images/{arch}-{name}/{version_str}");
            let guest_dir = std::path::PathBuf::from(&guest_dir_str);
            env.create_dir(&guest_dir).await?;
            trace!(
                component = "transform",
                type = "variant-rootfs",
                "staging guest image {addr} with id {id} at {guest_dir_str}"
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(&guest_dir)
                    .build(),
            )
            .await?;
        }

        for (opt, scope_dir) in [
            (&self.ca_bundle_source, "deps/ca-bundle"),
            (&self.tuf_root_source, "deps/tuf-root"),
        ] {
            if let Some(source) = opt {
                let dir = Path::new(scope_dir);
                env.create_dir(dir).await?;
                let id = source.get_unique_id().await?;
                trace!(
                    component = "transform",
                    type = "variant-rootfs",
                    "staging optional source into {scope_dir} with id {id}"
                );
                env.stage(
                    ctx,
                    ArtifactStageOptions::builder().id(id).path(dir).build(),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            let root_mount = vfs.create_dir("root-mount").await?;
            let out = vfs.create_dir("out").await?;
            let root_mount_path = root_mount.path().to_path_buf();
            let out_path = out.path().to_path_buf();
            drop(root_mount);
            drop(out);

            // (d) stage_root: install every RPM under deps/rpms into
            // root-mount/, with optional ca-bundle + tuf-root
            // decoration.
            let rpm_dir = std::path::PathBuf::from("deps/rpms");
            let ca_bundle = pick_first_in(&vfs, "deps/ca-bundle").await?;
            let tuf_root = pick_first_in(&vfs, "deps/tuf-root").await?;
            stage_root(
                &vfs,
                &StageRootInputs {
                    root_mount: &root_mount_path,
                    package_dir: &rpm_dir,
                    ca_bundle: ca_bundle.as_deref(),
                    tuf_root: tuf_root.as_deref(),
                    selinux_file_contexts: None,
                },
            )
            .await
            .context(error::BrImageSnafu)?;

            // SELinux discovery + labeling. Splitting the coordinator
            // is cheaper than adding an install-only flag to
            // StageRootInputs (see the module doc for the finalizer).
            let file_contexts_path = resolve_file_contexts(&vfs, &root_mount_path).await?;
            if let Some(fc) = &file_contexts_path {
                crate::image::selinux::compute_labels(&vfs, &root_mount_path, fc)
                    .await
                    .context(error::BrImageSnafu)?;
            } else {
                tracing::warn!(
                    component = "transform",
                    type_ = "variant-rootfs",
                    "no SELinux file_contexts found under {}/usr/lib/selinux — image will \
                     boot unlabeled",
                    root_mount_path.display()
                );
            }

            // (e) guest images: copy each staged guest-image tree
            // into root-mount/local/guest-images/<arch-name>/<ver>/.
            if !self.options.guest_images.is_empty() {
                copy_guest_trees(
                    &vfs,
                    log,
                    Path::new("deps/guest-images"),
                    &root_mount_path,
                )
                .await?;
            }

            // Enumerate installed RPM names for the metadata layer.
            // The walk is recursive because staged RPMs live under a
            // nested layout (`Packages/<pkg>/[<arch>/]…`), not flat at
            // the root — mirroring the traversal in
            // `image::rpm_install::collect_rpms`. Non-fatal: an empty
            // list is acceptable when no RPMs were staged (tests).
            let mut packaged_rpms: Vec<String> = Vec::new();
            if vfs.try_exists(&rpm_dir).await? {
                let rpms = crate::image::vfs_ext::list_files_recursive(&vfs, &rpm_dir, "*.rpm")
                    .await
                    .context(error::BrImageSnafu)?;
                for path in rpms {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        packaged_rpms.push(name.to_string());
                    }
                }
                packaged_rpms.sort();
                packaged_rpms.dedup();
            }

            // SBOM: generate here (rootfs has `deps/rpms/`) rather
            // than in the finalizer, so the finalizer doesn't have to
            // re-stage every RPM just to produce a manifest of them.
            // The finalizer restages the SPDX file verbatim as its
            // `sbom.spdx.json` layer.
            let sbom_dir = out_path.join("sbom-work");
            let sbom_out = out_path.join("sbom.spdx.json");
            generate_sbom_spdx(&vfs, &rpm_dir, &sbom_dir, &sbom_out, "variant-rootfs::sbom")
                .await?;

            // Assemble outputs.
            //  Layer 0: rootfs tar (extract-on-stage).
            //  Layer 1: metadata JSON (opaque bytes).
            //  Layer 2: SBOM (SPDX JSON).
            let metadata = RootfsMetadata {
                file_contexts_path: file_contexts_path.clone(),
                packaged_rpms,
            };
            let metadata_bytes = metadata.to_json_bytes().context(error::BrImageSnafu)?;
            let metadata_path = out_path.join("metadata.json");
            vfs.write(&metadata_path, &metadata_bytes).await?;

            // Layer 0: archive root-mount/ into a tar file.
            let rootfs_tar_path = out_path.join("rootfs.tar");
            vfs.command(
                "rootfs-tar",
                "tar",
                &[
                    "-cf",
                    rootfs_tar_path.to_string_lossy().as_ref(),
                    "--sort=name",
                    "--numeric-owner",
                    "-C",
                    root_mount_path.to_string_lossy().as_ref(),
                    ".",
                ],
            )
            .await?;

            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            // Layer 0: rootfs tar. `MediaType::Custom(...)` — not
            // `MediaType::Tar(...)` — because the downstream stage
            // needs to control extraction: `env.stage` on a `Tar`
            // media type auto-extracts, but we want the raw bytes so
            // we can extract to a specific path (`root-mount/`) using
            // a subprocess to preserve permissions + selinux xattrs.
            for (src, media, hint) in [
                (&rootfs_tar_path, rootfs_media::ROOTFS_TAR, "rootfs.tar"),
                (&metadata_path, rootfs_media::METADATA_JSON, "metadata.json"),
                (&sbom_out, rootfs_media::SBOM_SPDX, "sbom.spdx.json"),
            ] {
                write_layer(ctx, env, src, media, hint, "variant-rootfs", &mut artifact)
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
