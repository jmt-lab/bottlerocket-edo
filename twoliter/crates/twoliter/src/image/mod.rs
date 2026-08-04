//! `image`: shared primitives for building Bottlerocket disk images.
//!
//! This module is a plain Rust library consumed by twoliter transforms.
//! Every I/O and subprocess operation runs through an
//! [`edo::environment::Vfs`] handle, so the module operates against the
//! transform's sandboxed environment rather than the host filesystem.
//! Callers construct a [`Vfs`](edo::environment::Vfs) from the
//! transform's [`Environment`](edo::environment::Environment),
//! [`Id`](edo::storage::Id), and [`Log`](edo::context::Log) and pass it
//! to `image` functions; the functions dispatch mkfs, dd,
//! veritysetup, dnf, and friends inside that environment.
//!
//! Submodules:
//!
//! - [`model`] / [`layout`] — pure types and layout resolution
//!   (`SupportedArch`, `ImageFormat`, `PartitionPlan`, `ImageLayout`,
//!   `ImageFeature`) plus `resolved_image_layout` and
//!   `validate_image_features`.
//! - [`partition`] — GPT layout math and partition-type/label/UUID
//!   accessors (port of `partyplanner`).
//! - [`compress`] — image-artifact suffix allowlist and compressor
//!   dispatch (port of `imghelper::compress_image` /
//!   `decompress_image`).
//! - [`verity`] — dm-verity hash-tree computation.
//! - [`secure_boot`] — cert-table encoding + shim/grub/kernel signing.
//! - [`grub`] — grub install + grub.cfg emission (non-signing).
//! - [`image`] / [`rpm_install`] / [`sbom`] / [`ovf`] / [`selinux`] /
//!   [`bootconfig`] / [`guest`] / [`metadata`] — the remaining ported
//!   pieces of `imghelper` and `guest-images-helper`.
//! - [`vfs_ext`] — small helpers layered over `edo::environment::Vfs`
//!   (best-effort remove, `stat`-based size lookup, `find`-based
//!   directory listing).
//!
//! See `doc/image-building.md` for the design spec.

pub mod bootconfig;
pub mod compress;
pub mod error;
pub mod grub;
pub mod guest;
pub mod image;
pub mod layout;
pub mod metadata;
pub mod model;
pub mod ovf;
pub mod partition;
pub mod rpm_install;
pub mod sbom;
pub mod secure_boot;
pub mod selinux;
pub mod verity;
pub mod vfs_ext;

// Data assets kept alongside the crate; consumers `include_str!` these
// indirectly via the constants below.

/// The `builder-group.spec` file used to synthesize an RPM that
/// `Provides: group(builder)`. Ported verbatim from
/// `old-twoliter/twoliter/embedded/builder-group.spec`.
pub const BUILDER_GROUP_SPEC: &str = include_str!("../../data/builder-group.spec");

/// The `metadata.spec` file used to build the per-variant metadata RPM.
/// Ported verbatim from `old-twoliter/twoliter/embedded/metadata.spec`.
pub const METADATA_SPEC: &str = include_str!("../../data/metadata.spec");

pub use compress::ArtifactSuffix;
pub use error::{Error, Result};
pub use grub::{GrubUefiInstall, install_grub_uefi};
pub use image::{StageRootInputs, stage_root};
pub use layout::{resolved_image_layout, validate_image_features};
pub use metadata::{
    BuildMetadataRpmInputs, RootfsMetadata, VariantManifest, VariantNameError, VariantSegments,
    build_metadata_rpm,
};
pub use model::{
    Arch, DEFAULT_DATA_IMAGE_SIZE_GIB, DEFAULT_NO_FIRST_PARTY_OS_IMAGE_SIZE_GIB,
    DEFAULT_OS_IMAGE_SIZE_GIB, DEFAULT_PARTITION_PLAN, DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB,
    ImageFeature, ImageFeatures, ImageFormat, ImageLayout, ImageSize, PartitionPlan, SupportedArch,
};
pub use ovf::{assemble_ova, render_ova_manifest, sha256_of};
pub use partition::{DiskLayoutManifest, apply_partition_layout, render_sgdisk_argv};
