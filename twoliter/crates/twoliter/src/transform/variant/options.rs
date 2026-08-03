//! Wire-format types for the `variant-*` transform family.
//!
//! Every `variant-*` transform's `FromElement` reads its element config
//! into a subset of [`VariantImageOptions`]. Keeping the union type in
//! one place (rather than one per transform) means adding a new field
//! to the TOML surface changes exactly one struct and every stage
//! picks the field up automatically if it's relevant.

use crate::image::{ImageFeature, ImageFeatures, ImageFormat, PartitionPlan};
use edo::context::Addr;
use serde::Deserialize;
use std::collections::BTreeSet;

/// Wire-format for the `variant-image` transform family's TOML block.
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
    /// [`crate::image::DEFAULT_PARTITION_PLAN`]).
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

    /// Sorted list of package short names to hand to `dnf install`
    /// at rootfs-stage time.
    ///
    /// Populated by [`crate::config::variant::Variant::to_edo`] as
    /// the union of two source blocks:
    ///
    /// - `[variant.included-packages].packages` — kit-supplied names;
    ///   their RPMs arrive via the rootfs element's `kits` source
    ///   scope.
    /// - `[variant.local-packages].packages`, transitively closed
    ///   over `[package].depends` — in-repo names; each corresponds
    ///   to a `//packages/<name>/rpm` entry in [`Self::packages`].
    ///
    /// Consumed by `VariantRootfsTransform::stage` when building the
    /// dnf command line for the rootfs. Every name here gets a
    /// `bottlerocket-` prefix at that point.
    #[serde(default)]
    pub included_packages: Vec<String>,

    /// Extra RPM packages (transform addresses, one per rpm build).
    #[serde(default)]
    pub packages: Vec<Addr>,

    /// Guest-image transforms whose outputs must be embedded in the
    /// rootfs at `/local/guest-images/<arch>-<name>/<version>/`.
    #[serde(default)]
    pub guest_images: Vec<Addr>,

    /// Arches this variant is allowed to build on. `None` = any arch.
    /// TODO: enforce at build time — old-twoliter's equivalent lives
    /// in `old-twoliter/tools/buildsys/src/manifest.rs::SupportedArch`
    /// and `validate_image_features`. Right now this field is carried
    /// on the wire but no transform reads it.
    #[serde(default)]
    pub supported_arches: Option<Vec<String>>,
}

pub(crate) fn default_image_format() -> ImageFormat {
    ImageFormat::Raw
}

pub(crate) fn default_partition_plan() -> PartitionPlan {
    crate::image::DEFAULT_PARTITION_PLAN
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
    pub fn as_bitset(&self) -> ImageFeatures {
        self.into()
    }

    /// Sorted set of enabled features. Used both by the manifest and
    /// as an input to the deterministic unique-id hash.
    pub fn as_set(&self) -> BTreeSet<ImageFeature> {
        self.as_bitset().to_set()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_deserialize_from_doc_example_toml() {
        // Doc's TOML surface, trimmed to the fields the transform
        // reads directly. Kits are not part of this wire type — they
        // flow into the rootfs element as `kit-image` sources under
        // the `"kits"` scope, and are consumed by
        // `VariantRootfsTransform::new` via `ctx.add_source(...)`.
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

packages = ["//packages/pkg-e/rpm"]
included-packages = ["pkg-e", "kernel-6.18"]

[image-features]
grub-set-private-var = true
systemd-networkd = true
uefi-secure-boot = true
xfs-data-partition = true
in-place-updates = false
encrypted-storage = false
erofs-root-partition = false
first-party-stack = true
"#;
        let opts: VariantImageOptions = toml::from_str(toml).expect("parse");
        assert_eq!(opts.name, "aws-k8s-1.29");
        assert_eq!(opts.image_format, ImageFormat::Raw);
        assert_eq!(opts.partition_plan, PartitionPlan::Split);
        assert!(opts.image_features.uefi_secure_boot);
        assert!(opts.image_features.first_party_stack);
        assert_eq!(opts.kernel_parameters, vec!["quiet", "console=ttyS0"]);
        assert_eq!(opts.packages.len(), 1);
        assert_eq!(
            opts.included_packages,
            vec!["pkg-e".to_string(), "kernel-6.18".to_string()],
        );
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
        assert!(opts.packages.is_empty());
        assert!(opts.included_packages.is_empty());
        // Default first-party-stack is true per doc §Fields.
        assert!(opts.image_features.first_party_stack);
    }
}
