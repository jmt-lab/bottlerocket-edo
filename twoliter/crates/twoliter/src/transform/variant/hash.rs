//! Deterministic unique-id hashing shared by every `variant-*` transform.
//!
//! [`HashInputs`] is the *union* of every input to every stage: adding
//! a field here is a one-line change and each stage's subset-hash
//! function picks it up automatically if it names the field. The four
//! [`hash_rootfs_inputs`], [`hash_disk_inputs`], [`hash_grub_inputs`],
//! and [`hash_image_inputs`] functions each fold a strict subset into a
//! sha256, labeled with a stage-specific prefix so a digest from one
//! stage cannot collide with a digest from another stage even when
//! their inputs happen to be identical.
//!
//! **Twoliter binary digest.** Every stage folds in the content-address
//! of the running twoliter binary via [`twoliter_binary_digest`]. A
//! twoliter release that changes `br-image` invalidates every stage's
//! cache — which is correct.

use crate::image::{ImageFeature, ImageFormat, ImageLayout, PartitionPlan};
use sha2::{Digest, Sha256};
use snafu::ResultExt;
use std::collections::BTreeSet;

use super::error;

/// Every input to the deterministic unique-id hash of any `variant-*`
/// transform. Individual stages pick out the subset they care about via
/// [`hash_rootfs_inputs`], [`hash_disk_inputs`], [`hash_grub_inputs`],
/// or [`hash_image_inputs`].
///
/// Extracted from the transform bodies so the hashing itself can be
/// exercised by unit tests without a live scheduler `Handle` (real
/// dep-id resolution requires one).
#[derive(Debug, Clone)]
pub struct HashInputs<'a> {
    pub arch: Option<&'a str>,
    pub sdk_digest: &'a str,
    pub kit_digests: Vec<String>,
    pub package_digests: Vec<String>,
    pub guest_image_digests: Vec<String>,
    pub ca_bundle_digest: Option<String>,
    pub tuf_root_digest: Option<String>,
    pub sbkeys_digest: Option<String>,
    pub ovf_template_digest: Option<String>,
    pub image_features: BTreeSet<ImageFeature>,
    pub resolved_layout: ImageLayout,
    pub image_format: ImageFormat,
    pub partition_plan: PartitionPlan,
    pub kernel_parameters: Vec<String>,
    pub uefi_secure_boot: bool,
    pub first_party_stack: bool,
    pub os_image_publish_size_gib: Option<u16>,
    pub data_image_publish_size_gib: Option<u16>,
    /// Upstream stage's digest (rootfs → disk → grub → image chain).
    /// `None` on the rootfs stage; `Some(<digest>)` on every downstream
    /// stage.
    pub upstream_stage_digest: Option<String>,
    pub twoliter_binary_digest: &'a str,
}

/// Hash `variant-rootfs` inputs into a stage-labeled sha256.
///
/// The rootfs stage cares about: arch, sdk digest, kit + package +
/// guest-image digests, `ca-bundle` + `tuf-root` source scopes, and
/// the twoliter binary digest. Feature flags, layout, kernel
/// parameters, `sbkeys`, and `ovf-template` do *not* affect the
/// staged rootfs — those are downstream concerns.
pub fn hash_rootfs_inputs(inputs: &HashInputs<'_>) -> Result<String, error::Error> {
    let mut hash = Sha256::new();
    hash.update(b"variant-rootfs");
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
    // Scope-tagged: `ca-bundle=abc` must not collide with
    // `tuf-root=abc`.
    for (scope, maybe_digest) in [
        ("ca-bundle=", &inputs.ca_bundle_digest),
        ("tuf-root=", &inputs.tuf_root_digest),
    ] {
        if let Some(d) = maybe_digest {
            hash.update(scope.as_bytes());
            hash.update(d.as_bytes());
            hash.update([0u8]);
        }
    }
    hash.update(inputs.twoliter_binary_digest.as_bytes());
    Ok(base16::encode_lower(hash.finalize().as_slice()))
}

/// Hash `variant-disk` inputs into a stage-labeled sha256.
///
/// The disk stage consumes the rootfs stage's digest plus the
/// partition/layout inputs (image features, resolved layout,
/// partition plan, image format) that decide how partitions are
/// carved and mkfs'd.
pub fn hash_disk_inputs(inputs: &HashInputs<'_>) -> Result<String, error::Error> {
    let mut hash = Sha256::new();
    hash.update(b"variant-disk");
    if let Some(upstream) = &inputs.upstream_stage_digest {
        hash.update(upstream.as_bytes());
    }
    hash.update(serde_json::to_vec(&inputs.image_features).context(error::SerializationSnafu)?);
    hash.update(serde_json::to_vec(&inputs.resolved_layout).context(error::SerializationSnafu)?);
    hash.update(inputs.image_format.to_string().as_bytes());
    hash.update(format!("{:?}", inputs.partition_plan).as_bytes());
    hash.update(inputs.twoliter_binary_digest.as_bytes());
    Ok(base16::encode_lower(hash.finalize().as_slice()))
}

/// Hash `variant-grub` inputs into a stage-labeled sha256.
///
/// The grub stage consumes the disk stage's digest plus kernel
/// parameters (order-sensitive), the `uefi-secure-boot` and
/// `first-party-stack` toggles, arch, and the optional `sbkeys`
/// digest.
pub fn hash_grub_inputs(inputs: &HashInputs<'_>) -> Result<String, error::Error> {
    let mut hash = Sha256::new();
    hash.update(b"variant-grub");
    if let Some(upstream) = &inputs.upstream_stage_digest {
        hash.update(upstream.as_bytes());
    }
    for kp in &inputs.kernel_parameters {
        hash.update(kp.as_bytes());
        // Separator so ["ab", "c"] != ["a", "bc"].
        hash.update([0u8]);
    }
    hash.update([inputs.uefi_secure_boot as u8]);
    hash.update([inputs.first_party_stack as u8]);
    hash.update(inputs.arch.unwrap_or("").as_bytes());
    if let Some(d) = &inputs.sbkeys_digest {
        hash.update(b"sbkeys=");
        hash.update(d.as_bytes());
        hash.update([0u8]);
    }
    hash.update(inputs.twoliter_binary_digest.as_bytes());
    Ok(base16::encode_lower(hash.finalize().as_slice()))
}

/// Hash `variant-image` (finalizer) inputs into a stage-labeled sha256.
///
/// The finalizer consumes the grub stage's digest plus image-format,
/// publish-size hints, and the optional `ovf-template` digest.
///
/// **Note on double-counting.** `manifest.json` embeds
/// `image-features` + `resolved-layout` but those are already folded
/// into the upstream stage digests, so we don't fold them again here.
pub fn hash_image_inputs(inputs: &HashInputs<'_>) -> Result<String, error::Error> {
    let mut hash = Sha256::new();
    hash.update(b"variant-image");
    if let Some(upstream) = &inputs.upstream_stage_digest {
        hash.update(upstream.as_bytes());
    }
    hash.update(inputs.image_format.to_string().as_bytes());
    if let Some(gib) = inputs.os_image_publish_size_gib {
        hash.update(b"os-publish=");
        hash.update(gib.to_string().as_bytes());
        hash.update([0u8]);
    }
    if let Some(gib) = inputs.data_image_publish_size_gib {
        hash.update(b"data-publish=");
        hash.update(gib.to_string().as_bytes());
        hash.update([0u8]);
    }
    if let Some(d) = &inputs.ovf_template_digest {
        hash.update(b"ovf-template=");
        hash.update(d.as_bytes());
        hash.update([0u8]);
    }
    hash.update(inputs.twoliter_binary_digest.as_bytes());
    Ok(base16::encode_lower(hash.finalize().as_slice()))
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
pub fn twoliter_binary_digest() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| match compute_twoliter_binary_digest() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                component = "transform",
                type_ = "variant-image",
                "unable to hash twoliter binary ({e}); falling back to version string. \
                     Cache invalidation will only trigger on version bumps until this is \
                     resolved."
            );
            format!("{}-{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
            partition_plan: PartitionPlan::Split,
            kernel_parameters: vec!["quiet".into()],
            uefi_secure_boot: false,
            first_party_stack: true,
            os_image_publish_size_gib: Some(8),
            data_image_publish_size_gib: Some(20),
            upstream_stage_digest: None,
            twoliter_binary_digest: "fixed-binary-digest-for-tests",
        }
    }

    // -------------- rootfs subset --------------

    #[test]
    fn rootfs_hash_is_deterministic() {
        let a = hash_rootfs_inputs(&base_inputs()).unwrap();
        let b = hash_rootfs_inputs(&base_inputs()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "expected sha256 hex, got {a}");
    }

    #[test]
    fn rootfs_hash_changes_on_arch_change() {
        let a = hash_rootfs_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.arch = Some("aarch64");
        let b = hash_rootfs_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn rootfs_hash_changes_on_kit_digest_change() {
        let a = hash_rootfs_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.kit_digests = vec!["different-kit".into()];
        let b = hash_rootfs_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn rootfs_hash_ignores_grub_only_inputs() {
        // Rootfs digest must not shift when only grub-affecting inputs
        // change: this is the whole point of the split.
        let a = hash_rootfs_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.kernel_parameters = vec!["a".into(), "b".into()];
        i.uefi_secure_boot = true;
        i.sbkeys_digest = Some("sb".into());
        let b = hash_rootfs_inputs(&i).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rootfs_scope_tags_prevent_digest_swap_collision() {
        // A digest labeled `ca-bundle=X` must hash differently from
        // the same digest labeled `tuf-root=X`.
        let mut a_in = base_inputs();
        a_in.ca_bundle_digest = Some("same-digest".into());
        let mut b_in = base_inputs();
        b_in.tuf_root_digest = Some("same-digest".into());
        assert_ne!(
            hash_rootfs_inputs(&a_in).unwrap(),
            hash_rootfs_inputs(&b_in).unwrap(),
        );
    }

    // -------------- disk subset --------------

    #[test]
    fn disk_hash_is_deterministic() {
        let a = hash_disk_inputs(&base_inputs()).unwrap();
        let b = hash_disk_inputs(&base_inputs()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn disk_hash_changes_on_upstream_change() {
        let mut a_in = base_inputs();
        a_in.upstream_stage_digest = Some("rootfs-a".into());
        let mut b_in = base_inputs();
        b_in.upstream_stage_digest = Some("rootfs-b".into());
        assert_ne!(
            hash_disk_inputs(&a_in).unwrap(),
            hash_disk_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn disk_hash_changes_on_feature_flip() {
        let a = hash_disk_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.image_features.insert(ImageFeature::XfsDataPartition);
        let b = hash_disk_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn disk_hash_ignores_kernel_parameters() {
        // Kernel parameters are a grub concern, not a disk-layout
        // concern.
        let a = hash_disk_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.kernel_parameters = vec!["quiet".into(), "console=ttyS0".into()];
        let b = hash_disk_inputs(&i).unwrap();
        assert_eq!(a, b);
    }

    // -------------- grub subset --------------

    #[test]
    fn grub_hash_is_deterministic() {
        let a = hash_grub_inputs(&base_inputs()).unwrap();
        let b = hash_grub_inputs(&base_inputs()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn grub_hash_changes_on_kernel_parameter_reorder() {
        // Order-sensitive: grub concatenates the parameters verbatim.
        let mut a_in = base_inputs();
        a_in.kernel_parameters = vec!["a".into(), "b".into()];
        let mut b_in = base_inputs();
        b_in.kernel_parameters = vec!["b".into(), "a".into()];
        assert_ne!(
            hash_grub_inputs(&a_in).unwrap(),
            hash_grub_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn grub_hash_separator_prevents_kernel_parameter_concat_collision() {
        // Regression: ["ab", "c"] must hash differently from
        // ["a", "bc"] even though the concatenated bytes are equal.
        let mut a_in = base_inputs();
        a_in.kernel_parameters = vec!["ab".into(), "c".into()];
        let mut b_in = base_inputs();
        b_in.kernel_parameters = vec!["a".into(), "bc".into()];
        assert_ne!(
            hash_grub_inputs(&a_in).unwrap(),
            hash_grub_inputs(&b_in).unwrap(),
        );
    }

    #[test]
    fn grub_hash_changes_on_uefi_secure_boot_flip() {
        let a = hash_grub_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.uefi_secure_boot = true;
        let b = hash_grub_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn grub_hash_changes_when_sbkeys_digest_appears() {
        let a = hash_grub_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.sbkeys_digest = Some("sbkeysdig1".into());
        let b = hash_grub_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    // -------------- image (finalizer) subset --------------

    #[test]
    fn image_hash_is_deterministic() {
        let a = hash_image_inputs(&base_inputs()).unwrap();
        let b = hash_image_inputs(&base_inputs()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn image_hash_changes_on_image_format() {
        let a = hash_image_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.image_format = ImageFormat::Vmdk;
        let b = hash_image_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn image_hash_changes_when_ovf_template_digest_appears() {
        let a = hash_image_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.ovf_template_digest = Some("ovfdig1".into());
        let b = hash_image_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn image_hash_changes_on_publish_size_change() {
        let a = hash_image_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.os_image_publish_size_gib = Some(42);
        let b = hash_image_inputs(&i).unwrap();
        assert_ne!(a, b);
    }

    // -------------- cross-stage anti-collision --------------

    #[test]
    fn stage_labels_prevent_cross_stage_collision() {
        // Even with identical fields, the four subset hashes must
        // differ because each uses a stage-specific label prefix.
        let inputs = base_inputs();
        let rootfs = hash_rootfs_inputs(&inputs).unwrap();
        let disk = hash_disk_inputs(&inputs).unwrap();
        let grub = hash_grub_inputs(&inputs).unwrap();
        let image = hash_image_inputs(&inputs).unwrap();
        // All four distinct.
        let set = BTreeSet::from([rootfs, disk, grub, image]);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn all_hashes_change_on_twoliter_binary_digest_change() {
        // A twoliter release that changes `br-image` must invalidate
        // every stage's cache.
        let a_root = hash_rootfs_inputs(&base_inputs()).unwrap();
        let a_disk = hash_disk_inputs(&base_inputs()).unwrap();
        let a_grub = hash_grub_inputs(&base_inputs()).unwrap();
        let a_image = hash_image_inputs(&base_inputs()).unwrap();
        let mut i = base_inputs();
        i.twoliter_binary_digest = "different-binary-digest";
        assert_ne!(a_root, hash_rootfs_inputs(&i).unwrap());
        assert_ne!(a_disk, hash_disk_inputs(&i).unwrap());
        assert_ne!(a_grub, hash_grub_inputs(&i).unwrap());
        assert_ne!(a_image, hash_image_inputs(&i).unwrap());
    }
}
