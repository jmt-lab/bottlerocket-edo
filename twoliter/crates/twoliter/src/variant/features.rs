use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::{collections::BTreeMap, fmt};

/// Set of supported image features with their associated bit size
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImageFeature {
    #[serde(rename = "grub-set-private-var")]
    GrubSetPrivateVar = 1isize,
    #[serde(rename = "systemd-networkd")]
    SystemdNetworkd = 2isize,
    #[serde(rename = "xfs-data-partition")]
    XfsDataPartition = 4isize,
    #[serde(rename = "erofs-root-partition")]
    ErofsRootPartition = 8isize,
    #[serde(rename = "uefi-secure-boot")]
    UefiSecureBoot = 16isize,
    #[serde(rename = "fips")]
    Fips = 32isize,
    #[serde(rename = "in-place-updates")]
    InPlaceUpdates = 64isize,
    #[serde(rename = "host-containers")]
    HostContainers = 128isize,
    #[serde(rename = "external-kmod-development")]
    ExternalKmodDevelopment = 256isize,
    #[serde(rename = "encrypted-storage")]
    EncryptedStorage = 512isize,
    #[serde(rename = "standalone-image")]
    StandaloneImage = 1024isize,
}

impl ImageFeature {
    pub const EXPERIMENTAL: &'static [ImageFeature] =
        &[Self::EncryptedStorage, Self::StandaloneImage];

    pub const DEPRECATED: &'static [ImageFeature] =
        &[Self::GrubSetPrivateVar, Self::SystemdNetworkd];

    pub const ALL: &'static [ImageFeature] = &[
        Self::GrubSetPrivateVar,
        Self::SystemdNetworkd,
        Self::XfsDataPartition,
        Self::ErofsRootPartition,
        Self::UefiSecureBoot,
        Self::Fips,
        Self::InPlaceUpdates,
        Self::HostContainers,
        Self::ExternalKmodDevelopment,
        Self::EncryptedStorage,
        Self::StandaloneImage,
    ];

    pub fn to_argname(&self) -> &'static str {
        match self {
            Self::XfsDataPartition => "--with-xfs-data-partition",
            Self::ErofsRootPartition => "--with-erofs-root-partition",
            Self::UefiSecureBoot => "--with-uefi-secure-boot",
            Self::InPlaceUpdates => "--with-in-place-updates",
            Self::EncryptedStorage => "--with-encrypted-storage",
            Self::StandaloneImage => "--with-standalone-image",
            _ => "",
        }
    }
}

impl fmt::Display for ImageFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
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
            ImageFeature::StandaloneImage => "standalone-image",
        };
        f.write_str(s)
    }
}

/// BitMask representing the settings of image features
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageFeatures(isize);

impl ImageFeatures {
    pub fn as_bytes(&self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub fn is_enabled(&self, feature: ImageFeature) -> bool {
        self.0 & feature as isize != 0
    }

    pub fn validate(&self, eif: bool) -> Vec<Collision> {
        let mut collisions = Vec::new();
        if eif && self.is_enabled(ImageFeature::UefiSecureBoot) {
            collisions.push(Collision::EifSecureBoot {});
        }
        if eif && !self.is_enabled(ImageFeature::StandaloneImage) {
            collisions.push(Collision::EifStandalone {});
        }
        if eif && self.is_enabled(ImageFeature::EncryptedStorage) {
            collisions.push(Collision::EifEncryptedStorage {});
        }
        if eif && self.is_enabled(ImageFeature::InPlaceUpdates) {
            collisions.push(Collision::EifInPlaceUpdates {});
        }
        if eif && self.is_enabled(ImageFeature::HostContainers) {
            collisions.push(Collision::EifHostContainers {});
        }
        if eif && self.is_enabled(ImageFeature::XfsDataPartition) {
            collisions.push(Collision::EifXfsDataPartition {});
        }

        if self.is_enabled(ImageFeature::StandaloneImage)
            && self.is_enabled(ImageFeature::UefiSecureBoot)
        {
            collisions.push(Collision::StandaloneSecureBoot {});
        }

        if self.is_enabled(ImageFeature::StandaloneImage)
            && self.is_enabled(ImageFeature::EncryptedStorage)
        {
            collisions.push(Collision::StandaloneEncryptedStorage {});
        }

        if self.is_enabled(ImageFeature::StandaloneImage)
            && self.is_enabled(ImageFeature::InPlaceUpdates)
        {
            collisions.push(Collision::StandaloneInPlaceUpdates {});
        }

        if self.is_enabled(ImageFeature::StandaloneImage)
            && self.is_enabled(ImageFeature::XfsDataPartition)
        {
            collisions.push(Collision::StandaloneXfsDataPartition {});
        }

        collisions
    }

    pub fn to_bconds(&self) -> String {
        // `metadata.spec` guards each feature with `%if %{with foo_bar}`,
        // which requires the bcond identifier to be an rpm macro name
        // (letters/digits/underscores). `ImageFeature`'s `Display` is
        // dash-cased for the wire format (`erofs-root-partition`), so
        // emitting it verbatim produces `%bcond_without erofs-root-partition`,
        // which rpm silently drops -- the spec then always falls through
        // to the `%else` branch and provides `no-erofs-root-partition`
        // even when the variant requested `erofs-root-partition = true`.
        //
        // Match old-twoliter (`build.Dockerfile:249-257`) and normalize
        // to underscores here.
        let mut out = String::new();
        for feature in ImageFeature::ALL {
            if self.is_enabled(feature.clone()) {
                let name = feature.to_string().replace('-', "_");
                out.push_str(&format!("%bcond_without {}\n", name));
            }
        }
        out
    }

    pub fn to_args(&self) -> Vec<String> {
        ImageFeature::ALL
            .iter()
            .filter_map(|x| {
                let y = x.to_argname();
                if y.is_empty() {
                    None
                } else {
                    Some(format!(
                        "{y}={}",
                        if self.is_enabled(x.clone()) {
                            "yes"
                        } else {
                            "no"
                        }
                    ))
                }
            })
            .collect()
    }
}

impl<'de> serde::Deserialize<'de> for ImageFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let table: BTreeMap<ImageFeature, bool> = BTreeMap::deserialize(deserializer)?;
        let bitmask: isize = table
            .iter()
            .map(|(feature, flag)| {
                if *flag {
                    feature.clone() as isize
                } else {
                    0isize
                }
            })
            .reduce(|l, r| l + r)
            .unwrap_or(0);
        Ok(Self(bitmask))
    }
}

#[derive(Debug, Snafu)]
pub enum Collision {
    #[snafu(display(
        "encrypted storage requires the BOTTLEROCKET-PRIVATE/DATA partitions, which an EIF image does not have"
    ))]
    EifEncryptedStorage,
    #[snafu(display(
        "in-place updates require two banks of OS partitions; an EIF ships a single bank"
    ))]
    EifInPlaceUpdates,
    #[snafu(display("secure boot is not supporeted by the EIF pipeline"))]
    EifSecureBoot,
    #[snafu(display(
        "host-containers require the BOTTLEROCKET-DATA partition, which an EIF image does not have"
    ))]
    EifHostContainers,
    #[snafu(display(
        "The EIF format is inherently stripped down and requires `standalone-image = truez`"
    ))]
    EifStandalone,
    #[snafu(display(
        "xfs-data-partition requires the BOTTLEROCKET-DATA partition, which an EIF image does not contain."
    ))]
    EifXfsDataPartition,
    #[snafu(display(
        "secure boot relies on signed first-party artifacts that are not present when `standalone-image = true`"
    ))]
    StandaloneSecureBoot,
    #[snafu(display(
        "encrypted storage requires the BOTTLEROCKET-PRIVATE/DATA partitions, which are not built when `standalone-image = true`"
    ))]
    StandaloneEncryptedStorage,
    #[snafu(display(
        "in-place updates require two banks of OS partitions, which are not built when `standalone-image = true`"
    ))]
    StandaloneInPlaceUpdates,
    #[snafu(display(
        "xfs-data-partition requires the BOTTLEROCKET-DATA partition, which is not built when `standalone-image = true`"
    ))]
    StandaloneXfsDataPartition,
}
