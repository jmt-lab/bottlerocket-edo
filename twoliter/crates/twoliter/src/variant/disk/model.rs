use serde::{Deserialize, Serialize};

use crate::variant::{ImageFeatures, ImageOptions, disk::constants::*};

/// Identifier for a Bottlerocket disk partition Slot
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum Slot {
    Bios,
    EfiSystem,
    EfiBackup,
    BootA,
    BootB,
    RootA,
    RootB,
    HashA,
    HashB,
    ReservedA,
    ReservedB,
    Private,
    DataA,
    DataB,
}

impl Slot {
    pub fn to_typecode(&self) -> &'static str {
        match self {
            Self::Bios => BIOS_BOOT_TYPECODE,
            Self::EfiSystem => EFI_SYSTEM_TYPECODE,
            Self::EfiBackup => EFI_BACKUP_TYPECODE,
            Self::BootA | Self::BootB => BOTTLEROCKET_BOOT_TYPECODE,
            Self::RootA | Self::RootB => BOTTLEROCKET_ROOT_TYPECODE,
            Self::HashA | Self::HashB => BOTTLEROCKET_HASH_TYPECODE,
            Self::ReservedA | Self::ReservedB => BOTTLEROCKET_RESERVED_TYPECODE,
            Self::Private => BOTTLEROCKET_PRIVATE_TYPECODE,
            Self::DataA | Self::DataB => BOTTLEROCKET_DATA_TYPECODE,
        }
    }

    pub fn to_label(&self) -> &'static str {
        match self {
            Self::Bios => "BIOS-BOOT",
            Self::EfiSystem => "EFI-SYSTEM",
            Self::EfiBackup => "EFI-BACKUP",
            Self::BootA => "BOTTLEROCKET-BOOT-A",
            Self::BootB => "BOTTLEROCKET-BOOT-B",
            Self::RootA => "BOTTLEROCKET-ROOT-A",
            Self::RootB => "BOTTLEROCKET-ROOT-B",
            Self::HashA => "BOTTLEROCKET-HASH-A",
            Self::HashB => "BOTTLEROCKET-HASH-B",
            Self::ReservedA => "BOTTLEROCKET-RESERVED-A",
            Self::ReservedB => "BOTTLEROCKET-RESERVED-B",
            Self::Private => "BOTTLEROCKET-PRIVATE",
            Self::DataA => "BOTTLEROCKET-DATA-A",
            Self::DataB => "BOTTLEROCKET-DATA_B",
        }
    }

    pub fn to_attributes(&self) -> &'static [(&'static str, u8)] {
        match self {
            Self::BootA => &[("set", 48), ("set", 56)],
            Self::BootB => &[("clear", 48), ("clear", 56)],
            _ => &[],
        }
    }
}

/// Single Partition in the disk layout
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct Partition {
    pub slot: Slot,
    /// Offset from the start of the disk, in MiB
    pub offset: u32,
    /// Size in MiB
    pub size: u32,
}

/// Represents a single bank in the disk image
#[derive(Debug, Clone, bon::Builder)]
pub struct Bank {
    pub efi: Partition,
    pub boot: Partition,
    pub root: Partition,
    pub hash: Partition,
    pub reserved: Option<Partition>,
}
