//! `variant-image` transform: build a Bottlerocket variant disk image
//! (`os.img.lz4`, `boot.ext4.lz4`, `root.ext4.lz4`, `verity.lz4`,
//! `boot-config`, `manifest.json`, `sbom.spdx.json`, optionally
//! `data.img.lz4`) from an SDK, kits, extra RPMs, and optional guest
//! images.
//!
//! Structure mirrors `kit.rs` and `package.rs`: `FromElement` reads
//! configuration + resolved sources, `TransformImpl` walks the
//! prepare/stage/transform lifecycle, and every subprocess is issued
//! through `Vfs::command`. Every primitive lives in `br-image`; this
//! module is orchestration.
//!
//! **Source-scope contract.** The transform reads `element.source` as
//! a keyed [`SourceMap`](edo::context::element::SourceMap) with the
//! following scope names (any subset may be present; only `default` is
//! required):
//!
//! | Scope | Cardinality | Staged at | Consumed by |
//! |---|---|---|---|
//! | `default`      | exactly 1 addr | (SDK image) | environment base |
//! | `ca-bundle`    | 0 or 1 addr    | `deps/ca-bundle/ca-bundle.crt` | [`br_image::stage_root`] |
//! | `tuf-root`     | 0 or 1 addr    | `deps/tuf-root/root.json` | [`br_image::stage_root`] |
//! | `sbkeys`       | 0 or 1 addr    | `deps/sbkeys/…` | [`br_image::secure_boot::Sbsetup`], `install_grub_uefi` |
//! | `ovf-template` | 0 or 1 addr    | `deps/ovf-template/…` | [`br_image::ovf::assemble_ova`] |
//!
//! **Twoliter binary digest.** [`get_unique_id`] folds in the SHA256
//! of the running twoliter binary (via `std::env::current_exe()`,
//! memoized once per process). This means a twoliter release that
//! changes `br-image` invalidates every variant's cache — which is
//! correct.
//!
//! **SELinux file_contexts.** After `stage_root` installs the SELinux
//! policy RPM, the file_contexts path is discovered under
//! `root-mount/usr/lib/selinux/*/contexts/files/file_contexts` and
//! passed to `mkfs.erofs`. Variants without a policy RPM fall back to
//! an empty file_contexts and emit a `tracing::warn!`.
//!
//! **Media types.** Output layer media-type strings use
//! [`edo::storage::MediaType::Custom`] with the exact strings from
//! `doc/transforms/variant-image.md::Outputs`. Compression is
//! already applied to the bytes on disk by `compress_image`, so the
//! layer's compression is [`edo::storage::Compression::None`].

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{
        Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType,
    },
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt};
use std::path::Path;
use tracing::trace;

use br_image::{
    Arch, ArtifactSuffix, ImageFeature, ImageFeatures, ImageFormat, ImageLayout, PartitionPlan,
    StageRootInputs, VariantManifest,
    bootconfig::assemble_bootconfig,
    compress::compress_image,
    grub::{
        GrubCfgOptions, GrubInstall, GrubUefiInstall, install_grub_bios, install_grub_uefi,
        render_grub_cfg,
    },
    guest::copy_guest_artifacts,
    image::{mkfs_data_ext4, mkfs_data_xfs, mkfs_root_erofs, stage_root},
    layout::{resolved_image_layout, validate_image_features},
    partition::{
        BOOT_SCALE_FACTOR, DiskLayout, HASH_SCALE_FACTOR, Part, apply_partition_layout,
        compute_partition_layout,
    },
    sbom::generate_sbom,
    verity::{VERITY_DATA_BLOCK_SIZE, generate_verity_root},
    vfs_ext::{list_dir_all, size_bytes},
};
use std::collections::BTreeSet;

/// Wire-format for the `variant-image` transform's TOML block.
///
/// Field-for-field port of the TOML surface in
/// `doc/transforms/variant-image.md`. Every non-required field carries
/// a `#[serde(default)]` so a minimal `variant.toml` is accepted.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct VariantImageOptions {
    pub name: String,
    pub version: semver::Version,
    #[serde(default)]
    pub arch: Option<String>,

    /// Image format on disk. Defaults to `raw`.
    #[serde(default = "default_image_format")]
    pub image_format: ImageFormat,

    /// Partition-plan; defaults to `split` (matches
    /// [`br_image::DEFAULT_PARTITION_PLAN`]).
    #[serde(default = "default_partition_plan")]
    pub partition_plan: PartitionPlan,

    #[serde(default)]
    pub os_image_size_gib: Option<u16>,
    #[serde(default)]
    pub data_image_size_gib: Option<u16>,
    #[serde(default)]
    pub os_image_publish_size_gib: Option<u16>,
    #[serde(default)]
    pub data_image_publish_size_gib: Option<u16>,

    /// Kernel-parameters — order-sensitive.
    #[serde(default)]
    pub kernel_parameters: Vec<String>,

    /// The `[variant.image-features]` sub-table.
    #[serde(default)]
    pub image_features: SerializableFeatures,

    /// The `[variant.included-packages]` sub-table.
    #[serde(default)]
    pub included_packages: IncludedPackages,

    /// The set of kits (as edo transform addresses). The
    /// config-generation layer resolves each `[[variant.kit]]` entry
    /// to its transform address and passes them here.
    #[serde(default)]
    pub kits: Vec<Addr>,

    /// Extra RPM packages (transform addresses, one per rpm build).
    #[serde(default)]
    pub packages: Vec<Addr>,

    /// Guest-image transforms whose outputs must be embedded in the
    /// rootfs at `/local/guest-images/<arch>-<name>/<version>/`.
    #[serde(default)]
    pub guest_images: Vec<Addr>,
}

fn default_image_format() -> ImageFormat {
    ImageFormat::Raw
}

fn default_partition_plan() -> PartitionPlan {
    br_image::DEFAULT_PARTITION_PLAN
}

/// Wire-format for the `[variant.included-packages]` block.
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct IncludedPackages {
    #[serde(default)]
    pub packages: Vec<String>,
}

/// Deserializable feature-bitset mirroring [`ImageFeatures`] with a
/// [`Default`] impl that matches the doc's defaults
/// (`first-party-stack = true`, every other feature off).
#[derive(Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", default)]
pub struct SerializableFeatures {
    pub grub_set_private_var: bool,
    pub systemd_networkd: bool,
    pub xfs_data_partition: bool,
    pub erofs_root_partition: bool,
    pub uefi_secure_boot: bool,
    pub fips: bool,
    pub in_place_updates: bool,
    pub host_containers: bool,
    pub external_kmod_development: bool,
    pub encrypted_storage: bool,
    pub first_party_stack: bool,
}

impl Default for SerializableFeatures {
    fn default() -> Self {
        Self {
            grub_set_private_var: false,
            systemd_networkd: false,
            xfs_data_partition: false,
            erofs_root_partition: false,
            uefi_secure_boot: false,
            fips: false,
            in_place_updates: false,
            host_containers: false,
            external_kmod_development: false,
            encrypted_storage: false,
            first_party_stack: true,
        }
    }
}

impl From<&SerializableFeatures> for ImageFeatures {
    fn from(s: &SerializableFeatures) -> Self {
        ImageFeatures {
            grub_set_private_var: s.grub_set_private_var,
            systemd_networkd: s.systemd_networkd,
            xfs_data_partition: s.xfs_data_partition,
            erofs_root_partition: s.erofs_root_partition,
            uefi_secure_boot: s.uefi_secure_boot,
            fips: s.fips,
            in_place_updates: s.in_place_updates,
            host_containers: s.host_containers,
            external_kmod_development: s.external_kmod_development,
            encrypted_storage: s.encrypted_storage,
            first_party_stack: s.first_party_stack,
        }
    }
}

impl SerializableFeatures {
    fn as_bitset(&self) -> ImageFeatures {
        self.into()
    }

    /// Sorted set of enabled features. Used both by the manifest and
    /// as an input to the deterministic unique-id hash.
    fn as_set(&self) -> BTreeSet<ImageFeature> {
        self.as_bitset().to_set()
    }
}

/// Transform state: options plus the resolved SDK source. Kits,
/// packages, and guest images are transform-to-transform dependencies
/// (like `PackageBuildTransform::depends`) so they live in `options`.
pub struct VariantImageTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantImageOptions,
    /// Resolved SDK image source. Required, exactly one.
    pub sdk_source: Source,
    /// Optional CA bundle source (`ca-bundle` scope). Staged at
    /// `deps/ca-bundle/`; the first file therein is passed to
    /// [`br_image::stage_root`]'s `ca_bundle` field.
    pub ca_bundle_source: Option<Source>,
    /// Optional TUF root source (`tuf-root` scope). Staged at
    /// `deps/tuf-root/`; the first file therein is passed to
    /// [`br_image::stage_root`]'s `tuf_root` field.
    pub tuf_root_source: Option<Source>,
    /// Optional secure-boot keys directory (`sbkeys` scope). Staged
    /// at `deps/sbkeys/`; consumed by
    /// [`br_image::secure_boot::Sbsetup`] and every signing helper
    /// invoked by [`br_image::grub::install_grub_uefi`].
    pub sbkeys_source: Option<Source>,
    /// Optional OVF template source (`ovf-template` scope). Staged at
    /// `deps/ovf-template/`; the first file therein is used as the
    /// template for [`br_image::ovf::render_ovf`] on the VMDK path.
    pub ovf_template_source: Option<Source>,
}

/// Compute a stable content-address digest for the running twoliter
/// binary. Memoized behind a [`std::sync::OnceLock`] so we hash the
/// executable once per process. On failure (unable to locate the
/// executable, permission error, or read failure) falls back to
/// `CARGO_PKG_NAME + '-' + CARGO_PKG_VERSION` with a `tracing::warn!`
/// so cache-invalidation degrades to the version-string level rather
/// than failing the build.
///
/// See `doc/transforms/variant-image.md::Unique-id` for why this must
/// participate in `get_unique_id`: a twoliter release that changes
/// `br-image` invalidates every variant's cache.
fn twoliter_binary_digest() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        match compute_twoliter_binary_digest() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    component = "transform",
                    type_ = "variant-image",
                    "unable to hash twoliter binary ({e}); falling back to version string. \
                     Cache invalidation will only trigger on version bumps until this is \
                     resolved."
                );
                format!(
                    "{}-{}",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                )
            }
        }
    })
}

/// Hash the twoliter binary's on-disk bytes as sha256, hex-encoded.
/// Called at most once per process (via [`twoliter_binary_digest`]).
fn compute_twoliter_binary_digest() -> Result<String, std::io::Error> {
    let path = std::env::current_exe()?;
    let bytes = std::fs::read(&path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(base16::encode_lower(hasher.finalize().as_slice()))
}

/// Return the first regular file directly under `dir` inside `vfs`,
/// or `None` when the directory is absent or empty.
///
/// Used to pick the single file each optional single-source scope
/// (`ca-bundle`, `tuf-root`, `ovf-template`) stages into its
/// scope-specific `deps/<scope>/` directory. Sources may name their
/// file anything (`ca-bundle.crt`, `bundle.pem`, `certs.crt`, …) so
/// we don't assume a fixed leaf name.
async fn pick_first_in(
    vfs: &Vfs,
    dir: &str,
) -> Result<Option<std::path::PathBuf>, error::Error> {
    let dir_path = Path::new(dir);
    if !try_exists(vfs, dir_path).await? {
        return Ok(None);
    }
    let files = br_image::vfs_ext::list_files(vfs, dir_path, "*")
        .await
        .context(error::BrImageSnafu)?;
    Ok(files.into_iter().next())
}

/// Wrap `Vfs::try_exists` in the transform's `error::Error` type. The
/// raw `EnvironmentError` doesn't have a direct `From` into
/// `error::Error`, so route through `br_image::Error` (whose `Env`
/// variant is `#[snafu(transparent)]` and provides the conversion).
async fn try_exists(vfs: &Vfs, path: &Path) -> Result<bool, error::Error> {
    vfs.try_exists(path)
        .await
        .map_err(br_image::Error::from)
        .context(error::BrImageSnafu)
}

/// Locate the SELinux `file_contexts` path inside an already-installed
/// rootfs. First tries the canonical `targeted` policy path; falls
/// back to a glob across every policy directory. Returns `None` when
/// no policy is present (the variant did not install an SELinux
/// policy RPM), which is a valid — if unusual — configuration.
async fn resolve_file_contexts(
    vfs: &Vfs,
    root_mount: &Path,
) -> Result<Option<std::path::PathBuf>, error::Error> {
    let targeted = root_mount.join("usr/lib/selinux/targeted/contexts/files/file_contexts");
    if try_exists(vfs, &targeted).await? {
        return Ok(Some(targeted));
    }
    // Fall back: walk `usr/lib/selinux/*/contexts/files/file_contexts`.
    // `list_files` is non-recursive, so we enumerate policy dirs
    // ourselves.
    let selinux_root = root_mount.join("usr/lib/selinux");
    if !try_exists(vfs, &selinux_root).await? {
        return Ok(None);
    }
    let policies = br_image::vfs_ext::list_dir_all(vfs, &selinux_root)
        .await
        .context(error::BrImageSnafu)?;
    for policy in policies {
        let candidate = policy.join("contexts/files/file_contexts");
        if try_exists(vfs, &candidate).await? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// `dd` a scratch image into `disk` at the given MiB offset, going
/// through [`br_image::image::dd_partition`] so the "one subprocess
/// per step" rule is preserved.
async fn copy_partition(
    vfs: &Vfs,
    src: &Path,
    disk: &Path,
    offset_mib: u32,
) -> Result<(), error::Error> {
    // dd_partition lives in br_image::image but is public via its
    // module path; import inline to keep the top of the file tidy.
    br_image::image::dd_partition(vfs, src, disk, offset_mib)
        .await
        .context(error::BrImageSnafu)
}

impl VariantImageTransform {
    fn partition_entry<'a>(
        &self,
        disk_layout: &'a DiskLayout,
        part: Part,
    ) -> Result<&'a br_image::partition::PartitionEntry, error::Error> {
        disk_layout
            .os_disk
            .get(part)
            .ok_or_else(|| error::Error::BuildStep {
                step: format!("partition-lookup:{}", part.short_name()),
                reason: format!("{} absent from computed OS-disk layout", part.short_name()),
            })
    }

    fn image_layout(&self) -> ImageLayout {
        // `ImageLayout::new` (in br-image) takes only os/data/hint/plan.
        // The publish-hint doubles as the `os_image_publish_size` seed;
        // if the caller specified a hint we use it, otherwise we fall
        // back to br-image's default.
        let hint = self
            .options
            .os_image_publish_size_gib
            .or(self.options.data_image_publish_size_gib)
            .unwrap_or(br_image::DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB.0);
        let data = self
            .options
            .data_image_size_gib
            .unwrap_or(br_image::DEFAULT_DATA_IMAGE_SIZE_GIB.0);
        ImageLayout::new(
            self.options.os_image_size_gib,
            data,
            hint,
            self.options.partition_plan,
        )
    }
}

/// Every input to the deterministic unique-id hash, in the shape
/// documented in `doc/transforms/variant-image.md::Unique-id`.
///
/// Extracted from [`VariantImageTransform::get_unique_id`] so the
/// hashing itself can be exercised by unit tests without a live
/// scheduler [`Handle`] (real dep-id resolution requires one).
#[derive(Debug, Clone)]
struct HashInputs<'a> {
    arch: Option<&'a str>,
    sdk_digest: &'a str,
    kit_digests: Vec<String>,
    package_digests: Vec<String>,
    guest_image_digests: Vec<String>,
    ca_bundle_digest: Option<String>,
    tuf_root_digest: Option<String>,
    sbkeys_digest: Option<String>,
    ovf_template_digest: Option<String>,
    image_features: BTreeSet<ImageFeature>,
    resolved_layout: ImageLayout,
    image_format: ImageFormat,
    kernel_parameters: Vec<String>,
    twoliter_binary_digest: &'a str,
}

fn hash_static_inputs(inputs: &HashInputs<'_>) -> Result<String, error::Error> {
    let mut hash = Sha256::new();
    hash.update(b"variant-image");
    hash.update(inputs.arch.unwrap_or("").as_bytes());
    hash.update(inputs.sdk_digest.as_bytes());
    for d in &inputs.kit_digests {
        hash.update(d.as_bytes());
    }
    for d in &inputs.package_digests {
        hash.update(d.as_bytes());
    }
    for d in &inputs.guest_image_digests {
        hash.update(d.as_bytes());
    }
    // Optional source-scope digests. Each is folded in with a
    // scope-tagged prefix so `ca-bundle=abc` and `tuf-root=abc` don't
    // collapse to the same hash contribution.
    for (scope, maybe_digest) in [
        ("ca-bundle=", &inputs.ca_bundle_digest),
        ("tuf-root=", &inputs.tuf_root_digest),
        ("sbkeys=", &inputs.sbkeys_digest),
        ("ovf-template=", &inputs.ovf_template_digest),
    ] {
        if let Some(d) = maybe_digest {
            hash.update(scope.as_bytes());
            hash.update(d.as_bytes());
            hash.update(&[0u8]);
        }
    }
    hash.update(
        serde_json::to_vec(&inputs.image_features).context(error::SerializationSnafu)?,
    );
    hash.update(
        serde_json::to_vec(&inputs.resolved_layout).context(error::SerializationSnafu)?,
    );
    hash.update(inputs.image_format.to_string().as_bytes());
    for kp in &inputs.kernel_parameters {
        hash.update(kp.as_bytes());
        // Separator so ["ab", "c"] != ["a", "bc"].
        hash.update(&[0u8]);
    }
    // Content-address of the running twoliter binary — a release that
    // changes `br-image` invalidates every variant's cache. Computed
    // once per process via [`twoliter_binary_digest`].
    hash.update(inputs.twoliter_binary_digest.as_bytes());
    Ok(base16::encode_lower(hash.finalize().as_slice()))
}

/// Media-type strings for each output artifact, per
/// `doc/transforms/variant-image.md::Outputs`. Kept as `&'static str`s
/// so the transform body reads like a table.
mod output_media_types {
    pub const OS_IMG_LZ4: &str = "application/vnd.bottlerocket.image.os.lz4";
    pub const DATA_IMG_LZ4: &str = "application/vnd.bottlerocket.image.data.lz4";
    pub const BOOT_EXT4_LZ4: &str = "application/vnd.bottlerocket.image.boot.ext4.lz4";
    pub const ROOT_EXT4_LZ4: &str = "application/vnd.bottlerocket.image.root.ext4.lz4";
    pub const VERITY_LZ4: &str = "application/vnd.bottlerocket.image.verity.lz4";
    pub const BOOTCONFIG: &str = "application/vnd.bottlerocket.image.bootconfig";
    pub const MANIFEST_JSON: &str = "application/vnd.bottlerocket.image.manifest+json";
    pub const SBOM_SPDX: &str = "application/spdx+json";
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

        // Read the keyed source-scope table. Every optional scope
        // holds at most one addr; `default` (the SDK) holds exactly
        // one.
        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();

        // SDK: exactly one addr on the "default" scope. Fall back to
        // the whole-table flatten for backward compatibility with
        // TOML that uses `source = "//sdk-addr"` (which binds to
        // "default").
        let sdk_element = {
            let default_scope: Vec<_> = scope_table
                .get("default")
                .cloned()
                .unwrap_or_default();
            match default_scope.as_slice() {
                [single] => single.clone(),
                [] => return error::MissingSdkSourceSnafu.fail(),
                _ => return error::TooManySdkSourcesSnafu.fail(),
            }
        };
        let sdk_source = ctx.add_source(&sdk_element).await?;

        // Optional keyed scopes. Each accepts at most one source addr;
        // multiple entries are a config error surfacing as
        // `TooMany<scope>Sources`.
        async fn resolve_optional(
            scope_table: &std::collections::BTreeMap<String, Vec<Element>>,
            scope: &str,
            ctx: &Context,
        ) -> Result<Option<Source>, error::Error> {
            let entries = match scope_table.get(scope) {
                Some(v) => v,
                None => return Ok(None),
            };
            match entries.as_slice() {
                [single] => Ok(Some(ctx.add_source(single).await?)),
                [] => Ok(None),
                _ => Err(error::Error::TooManyScopeSources {
                    scope: scope.to_string(),
                    count: entries.len(),
                }),
            }
        }
        let ca_bundle_source = resolve_optional(&scope_table, "ca-bundle", ctx).await?;
        let tuf_root_source = resolve_optional(&scope_table, "tuf-root", ctx).await?;
        let sbkeys_source = resolve_optional(&scope_table, "sbkeys", ctx).await?;
        let ovf_template_source = resolve_optional(&scope_table, "ovf-template", ctx).await?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            sdk_source,
            ca_bundle_source,
            tuf_root_source,
            sbkeys_source,
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
        // (a) SDK.
        let sdk_id = self.sdk_source.get_unique_id().await?;

        // (b) Each transform dep — kits, packages, guest images —
        // sorted by addr for determinism.
        let mut kits = self.options.kits.clone();
        kits.sort();
        let mut kit_digests = Vec::with_capacity(kits.len());
        for addr in &kits {
            let dep = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            kit_digests.push(dep.get_unique_id(ctx).await?.digest().to_string());
        }

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

        let feature_set = self.options.image_features.as_set();
        let resolved = resolved_image_layout(
            &self.image_layout(),
            &feature_set,
            Some(&self.options.image_format),
        );

        // Optional source-scope digests — each source's unique-id
        // digest folded in independently. `None` when the scope
        // isn't declared on the element, which is a semantic
        // difference from "declared with a zero-length digest".
        async fn digest_of(source: &Option<Source>) -> TransformResult<Option<String>> {
            match source {
                Some(s) => Ok(Some(s.get_unique_id().await?.digest().to_string())),
                None => Ok(None),
            }
        }
        let ca_bundle_digest = digest_of(&self.ca_bundle_source).await?;
        let tuf_root_digest = digest_of(&self.tuf_root_source).await?;
        let sbkeys_digest = digest_of(&self.sbkeys_source).await?;
        let ovf_template_digest = digest_of(&self.ovf_template_source).await?;

        let binary_digest = twoliter_binary_digest();

        let inputs = HashInputs {
            arch: self.options.arch.as_deref(),
            sdk_digest: sdk_id.digest().as_str(),
            kit_digests,
            package_digests,
            guest_image_digests: guest_digests,
            ca_bundle_digest,
            tuf_root_digest,
            sbkeys_digest,
            ovf_template_digest,
            image_features: feature_set,
            resolved_layout: resolved,
            image_format: self.options.image_format,
            kernel_parameters: self.options.kernel_parameters.clone(),
            twoliter_binary_digest: binary_digest,
        };

        let digest = hash_static_inputs(&inputs)?;
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
        Ok(self
            .options
            .kits
            .iter()
            .chain(self.options.packages.iter())
            .chain(self.options.guest_images.iter())
            .cloned()
            .collect())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        self.sdk_source.cache(log, ctx.storage()).await?;
        for opt in [
            &self.ca_bundle_source,
            &self.tuf_root_source,
            &self.sbkeys_source,
            &self.ovf_template_source,
        ] {
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
        for opt in [
            &self.ca_bundle_source,
            &self.tuf_root_source,
            &self.sbkeys_source,
            &self.ovf_template_source,
        ] {
            if let Some(s) = opt {
                if !s.is_cached(ctx.storage()).await? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Overlay layout mirrors the doc's `Environment`:
        //   deps/rpms/         — every kit + package RPM (local dnf repo)
        //   deps/guest-images/ — per (arch, name, version) subdirs
        //
        // Guest artifacts land at their staged addr's `path_hint`,
        // which for a variant/EIF transform is inherited from the
        // upstream artifact. We simply create the umbrella directory;
        // the ArtifactStageOptions per-guest below decides the leaf.
        let rpm_root = Path::new("deps/rpms");
        env.create_dir(rpm_root).await?;
        env.create_dir(Path::new("deps/guest-images")).await?;

        let arch = self
            .options
            .arch
            .clone()
            .unwrap_or_else(|| std::env::consts::ARCH.to_string());

        for addr in self.options.kits.iter().chain(self.options.packages.iter()) {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(
                component = "transform",
                type = "variant-image",
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
                type = "variant-image",
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

        // Optional single-source scopes. Each unpacks into a scoped
        // subdirectory so `transform()` can pick the first file
        // therein without worrying about how the source was named.
        for (opt, scope_dir) in [
            (&self.ca_bundle_source, "deps/ca-bundle"),
            (&self.tuf_root_source, "deps/tuf-root"),
            (&self.sbkeys_source, "deps/sbkeys"),
            (&self.ovf_template_source, "deps/ovf-template"),
        ] {
            if let Some(source) = opt {
                let dir = Path::new(scope_dir);
                env.create_dir(dir).await?;
                let id = source.get_unique_id().await?;
                trace!(
                    component = "transform",
                    type = "variant-image",
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
            let arch = parse_arch(self.options.arch.as_deref())?;
            let feature_set = self.options.image_features.as_set();
            let features_bitset: ImageFeatures = (&self.options.image_features).into();
            let resolved = resolved_image_layout(
                &self.image_layout(),
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

            // ------------------------------------------------------
            // Directory layout inside the container.
            //
            //   out/       — final compressed outputs (streamed to layers)
            //   work/      — raw disk images and intermediate filesystems
            //   root-mount/ — rootfs mount populated by `stage_root`
            //   scratch/   — per-partition scratch images used by mkfs
            //
            // Everything is a `vfs.command` call downstream; no host
            // filesystem is touched.
            // ------------------------------------------------------
            let out = vfs.create_dir("out").await?;
            let work = vfs.create_dir("work").await?;
            let root_mount = vfs.create_dir("root-mount").await?;
            let scratch = vfs.create_dir("scratch").await?;

            let out_path = out.path().to_path_buf();
            let work_path = work.path().to_path_buf();
            let root_mount_path = root_mount.path().to_path_buf();
            let scratch_path = scratch.path().to_path_buf();
            drop(out);
            drop(work);
            drop(root_mount);
            drop(scratch);

            // ------------------------------------------------------
            // Step (a): partition math.
            // ------------------------------------------------------
            let disk_layout: DiskLayout =
                compute_partition_layout(&resolved, &features_bitset).context(error::BrImageSnafu)?;

            // ------------------------------------------------------
            // Step (b): truncate + sgdisk on the OS disk (and data
            // disk when the plan is Split).
            // ------------------------------------------------------
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

            let data_disk = if let Some(data_layout) = &disk_layout.data_disk {
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
                Some(path)
            } else {
                None
            };

            // ------------------------------------------------------
            // Step (d): stage the rootfs.
            //
            // CA bundle and TUF root are pulled from the `ca-bundle`
            // and `tuf-root` source scopes when present. Each unpacked
            // into `deps/<scope>/`; we pick the first file therein.
            // Absent scopes stay `None` (stage_root skips those steps).
            //
            // SELinux file_contexts is discovered on the *installed*
            // rootfs by [`resolve_file_contexts`] below because that
            // path is provided by the policy RPM, which stage_root's
            // install step lands. To make the labeling step actually
            // run, we call stage_root twice: once with
            // `selinux_file_contexts=None` to install RPMs, then
            // discover the file_contexts path, then invoke
            // [`br_image::selinux::compute_labels`] directly with the
            // discovered path. (Splitting the coordinator is cheaper
            // than adding an "install-only" flag to StageRootInputs.)
            // ------------------------------------------------------
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

            // Discover the SELinux file_contexts under the installed
            // rootfs. First try the canonical `targeted` policy path;
            // fall back to a glob if the variant ships a different
            // policy name. Absent policy ⇒ empty file (rootfs boots
            // unlabeled; `tracing::warn!`).
            let file_contexts_path = resolve_file_contexts(&vfs, &root_mount_path).await?;
            if let Some(fc) = &file_contexts_path {
                br_image::selinux::compute_labels(&vfs, &root_mount_path, fc)
                    .await
                    .context(error::BrImageSnafu)?;
            } else {
                tracing::warn!(
                    component = "transform",
                    type_ = "variant-image",
                    "no SELinux file_contexts found under {}/usr/lib/selinux — image will \
                     boot unlabeled",
                    root_mount_path.display()
                );
            }

            // ------------------------------------------------------
            // Step (e): guest images.
            //
            // Every guest artifact staged by `stage()` under
            // `deps/guest-images/` is copied into the rootfs at
            // `/local/guest-images/`. `copy_guest_artifacts` iterates
            // the source directory non-recursively; we call it once
            // per per-guest subdirectory so each variant's artifacts
            // keep their `<arch>-<name>/<version>/` layout.
            // ------------------------------------------------------
            if !self.options.guest_images.is_empty() {
                let umbrella = std::path::PathBuf::from("deps/guest-images");
                if vfs.try_exists(&umbrella).await? {
                    // `stage()` creates one subdir per guest as
                    // `deps/guest-images/<arch>-<name>/<version>/`.
                    // Walk each `<arch>-<name>` level, then each
                    // `<version>` under it, and copy that leaf's
                    // artifacts into the rootfs preserving the same
                    // `local/guest-images/<arch>-<name>/<version>/`
                    // shape the doc's `Environment` section calls out.
                    for arch_name_dir in
                        list_dir_all(&vfs, &umbrella).await.context(error::BrImageSnafu)?
                    {
                        let arch_name = match arch_name_dir.file_name() {
                            Some(n) => n.to_string_lossy().to_string(),
                            None => continue,
                        };
                        for version_dir in list_dir_all(&vfs, &arch_name_dir)
                            .await
                            .context(error::BrImageSnafu)?
                        {
                            let version = match version_dir.file_name() {
                                Some(n) => n.to_string_lossy().to_string(),
                                None => continue,
                            };
                            let dst = root_mount_path
                                .join(format!("local/guest-images/{arch_name}/{version}"));
                            copy_guest_artifacts(&vfs, &version_dir, &dst)
                                .await
                                .context(error::BrImageSnafu)?;
                        }
                    }
                }
            }

            // ------------------------------------------------------
            // Step (c) + (f): mkfs on each partition and verity.
            //
            // We assemble the raw partition contents into scratch
            // files (one per partition), then `dd` them into the OS
            // disk at the offsets `apply_partition_layout` reserved.
            // The `mkfs_data_*` helpers already fold the `dd_partition`
            // step into their body, but the root-erofs + verity path
            // is composed here because it needs the erofs image size
            // to compute verity's data-block count.
            //
            // mkfs.erofs `--file-contexts` uses the same path we
            // discovered above. When the rootfs lacks a policy RPM we
            // fall back to an empty contexts file (already logged as
            // a warning during stage_root).
            // ------------------------------------------------------
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

            // Compute block-aligned root content size for verity.
            let root_bytes = size_bytes(&vfs, &root_image).await.context(error::BrImageSnafu)?;
            let block = VERITY_DATA_BLOCK_SIZE as u64;
            let root_content_bytes = (root_bytes / block) * block;
            // Scale factors are applied to the *resolved* OS image
            // size (post `resolved_image_layout`), so stripped-stack
            // and EIF variants pick up the same reduced base the
            // partition layout uses. Using `self.options.os_image_size_gib`
            // here would race the two computations against each other.
            let resolved_os_gib = resolved.os_image_size_gib().0 as u32;
            let hash_mib = resolved_os_gib * HASH_SCALE_FACTOR;
            let verity_image = work_path.join("verity.raw");
            let verity = generate_verity_root(
                &vfs,
                &root_image,
                &verity_image,
                hash_mib,
                root_content_bytes,
            )
            .await
            .context(error::BrImageSnafu)?;

            // Boot partition: mkfs.ext4 over the (per-arch) contents
            // of `root-mount/boot`. We reserve a scratch image and let
            // `mkfs_data_ext4` seed it from the boot directory before
            // `dd`-ing it into the OS disk. Skip the SELinux debugfs
            // patch (empty script is a valid no-op) — a real build
            // will supply the labeled script once the SELinux gap
            // above is closed.
            let boot_mib = resolved_os_gib * BOOT_SCALE_FACTOR;
            let boot_scratch = scratch_path.join("boot.ext4");
            let boot_mount = root_mount_path.join("boot");
            let boot_entry = disk_layout
                .os_disk
                .get(Part::BootA)
                .ok_or_else(|| error::Error::BuildStep {
                    step: "boot-partition-lookup".into(),
                    reason: "BOOT-A absent from computed OS-disk layout".into(),
                })?;
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

            // Data partition: mkfs.xfs or mkfs.ext4 depending on
            // features. The data mount is empty at build time — the
            // filesystem is finalised at first boot — but mkfs still
            // needs a mount-source to satisfy its `-d` argument. Point
            // at an empty scratch directory.
            let data_mount = scratch_path.join("empty-data-mount");
            vfs.create_dir(&data_mount).await?;
            let data_a_entry = disk_layout
                .os_disk
                .get(Part::DataA)
                .ok_or_else(|| error::Error::BuildStep {
                    step: "data-partition-lookup".into(),
                    reason: "DATA-A absent from computed OS-disk layout".into(),
                })?;
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

            // Root partition contents (erofs image) get dd-ed into
            // ROOT-A on the OS disk.
            let root_entry = self.partition_entry(&disk_layout, Part::RootA)?;
            copy_partition(&vfs, &root_image, &os_disk, root_entry.offset_mib).await?;

            // Verity hash tree into HASH-A.
            let hash_entry = self.partition_entry(&disk_layout, Part::HashA)?;
            copy_partition(&vfs, &verity_image, &os_disk, hash_entry.offset_mib).await?;

            // ------------------------------------------------------
            // Step (g): boot-config.
            //
            // Assemble any bootconfig snippets shipped by installed
            // RPMs under `root-mount/boot/boot-config.d/`. Falls back
            // to the noop kernel-space skeleton when nothing is there.
            // ------------------------------------------------------
            let bootconfig_dir = root_mount_path.join("boot/boot-config.d");
            let bootconfig_text = assemble_bootconfig(&vfs, &bootconfig_dir)
                .await
                .context(error::BrImageSnafu)?;
            let bootconfig_path = out_path.join("boot-config");
            vfs.write(&bootconfig_path, bootconfig_text.as_bytes()).await?;

            // ------------------------------------------------------
            // Step (h): grub install + grub.cfg emission.
            //
            // Two installs run in sequence:
            //   - BIOS grub (x86_64 only; no-ops on aarch64).
            //   - UEFI grub (both arches; ports the ESP-staging block
            //     in `rpm2img::L410-450`).
            //
            // Secure boot key material comes from the `sbkeys` source
            // scope, staged into `deps/sbkeys/`. The directory ships
            // `{PK,KEK,db,vendor}.{crt,cer}` + `config-sign.{key,pub}`
            // per the shell's `SBKEYS` convention.
            // ------------------------------------------------------
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

            // Find the ESP partition on the OS disk for the UEFI
            // install's `dd` target.
            let efi_a_entry = self.partition_entry(&disk_layout, Part::EfiA)?;
            let efi_mount = scratch_path.join("efi-mount");
            let efi_scratch = scratch_path.join("efi.img");

            // Resolve secure-boot key material paths when the feature
            // is on. Every path is derived from `deps/sbkeys/` under
            // the fixed filenames the shell version used (SBKEYS/).
            let sbkeys_dir = if features_bitset.uefi_secure_boot {
                if self.sbkeys_source.is_none() {
                    return Err(error::Error::BuildStep {
                        step: "uefi-secure-boot".into(),
                        reason: "uefi_secure_boot=true requires an `sbkeys` source scope on \
                                 the transform element; declare `source = { sbkeys = [\"//\
                                 addr\"] }` alongside the SDK addr."
                            .into(),
                    }
                    .into());
                }
                Some(std::path::PathBuf::from("deps/sbkeys"))
            } else {
                None
            };

            let sb_vendor_cert_der = sbkeys_dir.as_ref().map(|d| d.join("vendor.cer"));
            let sb_vendor_cer = sb_vendor_cert_der.clone();
            let sb_db_cer = sbkeys_dir.as_ref().map(|d| d.join("db.cer"));
            let sb_config_sign_pubkey = sbkeys_dir.as_ref().map(|d| d.join("config-sign.pub"));
            let sb_db_crt = sbkeys_dir.as_ref().map(|d| d.join("db.crt"));

            if features_bitset.uefi_secure_boot {
                let dir = sbkeys_dir
                    .clone()
                    .expect("sbkeys_dir Some when uefi_secure_boot=true");
                let sbsetup = br_image::secure_boot::Sbsetup {
                    profile: br_image::secure_boot::SbsetupProfile::Local,
                    sbkeys_dir: dir,
                    aws_ca_bundle: None,
                };
                sbsetup.apply(&vfs).await.context(error::BrImageSnafu)?;
            }
            install_grub_uefi(
                &vfs,
                &GrubUefiInstall {
                    root_mount: &root_mount_path,
                    os_image: &os_disk,
                    efi_mount: &efi_mount,
                    efi_scratch: &efi_scratch,
                    efi_partition_mib: efi_a_entry.size_mib,
                    efi_offset_mib: efi_a_entry.offset_mib,
                    uefi_secure_boot: features_bitset.uefi_secure_boot,
                    secure_boot_setup: None,
                    vendor_cert_der: sb_vendor_cert_der.as_deref(),
                    vendor_obj: None,
                    db_cer: sb_db_cer.as_deref(),
                    vendor_cer: sb_vendor_cer.as_deref(),
                    config_sign_pubkey: sb_config_sign_pubkey.as_deref(),
                    db_crt: sb_db_crt.as_deref(),
                    db_cer_for_esp: sb_db_cer.as_deref(),
                    arch,
                },
            )
            .await
            .context(error::BrImageSnafu)?;

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

            // ------------------------------------------------------
            // Step (i): compress every artifact.
            //
            // Each compression step reads from `work/` and writes into
            // `out/`. Downstream layer emission streams from the `out/`
            // paths verbatim, so `LayerOptions::Compression::None` is
            // correct (the on-disk bytes are already lz4-compressed
            // per media-type convention).
            // ------------------------------------------------------
            let os_out = out_path.join("os.img.lz4");
            compress_image(&vfs, &os_disk, &os_out, ArtifactSuffix::ImgLz4)
                .await
                .context(error::BrImageSnafu)?;

            let has_data = data_disk.is_some();
            if let Some(data_path) = data_disk.as_ref() {
                let data_out = out_path.join("data.img.lz4");
                compress_image(&vfs, data_path, &data_out, ArtifactSuffix::ImgLz4)
                    .await
                    .context(error::BrImageSnafu)?;
            }

            let boot_out = out_path.join("boot.ext4.lz4");
            compress_image(&vfs, &boot_scratch, &boot_out, ArtifactSuffix::Ext4Lz4)
                .await
                .context(error::BrImageSnafu)?;

            // The doc names the root output `root.ext4.lz4` even
            // though first-party-stack builds use erofs. Filename
            // convention matches the doc's Outputs table.
            let root_out = out_path.join("root.ext4.lz4");
            compress_image(&vfs, &root_image, &root_out, ArtifactSuffix::Ext4Lz4)
                .await
                .context(error::BrImageSnafu)?;

            let verity_out = out_path.join("verity.lz4");
            compress_image(&vfs, &verity_image, &verity_out, ArtifactSuffix::VerityLz4)
                .await
                .context(error::BrImageSnafu)?;

            // Additionally emit `.vmdk` copies of the OS (and Split
            // data) disks when the variant declared
            // `image-format = "vmdk"`. `compress_image` here is a
            // format converter, not a compressor — the resulting file
            // is a real VMDK image consumed by `assemble_ova` below.
            if matches!(self.options.image_format, ImageFormat::Vmdk) {
                let os_vmdk_path = out_path.join("os.img.vmdk");
                compress_image(&vfs, &os_disk, &os_vmdk_path, ArtifactSuffix::Vmdk)
                    .await
                    .context(error::BrImageSnafu)?;
                if let Some(data_path) = data_disk.as_ref() {
                    let data_vmdk_path = out_path.join("data.img.vmdk");
                    compress_image(&vfs, data_path, &data_vmdk_path, ArtifactSuffix::Vmdk)
                        .await
                        .context(error::BrImageSnafu)?;
                }
            }

            // ------------------------------------------------------
            // Step (j): SBOM.
            //
            // `generate_sbom` invokes `sbomtool` with both `--spdx`
            // and `--cyclonedx`. Pick the SPDX file out of the output
            // directory and stage it as the artifact layer.
            // ------------------------------------------------------
            let sbom_dir = out_path.join("sbom-work");
            generate_sbom(&vfs, &rpm_dir, &sbom_dir)
                .await
                .context(error::BrImageSnafu)?;
            let sbom_out = out_path.join("sbom.spdx.json");
            // sbomtool writes `<name>.spdx.json`; the transform's
            // package name is "variant-rpms" (see `sbom.rs`).
            let sbom_src = sbom_dir.join("variant-rpms.spdx.json");
            if vfs.try_exists(&sbom_src).await? {
                vfs.copy(&sbom_src, &sbom_out).await?;
            } else {
                // Some SDK builds of sbomtool omit the trailing name
                // segment; fall back to the first SPDX file present.
                let candidates = br_image::vfs_ext::list_files(&vfs, &sbom_dir, "*.spdx.json")
                    .await
                    .context(error::BrImageSnafu)?;
                if let Some(first) = candidates.first() {
                    vfs.copy(first, &sbom_out).await?;
                } else {
                    return Err(error::Error::BuildStep {
                        step: "sbom".into(),
                        reason: format!(
                            "no *.spdx.json produced by sbomtool under {}",
                            sbom_dir.display()
                        ),
                    }
                    .into());
                }
            }

            // ------------------------------------------------------
            // Step (k): variant manifest.
            // ------------------------------------------------------
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

            // ------------------------------------------------------
            // Step (l): OVA (vmdk-only).
            //
            // The vmdk files are already at `out/{os,data}.img.vmdk`
            // from the vmdk-emission block above. Render the OVF from
            // a caller-staged template, write it alongside the vmdks,
            // and assemble the OVA per DSP0243.
            //
            // The OVF template comes from the `ovf-template` source
            // scope; `stage()` unpacked it into `deps/ovf-template/`.
            // The first file therein is the template. Its shape
            // follows `br_image::ovf`'s placeholders (`{{OS_DISK}}`,
            // `{{DATA_DISK}}`, `{{OS_DISK_BYTES}}`,
            // `{{DATA_DISK_BYTES}}`; plus the four `_CERT_DER_HEX`
            // slots for secure-boot builds). Absent scope ⇒ typed
            // error rather than a silently-invalid OVA.
            // ------------------------------------------------------
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
                let ovf_template_path = pick_first_in(&vfs, "deps/ovf-template")
                    .await?
                    .ok_or_else(|| error::Error::BuildStep {
                        step: "ova".into(),
                        reason: "`ovf-template` source scope resolved but produced no file \
                                 under deps/ovf-template/ (empty source?)"
                            .into(),
                    })?;
                let template_bytes = vfs.read(&ovf_template_path).await?;
                let template_text = String::from_utf8(template_bytes).map_err(|e| {
                    error::Error::BuildStep {
                        step: "ova".into(),
                        reason: format!("OVF template is not valid UTF-8: {e}"),
                    }
                })?;
                let os_vmdk = out_path.join("os.img.vmdk");
                let data_vmdk_opt = if has_data {
                    Some(out_path.join("data.img.vmdk"))
                } else {
                    None
                };
                let ovf_out = out_path.join("bottlerocket.ovf");
                let rendered = br_image::ovf::render_ovf(
                    &template_text,
                    "os.img.vmdk",
                    if has_data { "data.img.vmdk" } else { "" },
                    os_publish as u32,
                    data_publish.max(0) as u32,
                    None,
                )
                .context(error::BrImageSnafu)?;
                vfs.write(&ovf_out, rendered.as_bytes()).await?;

                let ova_out = out_path.join("bottlerocket.ova");
                br_image::ovf::assemble_ova(
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

            // ------------------------------------------------------
            // Collect outputs. Order + media types match the doc's
            // Outputs table.
            // ------------------------------------------------------
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            let mut layer_specs: Vec<(&str, &str)> =
                vec![("os.img.lz4", output_media_types::OS_IMG_LZ4)];
            if has_data {
                layer_specs.push(("data.img.lz4", output_media_types::DATA_IMG_LZ4));
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
                let writer = ctx.storage().safe_start_layer().await?;
                env.read_stream(&entry, writer.clone()).await?;
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
                artifact.layers_mut().push(layer);
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

/// Parse the `arch` option string as an [`Arch`]. Unlike the free
/// standalone parser, this returns a typed transform error so callers
/// see a helpful message when a variant declares a bogus arch.
fn parse_arch(raw: Option<&str>) -> Result<Arch, error::Error> {
    let s = raw.unwrap_or(std::env::consts::ARCH);
    match s {
        "x86_64" | "amd64" => Ok(Arch::X86_64),
        "aarch64" | "arm64" => Ok(Arch::Aarch64),
        other => Err(error::Error::InvalidArch {
            arch: other.to_string(),
        }),
    }
}

pub mod error {
    use edo::{
        context::{Addr, ContextError},
        transform::TransformError,
    };
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: ContextError },
        #[snafu(display("dependent transform not found: {addr}"))]
        Depend { addr: Addr },
        #[snafu(display(
            "variant-image transform requires exactly one source pointing at the SDK image; none was declared"
        ))]
        MissingSdkSource,
        #[snafu(display(
            "variant-image transform requires exactly one source pointing at the SDK image; multiple sources were declared"
        ))]
        TooManySdkSources,
        #[snafu(display("br-image failure: {source}"))]
        BrImage {
            #[snafu(source(from(br_image::Error, Box::new)))]
            source: Box<br_image::Error>,
        },
        #[snafu(display("json serialization error: {source}"))]
        Serialization { source: serde_json::Error },
        #[snafu(display("unrecognized arch `{arch}` (expected `x86_64` or `aarch64`)"))]
        InvalidArch { arch: String },
        #[snafu(display("variant-image build step `{step}` failed: {reason}"))]
        BuildStep { step: String, reason: String },
        #[snafu(display(
            "variant-image source scope `{scope}` accepts at most one address; got {count}"
        ))]
        TooManyScopeSources { scope: String, count: usize },
    }

    impl From<Error> for TransformError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn base_inputs() -> HashInputs<'static> {
        HashInputs {
            arch: Some("x86_64"),
            sdk_digest: "deadbeef",
            kit_digests: vec!["kitdig1".into()],
            package_digests: vec!["pkgdig1".into()],
            guest_image_digests: vec![],
            ca_bundle_digest: None,
            tuf_root_digest: None,
            sbkeys_digest: None,
            ovf_template_digest: None,
            image_features: BTreeSet::from([ImageFeature::FirstPartyStack]),
            resolved_layout: ImageLayout::new(Some(2), 1, 22, PartitionPlan::Split),
            image_format: ImageFormat::Raw,
            kernel_parameters: vec!["quiet".into()],
            twoliter_binary_digest: "fixed-binary-digest-for-tests",
        }
    }

    #[test]
    fn hash_static_inputs_is_deterministic() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let b = hash_static_inputs(&base_inputs()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "expected sha256 hex, got {a}");
    }

    #[test]
    fn hash_changes_on_arch_change() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.arch = Some("aarch64");
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_on_feature_flip() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.image_features.insert(ImageFeature::UefiSecureBoot);
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_on_kernel_parameter_reorder() {
        // Order-sensitive: grub concatenates the parameters verbatim.
        let mut a_in = base_inputs();
        a_in.kernel_parameters = vec!["a".into(), "b".into()];
        let mut b_in = base_inputs();
        b_in.kernel_parameters = vec!["b".into(), "a".into()];
        assert_ne!(
            hash_static_inputs(&a_in).unwrap(),
            hash_static_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn hash_changes_on_image_format() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.image_format = ImageFormat::Vmdk;
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_separator_prevents_kernel_parameter_concat_collision() {
        // Regression: ["ab", "c"] must hash differently from
        // ["a", "bc"] even though the concatenated bytes are equal.
        let mut a_in = base_inputs();
        a_in.kernel_parameters = vec!["ab".into(), "c".into()];
        let mut b_in = base_inputs();
        b_in.kernel_parameters = vec!["a".into(), "bc".into()];
        assert_ne!(
            hash_static_inputs(&a_in).unwrap(),
            hash_static_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn hash_changes_when_ca_bundle_digest_appears() {
        // Adding a ca-bundle source must invalidate the cache.
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.ca_bundle_digest = Some("cadig1".into());
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_tuf_root_digest_appears() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.tuf_root_digest = Some("tufdig1".into());
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_sbkeys_digest_appears() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.sbkeys_digest = Some("sbkeysdig1".into());
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_when_ovf_template_digest_appears() {
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.ovf_template_digest = Some("ovfdig1".into());
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hash_changes_on_twoliter_binary_digest_change() {
        // A twoliter release that changes `br-image` must invalidate
        // every variant's cache. Confirm the hash is sensitive to the
        // binary digest.
        let a = hash_static_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.twoliter_binary_digest = "different-binary-digest";
        let b = hash_static_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn scope_tags_prevent_digest_swap_collision() {
        // A digest labeled `ca-bundle=X` must hash differently from
        // the same digest labeled `tuf-root=X`. The scope prefix in
        // `hash_static_inputs` prevents that collision.
        let mut a_in = base_inputs();
        a_in.ca_bundle_digest = Some("same-digest".into());
        let mut b_in = base_inputs();
        b_in.tuf_root_digest = Some("same-digest".into());
        assert_ne!(
            hash_static_inputs(&a_in).unwrap(),
            hash_static_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn options_deserialize_from_doc_example_toml() {
        // Doc's TOML surface, trimmed to the fields the transform
        // reads directly (kit references get resolved to `Addr`s by
        // the config-generation layer, so we substitute plain addrs
        // rather than the `[[variant.kit]]` table form).
        let toml = r#"
name = "aws-k8s-1.29"
version = "0.1.0"
arch = "x86_64"

image-format = "raw"
partition-plan = "split"
os-image-size-gib = 2
data-image-size-gib = 1
os-image-publish-size-gib = 8
data-image-publish-size-gib = 20

kernel-parameters = ["quiet", "console=ttyS0"]

kits = ["//kits/my-vendor/extra-3-kit/build"]
packages = ["//packages/pkg-e/rpm"]

[image-features]
grub-set-private-var = true
systemd-networkd = true
uefi-secure-boot = true
xfs-data-partition = true
in-place-updates = false
encrypted-storage = false
erofs-root-partition = false
first-party-stack = true

[included-packages]
packages = ["pkg-e"]
"#;
        let opts: VariantImageOptions = toml::from_str(toml).expect("parse");
        assert_eq!(opts.name, "aws-k8s-1.29");
        assert_eq!(opts.image_format, ImageFormat::Raw);
        assert_eq!(opts.partition_plan, PartitionPlan::Split);
        assert!(opts.image_features.uefi_secure_boot);
        assert!(opts.image_features.first_party_stack);
        assert_eq!(opts.kernel_parameters, vec!["quiet", "console=ttyS0"]);
        assert_eq!(opts.kits.len(), 1);
        assert_eq!(opts.packages.len(), 1);
        assert!(opts.guest_images.is_empty());
    }

    #[test]
    fn options_deserialize_minimal() {
        // Only the required fields present.
        let toml = r#"
name = "x"
version = "0.0.1"
"#;
        let opts: VariantImageOptions = toml::from_str(toml).expect("parse");
        assert_eq!(opts.image_format, ImageFormat::Raw);
        assert_eq!(opts.partition_plan, PartitionPlan::Split);
        assert!(opts.kits.is_empty());
        // Default first-party-stack is true per doc §Fields.
        assert!(opts.image_features.first_party_stack);
    }
}
