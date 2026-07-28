//! Layout resolution and image-feature validation. Ports of
//! `resolved_image_layout` and `validate_image_features` from
//! `old-twoliter/tools/buildsys/src/manifest.rs` (L1155–L1316).
//!
//! These functions are pure: no I/O, no subprocess. Their only inputs are
//! the parsed [`ImageLayout`], the [`ImageFeature`] set, and the optional
//! output [`ImageFormat`].

use std::collections::BTreeSet;

use crate::error::Error;
use crate::model::{
    DEFAULT_NO_FIRST_PARTY_OS_IMAGE_SIZE_GIB, ImageFeature, ImageFormat, ImageLayout, PartitionPlan,
};

/// Return an [`ImageLayout`] with fields adjusted for the given feature
/// set and image format.
///
/// - If `first-party-stack` is disabled (or the image format is `eif`,
///   which is inherently first-party-stack-free) and the manifest did
///   not explicitly set `os-image-size-gib`, the default OS image size
///   is reduced from 2 GiB to 1 GiB.
/// - If the image format is `eif`, the partition plan is forced to
///   `unified`.
///
/// `image_format` is optional so callers without access to the manifest's
/// image format can pass `None` and get the pre-existing
/// `first-party-stack`-only behavior.
pub fn resolved_image_layout(
    layout: &ImageLayout,
    features: &BTreeSet<ImageFeature>,
    image_format: Option<&ImageFormat>,
) -> ImageLayout {
    let mut resolved = *layout;
    let is_eif = matches!(image_format, Some(ImageFormat::Eif));
    let stripped = !features.contains(&ImageFeature::FirstPartyStack) || is_eif;
    if stripped && !layout.os_image_size_gib_was_set() {
        resolved.set_os_image_size_gib(Some(DEFAULT_NO_FIRST_PARTY_OS_IMAGE_SIZE_GIB));
    }
    if is_eif {
        resolved.partition_plan = PartitionPlan::Unified;
    }
    resolved
}

/// Validate that the requested image features and layout are compatible.
///
/// Enforces that disabling `first-party-stack` is not combined with any
/// of `uefi-secure-boot`, `encrypted-storage`, `in-place-updates`,
/// `host-containers`, `xfs-data-partition`, or with a `split` partition
/// plan.
///
/// An `image-format = "eif"` build is subject to the same constraints
/// regardless of the `first-party-stack` toggle: an EIF is a single-bank,
/// dm-verity-protected, partitionless (no DATA/PRIVATE/RESERVED) image.
///
/// All discovered conflicts are reported in a single error so users see
/// the complete list in one build cycle. The conflict list is iterated
/// in a fixed order, so the resulting error message is deterministic.
pub fn validate_image_features(
    features: &BTreeSet<ImageFeature>,
    layout: &ImageLayout,
    image_format: Option<&ImageFormat>,
) -> Result<(), Error> {
    let is_eif = matches!(image_format, Some(ImageFormat::Eif));
    // With `first-party-stack` enabled *and* a non-EIF format, every
    // combination is legal.
    if features.contains(&ImageFeature::FirstPartyStack) && !is_eif {
        return Ok(());
    }

    let context: &str = if is_eif {
        "`image-format = \"eif\"`"
    } else {
        "`first-party-stack = false`"
    };
    let reason_secure_boot: String = if is_eif {
        "secure boot is not supported by the EIF pipeline; `rpm2eif` does not sign shim/grub/vmlinuz".to_string()
    } else {
        "secure boot relies on signed first-party artifacts that are not present when `first-party-stack = false`".to_string()
    };
    let reason_encrypted_storage: String = if is_eif {
        "encrypted storage requires the BOTTLEROCKET-PRIVATE/DATA partitions, which an EIF image does not have".to_string()
    } else {
        "encrypted storage requires the BOTTLEROCKET-PRIVATE/DATA partitions, which are not built when `first-party-stack = false`".to_string()
    };
    let reason_ipu: String = if is_eif {
        "in-place updates require two banks of OS partitions; an EIF ships a single ROOT-A/HASH-A pair".to_string()
    } else {
        "in-place updates require two banks of OS partitions, which are not built when `first-party-stack = false`".to_string()
    };
    let reason_host_containers: String = if is_eif {
        "host-containers require the BOTTLEROCKET-DATA partition, which an EIF image does not have"
            .to_string()
    } else {
        "host-containers require the BOTTLEROCKET-DATA partition, which is not built when `first-party-stack = false`".to_string()
    };
    let reason_first_party_stack =
        "the EIF pipeline is inherently stripped down; `rpm2eif` requires `first-party-stack = false`"
            .to_string();
    let reason_xfs_data_partition: String = if is_eif {
        "xfs-data-partition requires the BOTTLEROCKET-DATA partition, which an EIF image does not have".to_string()
    } else {
        "xfs-data-partition requires the BOTTLEROCKET-DATA partition, which is not built when `first-party-stack = false`".to_string()
    };

    // `eif_only` flags mean the entry is checked only when `is_eif` is
    // true; a `first-party-stack = false` build without EIF should not
    // complain that first-party-stack itself is set.
    let conflicts: &[(ImageFeature, &str, String, bool)] = &[
        (
            ImageFeature::FirstPartyStack,
            "first-party-stack",
            reason_first_party_stack,
            true,
        ),
        (
            ImageFeature::XfsDataPartition,
            "xfs-data-partition",
            reason_xfs_data_partition,
            false,
        ),
        (
            ImageFeature::UefiSecureBoot,
            "uefi-secure-boot",
            reason_secure_boot,
            false,
        ),
        (
            ImageFeature::EncryptedStorage,
            "encrypted-storage",
            reason_encrypted_storage,
            false,
        ),
        (
            ImageFeature::InPlaceUpdates,
            "in-place-updates",
            reason_ipu,
            false,
        ),
        (
            ImageFeature::HostContainers,
            "host-containers",
            reason_host_containers,
            false,
        ),
    ];

    let mut conflict_messages: Vec<String> = conflicts
        .iter()
        .filter(|(feature, _, _, eif_only)| features.contains(feature) && (!eif_only || is_eif))
        .map(|(_, name, reason, _)| format!("`{name}`: {reason}"))
        .collect();

    if matches!(layout.partition_plan, PartitionPlan::Split) {
        let reason = if is_eif {
            "`partition-plan=split`: an EIF is a single unified disk; the split \
             layout has no meaning"
        } else {
            "`partition-plan=split`: when `first-party-stack = false`, the image uses a \
             single unified disk with no separate data image"
        };
        conflict_messages.push(reason.to_string());
    }

    if !conflict_messages.is_empty() {
        let reason = format!("\n  - {}", conflict_messages.join("\n  - "));
        return Err(Error::InvalidLayout {
            context: context.to_string(),
            reason,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ImageLayout;

    fn features(flags: &[ImageFeature]) -> BTreeSet<ImageFeature> {
        flags.iter().copied().collect()
    }

    #[test]
    fn resolved_layout_first_party_default_untouched() {
        let l = ImageLayout::default();
        let f = features(&[ImageFeature::FirstPartyStack]);
        let r = resolved_image_layout(&l, &f, None);
        assert_eq!(r.os_image_size_gib().0, 2);
        assert!(matches!(r.partition_plan, PartitionPlan::Split));
    }

    #[test]
    fn resolved_layout_stripped_reduces_default_os_size() {
        let l = ImageLayout::default();
        let f = features(&[]); // no first-party-stack
        let r = resolved_image_layout(&l, &f, None);
        assert_eq!(r.os_image_size_gib().0, 1);
    }

    #[test]
    fn resolved_layout_stripped_respects_explicit_os_size() {
        let l = ImageLayout::new(Some(5), 1, 22, PartitionPlan::Unified);
        let f = features(&[]);
        let r = resolved_image_layout(&l, &f, None);
        assert_eq!(r.os_image_size_gib().0, 5);
    }

    #[test]
    fn resolved_layout_eif_forces_unified_and_shrinks_default_os() {
        let l = ImageLayout::default();
        let f = features(&[ImageFeature::FirstPartyStack]);
        let r = resolved_image_layout(&l, &f, Some(&ImageFormat::Eif));
        assert!(matches!(r.partition_plan, PartitionPlan::Unified));
        assert_eq!(r.os_image_size_gib().0, 1);
    }

    #[test]
    fn validate_first_party_stack_allows_anything_non_eif() {
        let l = ImageLayout::default();
        let f = features(&[
            ImageFeature::FirstPartyStack,
            ImageFeature::UefiSecureBoot,
            ImageFeature::InPlaceUpdates,
        ]);
        validate_image_features(&f, &l, None).expect("first-party-stack + non-EIF is legal");
    }

    #[test]
    fn validate_empty_features_no_split_ok() {
        let l = ImageLayout::new(None, 1, 22, PartitionPlan::Unified);
        let f: BTreeSet<ImageFeature> = BTreeSet::new();
        validate_image_features(&f, &l, None).expect("empty features + unified is legal");
    }

    #[test]
    fn validate_first_party_stack_false_with_secure_boot_errors() {
        let l = ImageLayout::new(None, 1, 22, PartitionPlan::Unified);
        let f = features(&[ImageFeature::UefiSecureBoot]);
        let err = validate_image_features(&f, &l, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`first-party-stack = false`"), "msg: {msg}");
        assert!(msg.contains("uefi-secure-boot"), "msg: {msg}");
    }

    #[test]
    fn validate_first_party_stack_false_split_errors() {
        let l = ImageLayout::new(None, 1, 22, PartitionPlan::Split);
        let f: BTreeSet<ImageFeature> = BTreeSet::new();
        let err = validate_image_features(&f, &l, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("partition-plan=split"), "msg: {msg}");
    }

    #[test]
    fn validate_eif_forbids_first_party_stack_flag() {
        let l = ImageLayout::new(None, 1, 22, PartitionPlan::Unified);
        let f = features(&[ImageFeature::FirstPartyStack]);
        let err = validate_image_features(&f, &l, Some(&ImageFormat::Eif)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`image-format = \"eif\"`"), "msg: {msg}");
        assert!(msg.contains("`first-party-stack`"), "msg: {msg}");
    }

    #[test]
    fn validate_reports_multiple_conflicts_in_order() {
        let l = ImageLayout::new(None, 1, 22, PartitionPlan::Split);
        let f = features(&[
            ImageFeature::XfsDataPartition,
            ImageFeature::UefiSecureBoot,
            ImageFeature::EncryptedStorage,
            ImageFeature::InPlaceUpdates,
            ImageFeature::HostContainers,
        ]);
        let err = validate_image_features(&f, &l, None).unwrap_err();
        let msg = err.to_string();
        // Order per the `conflicts` slice, and split-plan last.
        let xfs_idx = msg.find("xfs-data-partition").expect("xfs mentioned");
        let sb_idx = msg.find("uefi-secure-boot").expect("secure-boot mentioned");
        let enc_idx = msg.find("encrypted-storage").expect("enc mentioned");
        let ipu_idx = msg.find("in-place-updates").expect("ipu mentioned");
        let hc_idx = msg.find("host-containers").expect("hc mentioned");
        let split_idx = msg.find("partition-plan=split").expect("split mentioned");
        assert!(
            xfs_idx < sb_idx
                && sb_idx < enc_idx
                && enc_idx < ipu_idx
                && ipu_idx < hc_idx
                && hc_idx < split_idx,
            "conflict order wrong: {msg}"
        );
    }
}
