//! Port of `partyplanner`: GPT partition layout math and constants for
//! Bottlerocket disk images.
//!
//! All units are MiB (matching the shell). Offsets are in MiB from the
//! start of the disk; sizes are in MiB.
//!
//! Layouts covered:
//! - Standard first-party-stack image: `set_partition_sizes`, both
//!   `in_place_updates=yes/no` and `split`/`unified` plans →
//!   [`compute_partition_layout`].
//! - Stripped-down `first_party_stack=no` image → same entry point,
//!   selecting a single-bank residual-ROOT layout.
//! - EIF sidecar disk: `set_eif_partition_sizes` → [`eif_partition_layout`].

use std::fmt;

use crate::error::Error;
use crate::model::{ImageFeatures, ImageLayout, PartitionPlan};

// -------------------------------------------------------------------
// Partition-type GUIDs (from `partyplanner` §1).

/// Bottlerocket-specific GPT type GUIDs, plus the vendor-defined BIOS
/// boot and EFI system partition typecodes.
pub const BOTTLEROCKET_BOOT_TYPECODE: &str = "6b636168-7420-6568-2070-6c616e657421";
pub const BOTTLEROCKET_ROOT_TYPECODE: &str = "5526016a-1a97-4ea4-b39a-b7c8c6ca4502";
pub const BOTTLEROCKET_HASH_TYPECODE: &str = "598f10af-c955-4456-6a99-7720068a6cea";
pub const BOTTLEROCKET_RESERVED_TYPECODE: &str = "0c5d99a5-d331-4147-baef-08e2b855bdc9";
pub const BOTTLEROCKET_PRIVATE_TYPECODE: &str = "440408bb-eb0b-4328-a6e5-a29038fad706";
pub const BOTTLEROCKET_DATA_TYPECODE: &str = "626f7474-6c65-6474-6861-726d61726b73";
/// `sgdisk` shorthand for the BIOS boot partition type; NOT a UUID.
pub const BIOS_BOOT_TYPECODE: &str = "ef02";
pub const EFI_SYSTEM_TYPECODE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const EFI_BACKUP_TYPECODE: &str = "B39CE39C-0A00-B4AB-2D11-F18F8237A21C";

/// GPT PARTUUID assigned to the "preferred" data partition (matches the
/// first data volume attached at boot).
pub const BOTTLEROCKET_DATA_PREFERRED_PARTGUID: &str = "5b94e8df-28b8-485c-9d19-362263b5944c";
/// GPT PARTUUID for the "fallback" data partition (created but not
/// chosen unless the preferred one is missing).
pub const BOTTLEROCKET_DATA_FALLBACK_PARTGUID: &str = "69040874-417d-4e26-a764-7885f22007ea";

// -------------------------------------------------------------------
// Fixed reservations (in MiB), from `partyplanner` §2.

/// GPT header/footer reservation, applied twice per disk.
pub const GPT_MIB: u32 = 1;
/// BIOS boot partition (x86 only, but always allocated).
pub const BIOS_MIB: u32 = 4;
/// Combined overhead: two GPT reservations + BIOS.
pub const OVERHEAD_MIB: u32 = GPT_MIB * 2 + BIOS_MIB;
/// EFI system partition per bank.
pub const EFI_MIB: u32 = 5;
/// Initial DATA-A partition size.
pub const DATA_A_MIB: u32 = 1;

// -------------------------------------------------------------------
// Scale factors, from `partyplanner` §3. Applied per GiB of image.

pub const BOOT_SCALE_FACTOR: u32 = 20;
pub const ROOT_SCALE_FACTOR: u32 = 460;
pub const HASH_SCALE_FACTOR: u32 = 5;
pub const RESERVE_SCALE_FACTOR: u32 = 15;
pub const PRIVATE_SCALE_FACTOR: u32 = 24;

// -------------------------------------------------------------------
// Types.

/// Identifier for a Bottlerocket disk partition slot. Matches the string
/// keys used by the shell's associative arrays (`BIOS`, `EFI-A`,
/// `ROOT-B`, `DATA-A`, etc.).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Bios,
    EfiA,
    EfiB,
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

impl Part {
    /// Return the shell's short name for this slot (used as an assoc-array key).
    pub const fn short_name(&self) -> &'static str {
        match self {
            Part::Bios => "BIOS",
            Part::EfiA => "EFI-A",
            Part::EfiB => "EFI-B",
            Part::BootA => "BOOT-A",
            Part::BootB => "BOOT-B",
            Part::RootA => "ROOT-A",
            Part::RootB => "ROOT-B",
            Part::HashA => "HASH-A",
            Part::HashB => "HASH-B",
            Part::ReservedA => "RESERVED-A",
            Part::ReservedB => "RESERVED-B",
            Part::Private => "PRIVATE",
            Part::DataA => "DATA-A",
            Part::DataB => "DATA-B",
        }
    }
}

/// One partition entry in an assembled layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionEntry {
    pub part: Part,
    /// Offset from the start of the disk, in MiB.
    pub offset_mib: u32,
    /// Size in MiB.
    pub size_mib: u32,
}

/// Complete partition layout for one disk. Entries are in on-disk order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionLayout {
    pub entries: Vec<PartitionEntry>,
}

impl PartitionLayout {
    /// Locate a partition by kind.
    pub fn get(&self, part: Part) -> Option<&PartitionEntry> {
        self.entries.iter().find(|e| e.part == part)
    }

    /// Human-readable rendering, one line per entry, for golden fixtures.
    pub fn render(&self) -> String {
        let mut s = String::new();
        for e in &self.entries {
            let line = format!(
                "{:<10} offset={:>6}  size={:>6}\n",
                e.part.short_name(),
                e.offset_mib,
                e.size_mib
            );
            s.push_str(&line);
        }
        s
    }
}

impl fmt::Display for PartitionLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Result of computing a full disk layout for a variant image. When the
/// plan is `Split`, `data_disk` holds the layout of the separate data
/// disk; otherwise it is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskLayout {
    /// The OS image layout. Always populated.
    pub os_disk: PartitionLayout,
    /// Optional secondary disk holding DATA-B, present only under
    /// `Split`.
    pub data_disk: Option<PartitionLayout>,
}

// -------------------------------------------------------------------
// Label / type / uuid accessors.

/// Return the GPT partition label the shell would assign
/// (`set_partition_labels`). Data partitions are unlabeled (labeling
/// happens at first boot); returns `Some("")` for those and `None` for
/// slots the shell doesn't label at all.
pub fn partition_label(part: Part) -> Option<&'static str> {
    match part {
        Part::Bios => Some("BIOS-BOOT"),
        Part::EfiA => Some("EFI-SYSTEM"),
        Part::EfiB => Some("EFI-BACKUP"),
        Part::DataA => Some(""),
        Part::DataB => Some(""),
        Part::Private => Some("BOTTLEROCKET-PRIVATE"),
        Part::BootA => Some("BOTTLEROCKET-BOOT-A"),
        Part::BootB => Some("BOTTLEROCKET-BOOT-B"),
        Part::RootA => Some("BOTTLEROCKET-ROOT-A"),
        Part::RootB => Some("BOTTLEROCKET-ROOT-B"),
        Part::HashA => Some("BOTTLEROCKET-HASH-A"),
        Part::HashB => Some("BOTTLEROCKET-HASH-B"),
        Part::ReservedA => Some("BOTTLEROCKET-RESERVED-A"),
        Part::ReservedB => Some("BOTTLEROCKET-RESERVED-B"),
    }
}

/// Return the GPT typecode string the shell assigns
/// (`set_partition_types`).
pub fn partition_typecode(part: Part) -> &'static str {
    match part {
        Part::Bios => BIOS_BOOT_TYPECODE,
        Part::EfiA => EFI_SYSTEM_TYPECODE,
        Part::EfiB => EFI_BACKUP_TYPECODE,
        Part::Private => BOTTLEROCKET_PRIVATE_TYPECODE,
        Part::DataA | Part::DataB => BOTTLEROCKET_DATA_TYPECODE,
        Part::BootA | Part::BootB => BOTTLEROCKET_BOOT_TYPECODE,
        Part::RootA | Part::RootB => BOTTLEROCKET_ROOT_TYPECODE,
        Part::HashA | Part::HashB => BOTTLEROCKET_HASH_TYPECODE,
        Part::ReservedA | Part::ReservedB => BOTTLEROCKET_RESERVED_TYPECODE,
    }
}

/// Return the PARTUUID the shell assigns to `DATA-A`/`DATA-B` for the
/// given plan (`set_partition_uuids`). Under `Split`, DATA-B is the
/// preferred partition; under `Unified` the roles are reversed. Every
/// other partition uses `sgdisk`'s auto-generated UUID and this function
/// returns `None`.
pub fn partition_uuid(part: Part, plan: PartitionPlan) -> Option<&'static str> {
    match (part, plan) {
        (Part::DataA, PartitionPlan::Split) => Some(BOTTLEROCKET_DATA_FALLBACK_PARTGUID),
        (Part::DataB, PartitionPlan::Split) => Some(BOTTLEROCKET_DATA_PREFERRED_PARTGUID),
        (Part::DataA, PartitionPlan::Unified) => Some(BOTTLEROCKET_DATA_PREFERRED_PARTGUID),
        (Part::DataB, PartitionPlan::Unified) => Some(BOTTLEROCKET_DATA_FALLBACK_PARTGUID),
        _ => None,
    }
}

// -------------------------------------------------------------------
// Layout builders.

/// Compute the on-disk partition layout for a Bottlerocket variant
/// image, given a resolved [`ImageLayout`] and feature set.
///
/// Mirrors `partyplanner::set_partition_sizes` (both the regular
/// dual-bank path and the stripped-down `first_party_stack=no` fast
/// path). See the ASCII-art layouts in `partyplanner` §4 for the
/// arithmetic reference.
pub fn compute_partition_layout(
    layout: &ImageLayout,
    features: &ImageFeatures,
) -> Result<DiskLayout, Error> {
    let os_gib = layout.os_image_size_gib().0 as u32;
    let data_gib = layout.data_image_size_gib.0 as u32;
    let plan = layout.partition_plan;
    let in_place = features.in_place_updates;
    let first_party = features.first_party_stack;

    if os_gib == 0 {
        return Err(Error::Partition {
            plan: format!("{plan:?}"),
            feature: "os_image_size_gib".into(),
            reason: "must be > 0".into(),
        });
    }
    if data_gib == 0 {
        return Err(Error::Partition {
            plan: format!("{plan:?}"),
            feature: "data_image_size_gib".into(),
            reason: "must be > 0".into(),
        });
    }

    // Small helpers so every scaled/added size is checked. A `u32`
    // holds ~4 TiB in MiB units; realistic image sizes are far below
    // that, but the port carries `set -u` semantics from the shell:
    // any silent wrap here would produce a plausible-looking but
    // wrong GPT.
    let plan_str = format!("{plan:?}");
    let scale = |factor: u32, name: &str| -> Result<u32, Error> {
        os_gib.checked_mul(factor).ok_or_else(|| Error::Partition {
            plan: plan_str.clone(),
            feature: name.to_string(),
            reason: format!("os_gib ({os_gib}) * {name} ({factor}) overflowed u32"),
        })
    };
    let add = |a: u32, b: u32, name: &str| -> Result<u32, Error> {
        a.checked_add(b).ok_or_else(|| Error::Partition {
            plan: plan_str.clone(),
            feature: name.to_string(),
            reason: format!("offset overflow while placing {name} ({a} + {b})"),
        })
    };
    let mul = |a: u32, b: u32, name: &str| -> Result<u32, Error> {
        a.checked_mul(b).ok_or_else(|| Error::Partition {
            plan: plan_str.clone(),
            feature: name.to_string(),
            reason: format!("size overflow while doubling {name} ({a} * {b})"),
        })
    };

    // Per-GiB scaled partition sizes.
    let boot_mib = scale(BOOT_SCALE_FACTOR, "boot_scale")?;
    let root_mib = scale(ROOT_SCALE_FACTOR, "root_scale")?;
    let hash_mib = scale(HASH_SCALE_FACTOR, "hash_scale")?;

    if !first_party {
        // Fast path: stripped-down single-bank layout. ROOT-A is the
        // residual after every fixed and scaled partition is placed.
        if in_place {
            return Err(Error::Partition {
                plan: format!("{plan:?}"),
                feature: "first_party_stack=no".into(),
                reason: "incompatible with in_place_updates=yes".into(),
            });
        }
        if matches!(plan, PartitionPlan::Split) {
            return Err(Error::Partition {
                plan: format!("{plan:?}"),
                feature: "first_party_stack=no".into(),
                reason: "incompatible with partition_plan=split".into(),
            });
        }

        let total_mib = scale(1024, "os_image_size_gib_to_mib")?;

        let mut offset = GPT_MIB; // skip GPT header
        let mut entries = Vec::new();

        entries.push(PartitionEntry {
            part: Part::Bios,
            offset_mib: offset,
            size_mib: BIOS_MIB,
        });
        offset = add(offset, BIOS_MIB, "BIOS")?;

        let efi_a_size = mul(EFI_MIB, 2, "EFI-A*2")?;
        entries.push(PartitionEntry {
            part: Part::EfiA,
            offset_mib: offset,
            size_mib: efi_a_size,
        });
        offset = add(offset, efi_a_size, "EFI-A")?;

        let boot_a_size = mul(boot_mib, 2, "BOOT-A*2")?;
        entries.push(PartitionEntry {
            part: Part::BootA,
            offset_mib: offset,
            size_mib: boot_a_size,
        });
        offset = add(offset, boot_a_size, "BOOT-A")?;

        // ROOT-A absorbs the leftover: total - GPT(header) - BIOS -
        // EFI-A*2 - BOOT-A*2 - HASH-A*2 - GPT(footer).
        let hash_a_size = mul(hash_mib, 2, "HASH-A*2")?;
        let root_a_mib = total_mib
            .checked_sub(GPT_MIB)
            .and_then(|v| v.checked_sub(BIOS_MIB))
            .and_then(|v| v.checked_sub(efi_a_size))
            .and_then(|v| v.checked_sub(boot_a_size))
            .and_then(|v| v.checked_sub(hash_a_size))
            .and_then(|v| v.checked_sub(GPT_MIB))
            .ok_or_else(|| Error::Partition {
                plan: plan_str.clone(),
                feature: "first_party_stack=no".into(),
                reason: format!("image size {os_gib} GiB is too small for stripped layout"),
            })?;
        if root_a_mib == 0 {
            return Err(Error::Partition {
                plan: plan_str.clone(),
                feature: "first_party_stack=no".into(),
                reason: format!("image size {os_gib} GiB leaves no room for ROOT-A"),
            });
        }

        entries.push(PartitionEntry {
            part: Part::RootA,
            offset_mib: offset,
            size_mib: root_a_mib,
        });
        offset = add(offset, root_a_mib, "ROOT-A")?;

        entries.push(PartitionEntry {
            part: Part::HashA,
            offset_mib: offset,
            size_mib: hash_a_size,
        });
        offset = add(offset, hash_a_size, "HASH-A")?;

        // Sanity: offsets + GPT footer must equal total_mib.
        let end = add(offset, GPT_MIB, "GPT-footer")?;
        if end != total_mib {
            return Err(Error::Partition {
                plan: plan_str.clone(),
                feature: "first_party_stack=no".into(),
                reason: format!("offset mismatch: offset={offset}, total_mib={total_mib}"),
            });
        }

        return Ok(DiskLayout {
            os_disk: PartitionLayout { entries },
            data_disk: None,
        });
    }

    // First-party-stack: standard layout.
    // `reserved_mib` = os_gib * RESERVE_SCALE_FACTOR - EFI_MIB, but
    // subtraction must be checked so a change to the scale factor
    // constants can't silently produce an underflow / debug-build panic.
    let reserved_pre = scale(RESERVE_SCALE_FACTOR, "reserve_scale")?;
    let reserved_mib = reserved_pre.checked_sub(EFI_MIB).ok_or_else(|| Error::Partition {
        plan: plan_str.clone(),
        feature: "reserved_mib".into(),
        reason: format!(
            "os_gib ({os_gib}) * RESERVE_SCALE_FACTOR ({RESERVE_SCALE_FACTOR}) = {reserved_pre} \
             is smaller than EFI_MIB ({EFI_MIB})"
        ),
    })?;
    let private_pre = scale(PRIVATE_SCALE_FACTOR, "private_scale")?;
    let private_mib = private_pre
        .checked_sub(OVERHEAD_MIB)
        .and_then(|v| v.checked_sub(DATA_A_MIB))
        .ok_or_else(|| Error::Partition {
            plan: plan_str.clone(),
            feature: "private_mib".into(),
            reason: format!(
                "os_gib ({os_gib}) * PRIVATE_SCALE_FACTOR ({PRIVATE_SCALE_FACTOR}) = {private_pre} \
                 leaves no room after OVERHEAD_MIB ({OVERHEAD_MIB}) + DATA_A_MIB ({DATA_A_MIB})"
            ),
        })?;

    let mut offset = GPT_MIB;
    let mut entries = Vec::new();

    entries.push(PartitionEntry {
        part: Part::Bios,
        offset_mib: offset,
        size_mib: BIOS_MIB,
    });
    offset = add(offset, BIOS_MIB, "BIOS")?;

    if in_place {
        // Two banks, each partition sized once.
        for (efi_p, boot_p, root_p, hash_p, res_p) in [
            (
                Part::EfiA,
                Part::BootA,
                Part::RootA,
                Part::HashA,
                Part::ReservedA,
            ),
            (
                Part::EfiB,
                Part::BootB,
                Part::RootB,
                Part::HashB,
                Part::ReservedB,
            ),
        ] {
            entries.push(PartitionEntry {
                part: efi_p,
                offset_mib: offset,
                size_mib: EFI_MIB,
            });
            offset = add(offset, EFI_MIB, "EFI")?;

            entries.push(PartitionEntry {
                part: boot_p,
                offset_mib: offset,
                size_mib: boot_mib,
            });
            offset = add(offset, boot_mib, "BOOT")?;

            entries.push(PartitionEntry {
                part: root_p,
                offset_mib: offset,
                size_mib: root_mib,
            });
            offset = add(offset, root_mib, "ROOT")?;

            entries.push(PartitionEntry {
                part: hash_p,
                offset_mib: offset,
                size_mib: hash_mib,
            });
            offset = add(offset, hash_mib, "HASH")?;

            entries.push(PartitionEntry {
                part: res_p,
                offset_mib: offset,
                size_mib: reserved_mib,
            });
            offset = add(offset, reserved_mib, "RESERVED")?;
        }
    } else {
        // Single bank, each scaled partition doubled.
        let efi_size = mul(EFI_MIB, 2, "EFI-A*2")?;
        entries.push(PartitionEntry {
            part: Part::EfiA,
            offset_mib: offset,
            size_mib: efi_size,
        });
        offset = add(offset, efi_size, "EFI-A")?;

        let boot_size = mul(boot_mib, 2, "BOOT-A*2")?;
        entries.push(PartitionEntry {
            part: Part::BootA,
            offset_mib: offset,
            size_mib: boot_size,
        });
        offset = add(offset, boot_size, "BOOT-A")?;

        let root_size = mul(root_mib, 2, "ROOT-A*2")?;
        entries.push(PartitionEntry {
            part: Part::RootA,
            offset_mib: offset,
            size_mib: root_size,
        });
        offset = add(offset, root_size, "ROOT-A")?;

        let hash_size = mul(hash_mib, 2, "HASH-A*2")?;
        entries.push(PartitionEntry {
            part: Part::HashA,
            offset_mib: offset,
            size_mib: hash_size,
        });
        offset = add(offset, hash_size, "HASH-A")?;

        let reserved_size = mul(reserved_mib, 2, "RESERVED-A*2")?;
        entries.push(PartitionEntry {
            part: Part::ReservedA,
            offset_mib: offset,
            size_mib: reserved_size,
        });
        offset = add(offset, reserved_size, "RESERVED-A")?;
    }

    entries.push(PartitionEntry {
        part: Part::Private,
        offset_mib: offset,
        size_mib: private_mib,
    });
    offset = add(offset, private_mib, "PRIVATE")?;

    // data_gib is validated > 0 above; check every arithmetic step.
    let data_mib = data_gib.checked_mul(1024).ok_or_else(|| Error::Partition {
        plan: plan_str.clone(),
        feature: "data_image_size_gib_to_mib".into(),
        reason: format!("data_gib ({data_gib}) * 1024 overflowed u32"),
    })?;

    let data_disk = match plan {
        PartitionPlan::Split => {
            // DATA-A on the OS disk (fallback partition, size DATA_A_MIB).
            entries.push(PartitionEntry {
                part: Part::DataA,
                offset_mib: offset,
                size_mib: DATA_A_MIB,
            });

            // DATA-B lives on a separate data disk. First+last MiB
            // reserved for GPT; the rest is DATA-B.
            let gpt_overhead = mul(GPT_MIB, 2, "GPT overhead")?;
            let data_b_size = data_mib.checked_sub(gpt_overhead).ok_or_else(|| Error::Partition {
                plan: plan_str.clone(),
                feature: "data_image_size_gib".into(),
                reason: format!(
                    "data_gib ({data_gib}) is too small to fit GPT overhead ({gpt_overhead} MiB)"
                ),
            })?;
            Some(PartitionLayout {
                entries: vec![PartitionEntry {
                    part: Part::DataB,
                    offset_mib: GPT_MIB,
                    size_mib: data_b_size,
                }],
            })
        }
        PartitionPlan::Unified => {
            // DATA-A absorbs the full data-image space on the unified disk.
            entries.push(PartitionEntry {
                part: Part::DataA,
                offset_mib: offset,
                size_mib: data_mib,
            });
            None
        }
    };

    Ok(DiskLayout {
        os_disk: PartitionLayout { entries },
        data_disk,
    })
}

/// Compute the minimal EIF sidecar disk-image layout: a 2-partition GPT
/// with just ROOT-A (erofs rootfs) and HASH-A (dm-verity hash tree).
///
/// - `rootfs_mib` is the rootfs size (rounded up to a whole MiB).
/// - `verity_mib` is the reserved size of the verity hash tree.
/// - `target_mib` (when non-zero) pads ROOT-A upward so the disk-image
///   totals exactly `target_mib`. Must be at least `rootfs_mib +
///   verity_mib + 2` MiB (2 MiB of GPT overhead). When `0`, the disk is
///   tight-fitted to the content.
pub fn eif_partition_layout(
    rootfs_mib: u32,
    verity_mib: u32,
    target_mib: u32,
) -> Result<PartitionLayout, Error> {
    if rootfs_mib == 0 || verity_mib == 0 {
        return Err(Error::Partition {
            plan: "eif".into(),
            feature: "rootfs_mib/verity_mib".into(),
            reason: format!(
                "rootfs_mib and verity_mib must be > 0 (got rootfs={rootfs_mib}, verity={verity_mib})"
            ),
        });
    }

    let minimum_mib = rootfs_mib
        .checked_add(verity_mib)
        .and_then(|v| v.checked_add(2))
        .ok_or_else(|| Error::Partition {
            plan: "eif".into(),
            feature: "minimum_mib".into(),
            reason: format!(
                "rootfs_mib ({rootfs_mib}) + verity_mib ({verity_mib}) + 2 overflowed u32"
            ),
        })?;
    let root_a_mib = if target_mib > 0 {
        if target_mib < minimum_mib {
            return Err(Error::Partition {
                plan: "eif".into(),
                feature: "target_mib".into(),
                reason: format!(
                    "target {target_mib} MiB is smaller than required minimum {minimum_mib} MiB \
                     (rootfs={rootfs_mib}, verity={verity_mib}, GPT overhead=2)"
                ),
            });
        }
        // Both subtractions are safe because target_mib >= minimum_mib.
        target_mib - verity_mib - 2
    } else {
        rootfs_mib
    };

    let root_end = 1u32
        .checked_add(root_a_mib)
        .ok_or_else(|| Error::Partition {
            plan: "eif".into(),
            feature: "root_end".into(),
            reason: format!("1 + root_a_mib ({root_a_mib}) overflowed u32"),
        })?;
    let hash_end = root_end
        .checked_add(verity_mib)
        .ok_or_else(|| Error::Partition {
            plan: "eif".into(),
            feature: "hash_end".into(),
            reason: format!("root_end ({root_end}) + verity_mib ({verity_mib}) overflowed u32"),
        })?;

    let entries = vec![
        PartitionEntry {
            part: Part::RootA,
            offset_mib: 1,
            size_mib: root_a_mib,
        },
        PartitionEntry {
            part: Part::HashA,
            offset_mib: root_end,
            size_mib: verity_mib,
        },
    ];

    let expected_total = if target_mib > 0 {
        target_mib
    } else {
        minimum_mib
    };
    if hash_end + 1 != expected_total {
        return Err(Error::Partition {
            plan: "eif".into(),
            feature: "offsets".into(),
            reason: format!(
                "offset mismatch: hash_end={hash_end}, expected_total={expected_total}"
            ),
        });
    }

    Ok(PartitionLayout { entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImageFeatures, ImageLayout, PartitionPlan};

    fn features(first_party: bool, in_place: bool) -> ImageFeatures {
        ImageFeatures {
            first_party_stack: first_party,
            in_place_updates: in_place,
            ..ImageFeatures::default()
        }
    }

    #[test]
    fn split_1gib_in_place_yes() {
        // os=1, data=1, split, in_place=yes, first_party=yes
        let l = ImageLayout::new(Some(1), 1, 22, PartitionPlan::Split);
        let f = features(true, true);
        let disk = compute_partition_layout(&l, &f).unwrap();
        let rendered = disk.os_disk.render();
        assert_eq!(
            rendered,
            include_str!("../tests/fixtures/layout-split-1gib-inplace-yes.txt")
        );
        // Data disk exists and holds DATA-B at MiB 1, size = 1 GiB - 2 MiB.
        let data = disk.data_disk.as_ref().expect("split has a data disk");
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].part, Part::DataB);
        assert_eq!(data.entries[0].offset_mib, 1);
        assert_eq!(data.entries[0].size_mib, 1024 - 2);
    }

    #[test]
    fn unified_1gib_in_place_no() {
        // os=1, data=1, unified, in_place=no, first_party=yes
        let l = ImageLayout::new(Some(1), 1, 22, PartitionPlan::Unified);
        let f = features(true, false);
        let disk = compute_partition_layout(&l, &f).unwrap();
        let rendered = disk.os_disk.render();
        assert_eq!(
            rendered,
            include_str!("../tests/fixtures/layout-unified-1gib-inplace-no.txt")
        );
        assert!(
            disk.data_disk.is_none(),
            "unified has no separate data disk"
        );
    }

    #[test]
    fn stripped_1gib() {
        // first_party=no, unified, in_place=no
        let l = ImageLayout::new(Some(1), 1, 22, PartitionPlan::Unified);
        let f = features(false, false);
        let disk = compute_partition_layout(&l, &f).unwrap();
        let rendered = disk.os_disk.render();
        assert_eq!(
            rendered,
            include_str!("../tests/fixtures/layout-stripped-1gib.txt")
        );
        assert!(disk.data_disk.is_none());
    }

    #[test]
    fn stripped_rejects_in_place() {
        let l = ImageLayout::new(Some(1), 1, 22, PartitionPlan::Unified);
        let f = features(false, true);
        let err = compute_partition_layout(&l, &f).unwrap_err();
        assert!(matches!(err, Error::Partition { .. }));
        assert!(err.to_string().contains("in_place_updates"));
    }

    #[test]
    fn stripped_rejects_split() {
        let l = ImageLayout::new(Some(1), 1, 22, PartitionPlan::Split);
        let f = features(false, false);
        let err = compute_partition_layout(&l, &f).unwrap_err();
        assert!(err.to_string().contains("split"));
    }

    #[test]
    fn eif_tight_100mib() {
        let layout = eif_partition_layout(100, 5, 0).unwrap();
        let rendered = layout.render();
        assert_eq!(
            rendered,
            include_str!("../tests/fixtures/layout-eif-tight-100mib.txt")
        );
    }

    #[test]
    fn eif_padded_200mib() {
        let layout = eif_partition_layout(100, 5, 200).unwrap();
        let rendered = layout.render();
        assert_eq!(
            rendered,
            include_str!("../tests/fixtures/layout-eif-padded-200mib.txt")
        );
    }

    #[test]
    fn eif_target_smaller_than_content_errors() {
        let err = eif_partition_layout(100, 5, 50).unwrap_err();
        assert!(err.to_string().contains("smaller"));
    }

    #[test]
    fn eif_zero_sizes_error() {
        assert!(eif_partition_layout(0, 5, 0).is_err());
        assert!(eif_partition_layout(100, 0, 0).is_err());
    }

    #[test]
    fn data_gib_zero_is_rejected() {
        // Setting data_image_size_gib = 0 must be an explicit error, not
        // silently produce a 0-sized DATA partition.
        let l = ImageLayout::new(Some(1), 0, 22, PartitionPlan::Unified);
        let f = features(true, false);
        let err = compute_partition_layout(&l, &f).unwrap_err();
        match err {
            Error::Partition { feature, .. } => {
                assert!(feature.contains("data_image_size_gib"), "feature={feature}");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn max_os_gib_reports_typed_error_not_panic() {
        // ImageSize is a u16, so os_gib maxes at 65535. At that size
        // reserved * 2 easily overflows the ~30M private/reserved
        // budget. Assert we return a typed Partition error rather than
        // panicking in a debug build or wrapping in release.
        let l = ImageLayout::new(Some(u16::MAX), 1, 22, PartitionPlan::Unified);
        let f = features(true, false);
        // Either the layout is unrepresentable (typed error) or every
        // step fits — but never a panic or silent wrap.
        let res = compute_partition_layout(&l, &f);
        if let Err(err) = res {
            assert!(matches!(err, Error::Partition { .. }), "err={err:?}");
        }
    }

    #[test]
    fn partition_typecode_covers_every_slot() {
        for part in [
            Part::Bios,
            Part::EfiA,
            Part::EfiB,
            Part::BootA,
            Part::BootB,
            Part::RootA,
            Part::RootB,
            Part::HashA,
            Part::HashB,
            Part::ReservedA,
            Part::ReservedB,
            Part::Private,
            Part::DataA,
            Part::DataB,
        ] {
            let tc = partition_typecode(part);
            assert!(!tc.is_empty(), "{part:?} typecode empty");
        }
    }

    #[test]
    fn partition_uuid_swaps_by_plan() {
        // Under split, DATA-B is preferred; under unified, DATA-A is preferred.
        assert_eq!(
            partition_uuid(Part::DataB, PartitionPlan::Split),
            Some(BOTTLEROCKET_DATA_PREFERRED_PARTGUID)
        );
        assert_eq!(
            partition_uuid(Part::DataA, PartitionPlan::Split),
            Some(BOTTLEROCKET_DATA_FALLBACK_PARTGUID)
        );
        assert_eq!(
            partition_uuid(Part::DataA, PartitionPlan::Unified),
            Some(BOTTLEROCKET_DATA_PREFERRED_PARTGUID)
        );
        assert_eq!(
            partition_uuid(Part::DataB, PartitionPlan::Unified),
            Some(BOTTLEROCKET_DATA_FALLBACK_PARTGUID)
        );
        assert_eq!(partition_uuid(Part::RootA, PartitionPlan::Split), None);
    }

    #[test]
    fn partition_label_is_bottlerocket_prefixed_for_bank_partitions() {
        assert_eq!(partition_label(Part::BootA), Some("BOTTLEROCKET-BOOT-A"));
        assert_eq!(partition_label(Part::RootB), Some("BOTTLEROCKET-ROOT-B"));
        assert_eq!(
            partition_label(Part::ReservedA),
            Some("BOTTLEROCKET-RESERVED-A")
        );
        assert_eq!(partition_label(Part::Bios), Some("BIOS-BOOT"));
        assert_eq!(partition_label(Part::EfiA), Some("EFI-SYSTEM"));
        assert_eq!(partition_label(Part::EfiB), Some("EFI-BACKUP"));
        assert_eq!(partition_label(Part::DataA), Some(""));
    }
}
