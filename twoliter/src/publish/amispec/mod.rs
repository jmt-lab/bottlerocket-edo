pub mod ebs;

use aws_sdk_ec2::operation::register_image::RegisterImageInput;
use aws_sdk_ec2::operation::register_image::builders::RegisterImageInputBuilder;
use aws_sdk_ec2::types::BlockDeviceMapping as SdkBlockDeviceMapping;
use bon::Builder;
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub use ebs::EbsBlockDevice;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Builder)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[builder(on(_, into))]
pub struct AmiSpec {
    #[builder(field)]
    #[serde(alias = "block-device-mapping")]
    pub block_device_mappings: Option<HashMap<String, BlockDeviceMapping>>,
    pub name: String,
    pub architecture: Option<Architecture>,
    pub billing_product: Option<Vec<String>>,
    pub boot_mode: Option<BootMode>,
    pub description: Option<String>,
    pub ena_support: Option<bool>,
    pub imds_support: Option<ImdsSupport>,
    pub root_device_name: Option<String>,
    pub sriov_net_support: Option<SriovNetSupport>,
    pub tags: Option<HashMap<String, String>>,
    pub tpm_support: Option<TpmSupport>,
    pub uefi_data: Option<String>,
    pub virtualization_type: Option<VirtualizationType>,
}

// Allow adding a single BlockDeviceMapping per function call to the builder
impl<S: ami_spec_builder::State> AmiSpecBuilder<S> {
    pub fn block_device_mapping(
        mut self,
        block_device_mapping: impl Into<BlockDeviceMapping>,
    ) -> Self {
        let bdm = block_device_mapping.into();
        self.block_device_mappings
            .get_or_insert_with(Default::default)
            .insert(bdm.device_name.clone().unwrap(), bdm.clone());
        self
    }

    pub fn maybe_block_device_mapping(
        self,
        block_device_mapping: Option<impl Into<BlockDeviceMapping>>,
    ) -> Self {
        if let Some(named_block_device_mapping) = block_device_mapping {
            self.block_device_mapping(named_block_device_mapping)
        } else {
            self
        }
    }
}

impl AmiSpec {
    pub fn as_register_image_call(&self) -> RegisterImageInputBuilder {
        let AmiSpec {
            block_device_mappings,
            architecture,
            billing_product,
            boot_mode,
            description,
            ena_support,
            imds_support,
            name,
            root_device_name,
            sriov_net_support,
            tags,
            tpm_support,
            uefi_data,
            virtualization_type: _, // Only HVM is supported
        } = self;

        RegisterImageInput::builder()
            .name(name)
            // `amispec` only supports hvm images.
            // `paravirtual` is the RegisterImage default, so we just always set it to `hvm` here.
            .virtualization_type(VirtualizationType::Hvm.to_string())
            .set_architecture(architecture.map(Into::into))
            .set_billing_products(billing_product.clone())
            .set_block_device_mappings(block_device_mappings.as_ref().map(|bdms| {
                bdms.iter()
                    .map(|(device_name, block_device_mapping)| {
                        block_device_mapping.create_sdk_block_device_mapping(device_name)
                    })
                    .collect()
            }))
            .set_boot_mode(boot_mode.map(Into::into))
            .set_description(description.clone())
            .set_ena_support(*ena_support)
            .set_imds_support(imds_support.map(Into::into))
            .set_root_device_name(root_device_name.clone())
            .set_sriov_net_support(sriov_net_support.map(|sriov| sriov.to_string()))
            .set_tpm_support(tpm_support.map(Into::into))
            .set_uefi_data(uefi_data.clone())
            .set_tag_specifications(tags.as_ref().map(|tags| {
                vec![
                    aws_sdk_ec2::types::TagSpecification::builder()
                        .resource_type(aws_sdk_ec2::types::ResourceType::Image)
                        .set_tags(Some({
                            let mut sorted_tags: Vec<_> = tags.iter().collect();
                            sorted_tags.sort_by_key(|(k, _)| *k);
                            sorted_tags
                                .into_iter()
                                .map(|(k, v)| {
                                    aws_sdk_ec2::types::Tag::builder().key(k).value(v).build()
                                })
                                .collect()
                        }))
                        .build(),
                ]
            }))
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    I386,
    X86_64,
    #[serde(alias = "aarch64")]
    Arm64,
    X86_64Mac,
    #[serde(alias = "aarch64_mac")]
    Arm64Mac,
}
serde_plain::derive_fromstr_from_deserialize!(Architecture);
serde_plain::derive_display_from_serialize!(Architecture);

impl From<Architecture> for aws_sdk_ec2::types::ArchitectureValues {
    fn from(value: Architecture) -> Self {
        match value {
            Architecture::I386 => Self::I386,
            Architecture::X86_64 => Self::X8664,
            Architecture::Arm64 => Self::Arm64,
            Architecture::X86_64Mac => Self::X8664Mac,
            Architecture::Arm64Mac => Self::Arm64Mac,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum BootMode {
    LegacyBios,
    Uefi,
    UefiPreferred,
}
serde_plain::derive_fromstr_from_deserialize!(BootMode);
serde_plain::derive_display_from_serialize!(BootMode);

impl From<BootMode> for aws_sdk_ec2::types::BootModeValues {
    fn from(value: BootMode) -> Self {
        match value {
            BootMode::LegacyBios => Self::LegacyBios,
            BootMode::Uefi => Self::Uefi,
            BootMode::UefiPreferred => Self::UefiPreferred,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum ImdsSupport {
    #[serde(rename = "v2.0")]
    V2_0,
}
serde_plain::derive_fromstr_from_deserialize!(ImdsSupport);
serde_plain::derive_display_from_serialize!(ImdsSupport);

impl From<ImdsSupport> for aws_sdk_ec2::types::ImdsSupportValues {
    fn from(value: ImdsSupport) -> Self {
        match value {
            ImdsSupport::V2_0 => Self::V20,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum TpmSupport {
    #[serde(rename = "v2.0")]
    V2_0,
}
serde_plain::derive_fromstr_from_deserialize!(TpmSupport);
serde_plain::derive_display_from_serialize!(TpmSupport);

impl From<TpmSupport> for aws_sdk_ec2::types::TpmSupportValues {
    fn from(value: TpmSupport) -> Self {
        match value {
            TpmSupport::V2_0 => Self::V20,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum SriovNetSupport {
    #[serde(rename = "simple")]
    Simple,
}
serde_plain::derive_fromstr_from_deserialize!(SriovNetSupport);
serde_plain::derive_display_from_serialize!(SriovNetSupport);

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum VirtualizationType {
    Hvm,
    // Paravirtual is not supported
}
serde_plain::derive_fromstr_from_deserialize!(VirtualizationType);
serde_plain::derive_display_from_serialize!(VirtualizationType);

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Default, Builder)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[builder(on(_, into))]
pub struct BlockDeviceMapping {
    pub ebs: Option<EbsBlockDevice>,
    pub no_device: Option<String>,
    pub virtual_name: Option<String>,
    pub device_name: Option<String>,
}

impl BlockDeviceMapping {
    pub fn create_sdk_block_device_mapping(
        &self,
        device_name: impl Into<String>,
    ) -> SdkBlockDeviceMapping {
        SdkBlockDeviceMapping::builder()
            .set_ebs(
                self.ebs
                    .as_ref()
                    .map(|block_device| block_device.create_sdk_ebs_block_device()),
            )
            .set_no_device(self.no_device.clone())
            .set_virtual_name(self.virtual_name.clone())
            .device_name(self.device_name.clone().unwrap_or(device_name.into()))
            .build()
    }
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum AmiSpecError {
    #[snafu(display("failed to deserialize amispec.toml at {path:?}: {source}"))]
    Deserialize {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[snafu(display("failed to read amispec file at {path:?}: {source}"))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
}
