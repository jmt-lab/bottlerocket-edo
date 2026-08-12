use edo::context::Addr;
use serde::{Serialize, Deserialize};
use crate::{model::Arch, variant::{ImageFeatures, ImageFormat, PartitionPlan, Variant}};

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct VariantOptions {
    pub name: Variant,
    pub version: semver::Version,
    #[serde(default)]
    pub arch: Arch,
    #[serde(default)]
    pub depends: Vec<Addr>,
    #[serde(default)]
    pub guest_images: Vec<Addr>,
    pub image: ImageOptions,
    pub os: OsOptions,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct OsOptions {
    #[serde(default)]
    pub kernel_parameters: Vec<String>,
    #[serde(default)]
    pub packages: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct ImageOptions {
    pub format: ImageFormat,
    pub partitions: PartitionPlan,
    #[serde(default)]
    pub os_size: Option<u16>,
    #[serde(default)]
    pub data_size: Option<u16>,
    #[serde(default)]
    pub features: ImageFeatures,
}
