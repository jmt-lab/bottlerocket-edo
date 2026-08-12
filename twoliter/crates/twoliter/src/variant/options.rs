use std::path::PathBuf;

use crate::{
    model::Arch,
    variant::{ImageFeatures, ImageFormat, PartitionPlan, Variant},
};
use edo::context::Addr;
use serde::{Deserialize, Serialize};

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
    /// On-host path to `variants/<name>/template.ovf`. Populated by
    /// the config emitter (`config::variant::Variant::to_edo`) using
    /// old-twoliter's `Makefile.toml:173` convention. The transform
    /// stages the file into the container only if it exists on disk;
    /// non-VMDK variants without a template are silently tolerated
    /// because `imghelper.sh::sanity_checks` only faults the file in
    /// when `format = "vmdk"`.
    #[serde(default)]
    pub ovf_template: Option<PathBuf>,
}
