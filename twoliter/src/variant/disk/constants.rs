use crate::variant::PartitionPlan;

/// Historical default OS-image size when the manifest does not specify.
pub const DEFAULT_OS_IMAGE_SIZE_GIB: u16 = 2u16;
/// Default OS-image size when `standalone-image = true` (stripped image).
pub const DEFAULT_NO_FIRST_PARTY_OS_IMAGE_SIZE_GIB: u16 = 1u16;
pub const DEFAULT_DATA_IMAGE_SIZE_GIB: u16 = 1u16;
pub const DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB: u16 = 22u16;
pub const DEFAULT_PARTITION_PLAN: PartitionPlan = PartitionPlan::Split;

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
