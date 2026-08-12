use crate::variant::{
    ImageFeature, ImageFeatures, ImageOptions, PartitionPlan,
    disk::{
        constants::*,
        model::{Bank, Partition, Slot},
    },
};
use snafu::{OptionExt, ensure};
use std::collections::HashMap;

/// Represents a planned disk layout for a variant
#[derive(Debug, Clone)]
pub struct Layout {
    pub bios: Partition,
    pub banks: Vec<Bank>,
    pub private: Option<Partition>,
    pub data_a: Option<Partition>,
    pub data_b: Option<Partition>,
}

impl Layout {
    pub fn new(options: &ImageOptions, features: &ImageFeatures) -> Result<Self, error::Error> {
        let os_gib = options.os_size.unwrap_or(DEFAULT_OS_IMAGE_SIZE_GIB) as u32;
        let data_gib = options.data_size.unwrap_or(DEFAULT_DATA_IMAGE_SIZE_GIB) as u32;
        let plan = options.partitions;
        let in_place = features.is_enabled(ImageFeature::InPlaceUpdates);
        let standalone_image = features.is_enabled(ImageFeature::StandaloneImage);

        ensure!(os_gib > 0, error::OsZeroSnafu);
        if !standalone_image {
            ensure!(data_gib > 0, error::DataZeroSnafu);
        }

        // Per-GiB scaled parttition sizes
        let boot_mib = os_gib * BOOT_SCALE_FACTOR;
        let root_mib = os_gib * ROOT_SCALE_FACTOR;
        let hash_mib = os_gib * HASH_SCALE_FACTOR;
        let mut offset = GPT_MIB; // skip GPT header

        let bios = Partition::builder()
            .slot(Slot::Bios)
            .offset(offset)
            .size(BIOS_MIB)
            .build();
        offset += BIOS_MIB;

        if standalone_image {
            // Stripped-down single-bank layout is used when standalone-image = true
            ensure!(!in_place, error::InPlaceStandaloneSnafu);
            ensure!(
                !matches!(plan, PartitionPlan::Split),
                error::SplitStandaloneSnafu
            );

            let total_mib = os_gib * 1024;
            let efi_system = EFI_MIB * 2;
            let boot_size = boot_mib * 2;
            let hash_size = hash_mib * 2;
            // ROOT-A absorbs the leftover in standalone-image
            // total - GPT(header) - BIOS - EFI-SYSTEM*2 - BOOT-A*2 - HASH-A*2 - GPT(footer)
            let root_size = total_mib - GPT_MIB - efi_system - boot_size - hash_size - GPT_MIB;
            ensure!(root_size != 0, error::RootZeroSnafu);

            let slots = HashMap::from([
                (Slot::EfiSystem, efi_system),
                (Slot::BootA, boot_size),
                (Slot::RootA, root_size),
                (Slot::HashA, hash_size),
            ]);
            let (bank, offset) = bank(offset, slots)?;

            // Sanity check: offsets + GTP footer must equal total_mib
            ensure!(
                offset + GPT_MIB == total_mib,
                error::OffsetMismatchSnafu { offset, total_mib }
            );

            return Ok(Self {
                bios,
                banks: vec![bank],
                private: None, // standalone-image does not have a private partition
                data_a: None,  // standalone-image does not have a data partition
                data_b: None,
            });
        }

        // Standard disk layout
        //
        let reserved_mib = os_gib * RESERVE_SCALE_FACTOR - EFI_MIB;
        let private_mib = os_gib * PRIVATE_SCALE_FACTOR - OVERHEAD_MIB - DATA_A_MIB;
        let mut banks = Vec::new();
        if in_place {
            // Bank A
            let slots = HashMap::from([
                (Slot::EfiSystem, EFI_MIB),
                (Slot::BootA, boot_mib),
                (Slot::RootA, root_mib),
                (Slot::HashA, hash_mib),
                (Slot::ReservedA, reserved_mib),
            ]);
            let (bank_a, adj_offset) = bank(offset, slots)?;
            offset = adj_offset;
            banks.push(bank_a);

            // Bank B
            let slots = HashMap::from([
                (Slot::EfiBackup, EFI_MIB),
                (Slot::BootB, boot_mib),
                (Slot::RootB, root_mib),
                (Slot::HashB, hash_mib),
                (Slot::ReservedB, reserved_mib),
            ]);
            let (bank_b, adj_offset) = bank(offset, slots)?;
            offset = adj_offset;
            banks.push(bank_b);
        } else {
            // Single bank, each partition doubled
            let slots = HashMap::from([
                (Slot::EfiSystem, EFI_MIB * 2),
                (Slot::BootA, boot_mib * 2),
                (Slot::RootA, root_mib * 2),
                (Slot::HashA, hash_mib * 2),
                (Slot::ReservedA, reserved_mib * 2),
            ]);

            let (bank, adj_offset) = bank(offset, slots)?;
            banks.push(bank);
            offset = adj_offset;
        }

        let private = Partition::builder()
            .slot(Slot::Private)
            .offset(offset)
            .size(private_mib)
            .build();
        offset += private_mib;

        let data_mib = data_gib * 1024;
        let data_a = Partition::builder()
            .slot(Slot::DataA)
            .offset(offset)
            .size(data_mib)
            .build();
        offset += data_mib;

        let data_b = if plan == PartitionPlan::Split {
            // For a split data image, the first and last MiB are reserved for
            // the GPT labels, and the rest is for data partition B
            Some(
                Partition::builder()
                    .slot(Slot::DataB)
                    .offset(1)
                    .size(data_mib - GPT_MIB * 2)
                    .build(),
            )
        } else {
            None
        };

        Ok(Self {
            banks,
            bios,
            private: Some(private),
            data_a: Some(data_a),
            data_b,
        })
    }
}

pub fn bank(offset: u32, slots: HashMap<Slot, u32>) -> Result<(Bank, u32), error::Error> {
    let (efi_slot, efi_mib) = slots
        .get_key_value(&Slot::EfiSystem)
        .or(slots.get_key_value(&Slot::EfiBackup))
        .context(error::EfiMissingSnafu)?;
    let efi = Partition::builder()
        .slot(efi_slot.clone())
        .size(*efi_mib)
        .offset(offset)
        .build();
    let mut offset = offset + *efi_mib;
    let (boot_slot, boot_mib) = slots
        .get_key_value(&Slot::BootA)
        .or(slots.get_key_value(&Slot::BootB))
        .context(error::BootMissingSnafu)?;
    let boot = Partition::builder()
        .slot(boot_slot.clone())
        .size(*boot_mib)
        .offset(offset)
        .build();
    offset += *boot_mib;
    let (root_slot, root_mib) = slots
        .get_key_value(&Slot::RootA)
        .or(slots.get_key_value(&Slot::RootB))
        .context(error::RootMissingSnafu)?;
    let root = Partition::builder()
        .slot(root_slot.clone())
        .size(*root_mib)
        .offset(offset)
        .build();
    offset += *root_mib;
    let (hash_slot, hash_mib) = slots
        .get_key_value(&Slot::HashA)
        .or(slots.get_key_value(&Slot::HashB))
        .context(error::HashMissingSnafu)?;
    let hash = Partition::builder()
        .slot(hash_slot.clone())
        .size(*hash_mib)
        .offset(offset)
        .build();
    offset += *hash_mib;
    let reserved = if let Some((reserved_slot, reserved_mib)) = slots
        .get_key_value(&Slot::ReservedA)
        .or(slots.get_key_value(&Slot::ReservedB))
    {
        let part = Partition::builder()
            .slot(reserved_slot.clone())
            .size(*reserved_mib)
            .offset(offset)
            .build();
        offset += *reserved_mib;
        Some(part)
    } else {
        None
    };
    Ok((
        Bank::builder()
            .efi(efi)
            .boot(boot)
            .root(root)
            .hash(hash)
            .maybe_reserved(reserved)
            .build(),
        offset,
    ))
}

pub mod error {
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("an os image requires a boot partition"))]
        BootMissing,
        #[snafu(display("in-place-updates is not supported with `standalone-image = true`"))]
        InPlaceStandalone,
        #[snafu(display("split partition layout is not supported with `standalone-image = true`"))]
        SplitStandalone,
        #[snafu(display("an os image must have a data partition of size larger than 0"))]
        DataZero,
        #[snafu(display("an os image requires an efi partition"))]
        EfiMissing,
        #[snafu(display("an os image requires a hash partition"))]
        HashMissing,
        #[snafu(display("offset mismatch: offset={offset}, total_mib={total_mib}"))]
        OffsetMismatch { offset: u32, total_mib: u32 },
        #[snafu(display("an os image must at least be 1GiB"))]
        OsZero,
        #[snafu(display("an os image requires a root partition"))]
        RootMissing,
        #[snafu(display("an os image must have a root partition > 0"))]
        RootZero,
    }
}
