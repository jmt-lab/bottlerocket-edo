//! Core types shared across image-building modules.
//!
//! Every type here is `serde`-friendly, uses the manifest layer's
//! kebab-case wire format, and is ported from
//! `old-twoliter/tools/buildsys/src/manifest.rs` (the `SupportedArch`,
//! `ImageFormat`, `PartitionPlan`, `ImageLayout`, `ImageSize`,
//! `ImageFeature` types). Names retain their manifest origins so a grep
//! against old-twoliter still hits.

use std::cmp::max;
use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Target CPU architecture for a Bottlerocket image build.
///
/// Kept in sync with `SupportedArch` in the buildsys manifest. The alias
/// `SupportedArch` is exported for callers migrating from that module.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X86_64,
    Aarch64,
}

/// Backwards-compatible alias for [`Arch`].
pub type SupportedArch = Arch;

serde_plain::derive_fromstr_from_deserialize!(Arch);
serde_plain::derive_display_from_serialize!(Arch);

impl Arch {
    /// Map to the corresponding Docker/OCI architecture string. Matches
    /// `SupportedArch::goarch` in the buildsys manifest.
    pub fn goarch(&self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }
}

/// Output format for a variant image build.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Eif,
    Qcow2,
    Raw,
    Vmdk,
}

serde_plain::derive_fromstr_from_deserialize!(ImageFormat);
serde_plain::derive_display_from_serialize!(ImageFormat);

/// Disk partitioning strategy.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PartitionPlan {
    Split,
    Unified,
}

serde_plain::derive_fromstr_from_deserialize!(PartitionPlan);
serde_plain::derive_display_from_serialize!(PartitionPlan);

/// Constrained image-size newtype (in GiB). u16 gives 0-65535 GiB, which
/// matches every plausible variant image size.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ImageSize(pub u16);

impl fmt::Display for ImageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u16> for ImageSize {
    fn from(v: u16) -> Self {
        Self(v)
    }
}

/// Image layout, as deserialized from a variant manifest.
///
/// Callers should always run the layout through
/// [`crate::layout::resolved_image_layout`] before driving a build.
#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct ImageLayout {
    /// Raw value as it appeared (or did not appear) in the manifest. Use
    /// [`ImageLayout::os_image_size_gib`] to read the resolved value.
    #[serde(default, rename = "os-image-size-gib")]
    os_image_size_gib: Option<ImageSize>,
    #[serde(default = "ImageLayout::default_data_image_size_gib")]
    pub data_image_size_gib: ImageSize,
    #[serde(default = "ImageLayout::default_publish_image_size_hint_gib")]
    publish_image_size_hint_gib: ImageSize,
    #[serde(default = "ImageLayout::default_partition_plan")]
    pub partition_plan: PartitionPlan,
}

/// Historical default OS-image size when the manifest does not specify.
pub const DEFAULT_OS_IMAGE_SIZE_GIB: ImageSize = ImageSize(2);
/// Default OS-image size when `first-party-stack = false` (stripped image).
pub const DEFAULT_NO_FIRST_PARTY_OS_IMAGE_SIZE_GIB: ImageSize = ImageSize(1);
pub const DEFAULT_DATA_IMAGE_SIZE_GIB: ImageSize = ImageSize(1);
pub const DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB: ImageSize = ImageSize(22);
pub const DEFAULT_PARTITION_PLAN: PartitionPlan = PartitionPlan::Split;

impl ImageLayout {
    fn default_data_image_size_gib() -> ImageSize {
        DEFAULT_DATA_IMAGE_SIZE_GIB
    }

    fn default_publish_image_size_hint_gib() -> ImageSize {
        DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB
    }

    fn default_partition_plan() -> PartitionPlan {
        DEFAULT_PARTITION_PLAN
    }

    /// Constructor for tests and non-manifest callers.
    pub fn new(
        os_image_size_gib: Option<u16>,
        data_image_size_gib: u16,
        publish_image_size_hint_gib: u16,
        partition_plan: PartitionPlan,
    ) -> Self {
        Self {
            os_image_size_gib: os_image_size_gib.map(ImageSize),
            data_image_size_gib: ImageSize(data_image_size_gib),
            publish_image_size_hint_gib: ImageSize(publish_image_size_hint_gib),
            partition_plan,
        }
    }

    /// Resolved OS image size, substituting the historical 2 GiB default
    /// when the manifest is silent.
    pub fn os_image_size_gib(&self) -> ImageSize {
        self.os_image_size_gib.unwrap_or(DEFAULT_OS_IMAGE_SIZE_GIB)
    }

    /// Whether `os-image-size-gib` was explicitly set in the manifest.
    pub fn os_image_size_gib_was_set(&self) -> bool {
        self.os_image_size_gib.is_some()
    }

    /// Internal: overwrite the raw OS-image-size field. Used by
    /// [`crate::layout::resolved_image_layout`].
    pub(crate) fn set_os_image_size_gib(&mut self, v: Option<ImageSize>) {
        self.os_image_size_gib = v;
    }

    /// Publish-time OS and data image sizes in GiB. Matches the buildsys
    /// manifest's `publish_image_sizes_gib` exactly.
    ///
    /// For a `Split` plan the OS image publishes at its base size and the
    /// data image absorbs the remainder of the publish hint. For a
    /// `Unified` plan the OS image publishes at the full hint and the
    /// data-image size is returned as `-1` (sentinel: "no separate data
    /// image").
    pub fn publish_image_sizes_gib(&self) -> (i32, i32) {
        let os_image_base_size_gib = self.os_image_size_gib().0;
        let data_image_base_size_gib = self.data_image_size_gib.0;
        let publish_image_size_hint_gib = self.publish_image_size_hint_gib.0;
        let min_publish_image_size_gib = os_image_base_size_gib + data_image_base_size_gib;
        let publish_image_size_gib = max(publish_image_size_hint_gib, min_publish_image_size_gib);
        match self.partition_plan {
            PartitionPlan::Split => {
                let os_image_publish_size_gib = os_image_base_size_gib;
                let data_image_publish_size_gib = publish_image_size_gib - os_image_base_size_gib;
                (
                    os_image_publish_size_gib.into(),
                    data_image_publish_size_gib.into(),
                )
            }
            PartitionPlan::Unified => (publish_image_size_gib.into(), -1),
        }
    }
}

impl Default for ImageLayout {
    fn default() -> Self {
        Self {
            os_image_size_gib: None,
            data_image_size_gib: Self::default_data_image_size_gib(),
            publish_image_size_hint_gib: Self::default_publish_image_size_hint_gib(),
            partition_plan: Self::default_partition_plan(),
        }
    }
}

/// Individual image features. Every variant here has a corresponding
/// kebab-case manifest key and an UPPER_SNAKE_CASE env-var name that
/// build scripts consume.
///
/// | Layer                              | Representation       |
/// |------------------------------------|----------------------|
/// | Rust enum variant                  | UpperCamelCase       |
/// | Manifest TOML key (TryFrom)        | kebab-case           |
/// | Display impl (env var name)        | UPPER_SNAKE_CASE     |
/// | Shell script `--with-X=` value     | `yes` / `no`         |
/// | Runtime `image-features.env` value | `true` / `false`     |
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageFeature {
    GrubSetPrivateVar,
    SystemdNetworkd,
    XfsDataPartition,
    ErofsRootPartition,
    UefiSecureBoot,
    Fips,
    InPlaceUpdates,
    HostContainers,
    ExternalKmodDevelopment,
    EncryptedStorage,
    FirstPartyStack,
}

impl ImageFeature {
    /// Kebab-case manifest key for this feature.
    pub fn kebab(&self) -> &'static str {
        match self {
            ImageFeature::GrubSetPrivateVar => "grub-set-private-var",
            ImageFeature::SystemdNetworkd => "systemd-networkd",
            ImageFeature::XfsDataPartition => "xfs-data-partition",
            ImageFeature::ErofsRootPartition => "erofs-root-partition",
            ImageFeature::UefiSecureBoot => "uefi-secure-boot",
            ImageFeature::Fips => "fips",
            ImageFeature::InPlaceUpdates => "in-place-updates",
            ImageFeature::HostContainers => "host-containers",
            ImageFeature::ExternalKmodDevelopment => "external-kmod-development",
            ImageFeature::EncryptedStorage => "encrypted-storage",
            ImageFeature::FirstPartyStack => "first-party-stack",
        }
    }

    /// All features, in a deterministic order that matches the manifest
    /// author's usual mental ordering.
    pub const ALL: &'static [ImageFeature] = &[
        ImageFeature::GrubSetPrivateVar,
        ImageFeature::SystemdNetworkd,
        ImageFeature::XfsDataPartition,
        ImageFeature::ErofsRootPartition,
        ImageFeature::UefiSecureBoot,
        ImageFeature::Fips,
        ImageFeature::InPlaceUpdates,
        ImageFeature::HostContainers,
        ImageFeature::ExternalKmodDevelopment,
        ImageFeature::EncryptedStorage,
        ImageFeature::FirstPartyStack,
    ];
}

impl TryFrom<String> for ImageFeature {
    type Error = Error;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "grub-set-private-var" => Ok(ImageFeature::GrubSetPrivateVar),
            "systemd-networkd" => Ok(ImageFeature::SystemdNetworkd),
            "xfs-data-partition" => Ok(ImageFeature::XfsDataPartition),
            "erofs-root-partition" => Ok(ImageFeature::ErofsRootPartition),
            "uefi-secure-boot" => Ok(ImageFeature::UefiSecureBoot),
            "fips" => Ok(ImageFeature::Fips),
            "in-place-updates" => Ok(ImageFeature::InPlaceUpdates),
            "host-containers" => Ok(ImageFeature::HostContainers),
            "external-kmod-development" => Ok(ImageFeature::ExternalKmodDevelopment),
            "encrypted-storage" => Ok(ImageFeature::EncryptedStorage),
            "first-party-stack" => Ok(ImageFeature::FirstPartyStack),
            _ => Err(Error::ParseImageFeature { what: s }),
        }
    }
}

impl fmt::Display for ImageFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ImageFeature::GrubSetPrivateVar => "GRUB_SET_PRIVATE_VAR",
            ImageFeature::SystemdNetworkd => "SYSTEMD_NETWORKD",
            ImageFeature::XfsDataPartition => "XFS_DATA_PARTITION",
            ImageFeature::ErofsRootPartition => "EROFS_ROOT_PARTITION",
            ImageFeature::UefiSecureBoot => "UEFI_SECURE_BOOT",
            ImageFeature::Fips => "FIPS",
            ImageFeature::InPlaceUpdates => "IN_PLACE_UPDATES",
            ImageFeature::HostContainers => "HOST_CONTAINERS",
            ImageFeature::ExternalKmodDevelopment => "EXTERNAL_KMOD_DEVELOPMENT",
            ImageFeature::EncryptedStorage => "ENCRYPTED_STORAGE",
            ImageFeature::FirstPartyStack => "FIRST_PARTY_STACK",
        };
        f.write_str(s)
    }
}

impl<'de> Deserialize<'de> for ImageFeature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ImageFeature::try_from(s).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ImageFeature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.kebab())
    }
}

/// Convenience view of an [`ImageFeature`] set as a struct of bools.
///
/// The type boundary crossing module lines is [`BTreeSet<ImageFeature>`]
/// (round-trippable with the manifest and preserved by
/// [`crate::layout::validate_image_features`]). This struct is provided
/// as an ergonomic view for internal call sites that want field-name
/// access; convert with [`Self::from_set`] / [`Self::to_set`].
///
/// The `BTreeSet` (rather than `HashSet`) gives deterministic iteration
/// for messages and hashing.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct ImageFeatures {
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

impl ImageFeatures {
    /// Build from a [`BTreeSet`] of features.
    pub fn from_set(set: &BTreeSet<ImageFeature>) -> Self {
        Self {
            grub_set_private_var: set.contains(&ImageFeature::GrubSetPrivateVar),
            systemd_networkd: set.contains(&ImageFeature::SystemdNetworkd),
            xfs_data_partition: set.contains(&ImageFeature::XfsDataPartition),
            erofs_root_partition: set.contains(&ImageFeature::ErofsRootPartition),
            uefi_secure_boot: set.contains(&ImageFeature::UefiSecureBoot),
            fips: set.contains(&ImageFeature::Fips),
            in_place_updates: set.contains(&ImageFeature::InPlaceUpdates),
            host_containers: set.contains(&ImageFeature::HostContainers),
            external_kmod_development: set.contains(&ImageFeature::ExternalKmodDevelopment),
            encrypted_storage: set.contains(&ImageFeature::EncryptedStorage),
            first_party_stack: set.contains(&ImageFeature::FirstPartyStack),
        }
    }

    /// Convert back to a [`BTreeSet`]. Fully round-trippable with
    /// [`Self::from_set`].
    pub fn to_set(&self) -> BTreeSet<ImageFeature> {
        let mut out = BTreeSet::new();
        if self.grub_set_private_var {
            out.insert(ImageFeature::GrubSetPrivateVar);
        }
        if self.systemd_networkd {
            out.insert(ImageFeature::SystemdNetworkd);
        }
        if self.xfs_data_partition {
            out.insert(ImageFeature::XfsDataPartition);
        }
        if self.erofs_root_partition {
            out.insert(ImageFeature::ErofsRootPartition);
        }
        if self.uefi_secure_boot {
            out.insert(ImageFeature::UefiSecureBoot);
        }
        if self.fips {
            out.insert(ImageFeature::Fips);
        }
        if self.in_place_updates {
            out.insert(ImageFeature::InPlaceUpdates);
        }
        if self.host_containers {
            out.insert(ImageFeature::HostContainers);
        }
        if self.external_kmod_development {
            out.insert(ImageFeature::ExternalKmodDevelopment);
        }
        if self.encrypted_storage {
            out.insert(ImageFeature::EncryptedStorage);
        }
        if self.first_party_stack {
            out.insert(ImageFeature::FirstPartyStack);
        }
        out
    }
}

impl From<&BTreeSet<ImageFeature>> for ImageFeatures {
    fn from(set: &BTreeSet<ImageFeature>) -> Self {
        Self::from_set(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_goarch() {
        assert_eq!(Arch::X86_64.goarch(), "amd64");
        assert_eq!(Arch::Aarch64.goarch(), "arm64");
    }

    #[test]
    fn image_layout_defaults() {
        let l = ImageLayout::default();
        assert_eq!(l.os_image_size_gib(), ImageSize(2));
        assert!(!l.os_image_size_gib_was_set());
        assert_eq!(l.data_image_size_gib, ImageSize(1));
        assert!(matches!(l.partition_plan, PartitionPlan::Split));
    }

    #[test]
    fn publish_image_sizes_split_uses_hint_for_data() {
        // os=2, data=1, hint=22, split -> (2, 20)
        let l = ImageLayout::new(Some(2), 1, 22, PartitionPlan::Split);
        assert_eq!(l.publish_image_sizes_gib(), (2, 20));
    }

    #[test]
    fn publish_image_sizes_unified_returns_negative_one() {
        // unified -> (max(hint, os+data), -1)
        let l = ImageLayout::new(Some(2), 1, 22, PartitionPlan::Unified);
        assert_eq!(l.publish_image_sizes_gib(), (22, -1));
    }

    #[test]
    fn publish_image_sizes_min_size_wins_over_small_hint() {
        // If hint < os + data, we bump to os + data.
        let l = ImageLayout::new(Some(10), 5, 3, PartitionPlan::Split);
        // publish = max(3, 15) = 15; split → (10, 15-10) = (10, 5)
        assert_eq!(l.publish_image_sizes_gib(), (10, 5));
    }

    #[test]
    fn image_feature_try_from_all() {
        for f in ImageFeature::ALL {
            let parsed = ImageFeature::try_from(f.kebab().to_string()).unwrap();
            assert_eq!(parsed, *f, "round trip failed for {f:?}");
        }
    }

    #[test]
    fn image_feature_try_from_rejects_unknown() {
        let err = ImageFeature::try_from("not-a-feature".to_string()).unwrap_err();
        assert!(matches!(err, Error::ParseImageFeature { .. }));
    }

    #[test]
    fn image_feature_display_upper_snake() {
        assert_eq!(
            ImageFeature::FirstPartyStack.to_string(),
            "FIRST_PARTY_STACK"
        );
        assert_eq!(ImageFeature::UefiSecureBoot.to_string(), "UEFI_SECURE_BOOT");
        assert_eq!(
            ImageFeature::ExternalKmodDevelopment.to_string(),
            "EXTERNAL_KMOD_DEVELOPMENT",
        );
        assert_eq!(ImageFeature::Fips.to_string(), "FIPS");
    }

    #[test]
    fn image_features_roundtrip_all_flags() {
        let mut set = BTreeSet::new();
        for f in ImageFeature::ALL {
            set.insert(*f);
        }
        let s = ImageFeatures::from_set(&set);
        assert!(s.grub_set_private_var);
        assert!(s.systemd_networkd);
        assert!(s.xfs_data_partition);
        assert!(s.erofs_root_partition);
        assert!(s.uefi_secure_boot);
        assert!(s.fips);
        assert!(s.in_place_updates);
        assert!(s.host_containers);
        assert!(s.external_kmod_development);
        assert!(s.encrypted_storage);
        assert!(s.first_party_stack);
        assert_eq!(s.to_set(), set);
    }

    #[test]
    fn image_features_empty_roundtrip() {
        let set: BTreeSet<ImageFeature> = BTreeSet::new();
        let s = ImageFeatures::from_set(&set);
        assert_eq!(s, ImageFeatures::default());
        assert_eq!(s.to_set(), set);
    }
}
