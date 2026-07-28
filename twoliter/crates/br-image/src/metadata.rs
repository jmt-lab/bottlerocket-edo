//! Variant-metadata RPM spec and rpmbuild argument generation, and the
//! variant-manifest emission that `twoliter publish` consumers read.
//!
//! Consumes the bundled `metadata.spec` (see [`crate::METADATA_SPEC`])
//! along with the resolved [`ImageFeatures`] to render the `--with` /
//! `--without` argument list rpmbuild expects.
//!
//! [`VariantManifest`] is the JSON blob that ends up as the
//! `manifest.json` output layer of the `variant-image` transform. Its
//! shape is a straight port of the fields
//! `old-twoliter/tools/buildsys/src/manifest.rs::ManifestInfo` writes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use snafu::ResultExt;

use crate::error::{Result, SerializationSnafu};
use crate::model::{
    Arch, ImageFeature, ImageFeatures, ImageFormat, ImageLayout, PartitionPlan,
};

/// Convert an [`ImageFeatures`] snapshot into the ordered
/// `--with FLAG` / `--without FLAG` argument list for `rpmbuild`. Every
/// feature listed in [`RPMBUILD_TOGGLES`] appears exactly once.
///
/// Matches the shell's convention: features named in `metadata.spec`
/// under `%if %{with feature_name}` translate to `--with feature_name`
/// / `--without feature_name`.
pub fn render_metadata_rpmbuild_args(features: &ImageFeatures) -> Vec<String> {
    let mut out = Vec::with_capacity(RPMBUILD_TOGGLES.len() * 2);
    for (flag, on) in metadata_toggles(features) {
        out.push(if on {
            "--with".into()
        } else {
            "--without".into()
        });
        out.push(flag.into());
    }
    out
}

/// Features that appear as `%if %{with ...}` toggles in `metadata.spec`.
///
/// The `rpmbuild_name` column is the exact identifier the spec expects;
/// the accessor produces the value from an [`ImageFeatures`] snapshot.
pub const RPMBUILD_TOGGLES: &[(ImageFeature, &str)] = &[
    (ImageFeature::InPlaceUpdates, "in_place_updates"),
    (ImageFeature::HostContainers, "host_containers"),
    (ImageFeature::UefiSecureBoot, "uefi_secure_boot"),
    (ImageFeature::XfsDataPartition, "xfs_data_partition"),
    (ImageFeature::ErofsRootPartition, "erofs_root_partition"),
    (ImageFeature::Fips, "fips"),
    (
        ImageFeature::ExternalKmodDevelopment,
        "external_kmod_development",
    ),
    (ImageFeature::EncryptedStorage, "encrypted_storage"),
    (ImageFeature::FirstPartyStack, "first_party_stack"),
];

fn metadata_toggles(features: &ImageFeatures) -> Vec<(&'static str, bool)> {
    RPMBUILD_TOGGLES
        .iter()
        .map(|(feature, name)| (*name, is_on(features, *feature)))
        .collect()
}

fn is_on(features: &ImageFeatures, feature: ImageFeature) -> bool {
    match feature {
        ImageFeature::GrubSetPrivateVar => features.grub_set_private_var,
        ImageFeature::SystemdNetworkd => features.systemd_networkd,
        ImageFeature::XfsDataPartition => features.xfs_data_partition,
        ImageFeature::ErofsRootPartition => features.erofs_root_partition,
        ImageFeature::UefiSecureBoot => features.uefi_secure_boot,
        ImageFeature::Fips => features.fips,
        ImageFeature::InPlaceUpdates => features.in_place_updates,
        ImageFeature::HostContainers => features.host_containers,
        ImageFeature::ExternalKmodDevelopment => features.external_kmod_development,
        ImageFeature::EncryptedStorage => features.encrypted_storage,
        ImageFeature::FirstPartyStack => features.first_party_stack,
    }
}

/// Variant `manifest.json` — the descriptor
/// `twoliter publish ami` / `twoliter publish repo` consume when they
/// pick up an already-built variant image.
///
/// Every field maps 1:1 to an emitter in
/// `old-twoliter/tools/buildsys/src/manifest.rs::ManifestInfo`. Kept
/// kebab-case on the wire so the file remains grep-compatible with the
/// old manifests.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct VariantManifest {
    pub name: String,
    pub version: semver::Version,
    pub arch: Arch,
    pub image_format: ImageFormat,
    pub partition_plan: PartitionPlan,
    /// Full resolved layout (post-[`crate::layout::resolved_image_layout`]).
    pub image_layout: ImageLayout,
    /// The set of enabled features. Serialized as a sorted array of
    /// kebab-case identifiers (matches [`ImageFeature`]'s wire
    /// format), which round-trips cleanly through the same set-of-
    /// features shape old buildsys used.
    pub image_features: BTreeSet<ImageFeature>,
    /// Publish-time OS image size in GiB, or `None` when the caller
    /// left it to publish tooling to derive.
    #[serde(default)]
    pub os_image_publish_size_gib: Option<i32>,
    /// Publish-time data image size in GiB. `-1` means "no separate
    /// data image" (the caller is on a `Unified` plan); `None` when
    /// unset.
    #[serde(default)]
    pub data_image_publish_size_gib: Option<i32>,
    /// Kernel-parameters in their declaration order (order-sensitive:
    /// grub concatenates them verbatim).
    #[serde(default)]
    pub kernel_parameters: Vec<String>,
}

impl VariantManifest {
    /// Serialize to pretty-printed JSON bytes suitable for writing to
    /// the `manifest.json` output layer.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(self).context(SerializationSnafu)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_off_yields_all_without() {
        let f = ImageFeatures::default();
        let args = render_metadata_rpmbuild_args(&f);
        assert_eq!(args.len(), RPMBUILD_TOGGLES.len() * 2);
        for pair in args.chunks(2) {
            assert_eq!(pair[0], "--without");
        }
    }

    #[test]
    fn selected_features_produce_with() {
        let f = ImageFeatures {
            uefi_secure_boot: true,
            first_party_stack: true,
            in_place_updates: true,
            ..ImageFeatures::default()
        };
        let args = render_metadata_rpmbuild_args(&f);
        // Format is (--with|--without) NAME pairs; scan for our flags.
        let mut with = Vec::new();
        let mut without = Vec::new();
        for pair in args.chunks(2) {
            if pair[0] == "--with" {
                with.push(pair[1].clone());
            } else {
                without.push(pair[1].clone());
            }
        }
        assert!(with.contains(&"uefi_secure_boot".to_string()));
        assert!(with.contains(&"first_party_stack".to_string()));
        assert!(with.contains(&"in_place_updates".to_string()));
        assert!(without.contains(&"fips".to_string()));
        assert!(without.contains(&"encrypted_storage".to_string()));
    }

    #[test]
    fn embedded_metadata_spec_is_non_empty() {
        assert!(crate::METADATA_SPEC.contains("%{_cross_os}metadata"));
    }

    #[test]
    fn variant_manifest_round_trips_through_json() {
        let mut features = BTreeSet::new();
        features.insert(ImageFeature::FirstPartyStack);
        features.insert(ImageFeature::UefiSecureBoot);
        let manifest = VariantManifest {
            name: "aws-k8s-1.29".into(),
            version: semver::Version::new(0, 1, 0),
            arch: Arch::X86_64,
            image_format: ImageFormat::Raw,
            partition_plan: PartitionPlan::Split,
            image_layout: ImageLayout::new(Some(2), 1, 22, PartitionPlan::Split),
            image_features: features,
            os_image_publish_size_gib: Some(2),
            data_image_publish_size_gib: Some(20),
            kernel_parameters: vec!["quiet".into(), "console=ttyS0".into()],
        };
        let bytes = manifest.to_json_bytes().expect("serialize");
        let parsed: VariantManifest = serde_json::from_slice(&bytes).expect("round-trip");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn variant_manifest_wire_shape_is_kebab_case() {
        let manifest = VariantManifest {
            name: "x".into(),
            version: semver::Version::new(0, 0, 1),
            arch: Arch::X86_64,
            image_format: ImageFormat::Raw,
            partition_plan: PartitionPlan::Split,
            image_layout: ImageLayout::default(),
            image_features: BTreeSet::new(),
            os_image_publish_size_gib: None,
            data_image_publish_size_gib: None,
            kernel_parameters: vec![],
        };
        let bytes = manifest.to_json_bytes().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        // Every field name is kebab-case; assert on a handful of the
        // hyphen-containing ones to catch a rename regression.
        assert!(text.contains("\"image-format\""), "text={text}");
        assert!(text.contains("\"partition-plan\""), "text={text}");
        assert!(text.contains("\"image-layout\""), "text={text}");
        assert!(text.contains("\"image-features\""), "text={text}");
        assert!(text.contains("\"kernel-parameters\""), "text={text}");
    }
}
