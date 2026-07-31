//! The `variant-*` transform family: assemble a Bottlerocket variant
//! disk image from an SDK, kits, extra RPMs, and optional guest
//! images.
//!
//! The user-visible artifact — `os.img.lz4` + friends — is produced by
//! a chain of four independently-cached edo transforms:
//!
//! | Kind | Address suffix | Produces |
//! |---|---|---|
//! | `variant-rootfs` | `.../rootfs` | tar of `root-mount/` + metadata |
//! | `variant-disk` | `.../disk` | raw partition scratches + `os.img.partial` |
//! | `variant-grub` | `.../grub` | finalised `os.img` + boot-config |
//! | `variant-image` | `<addr>` (unchanged) | eight-layer final artifact |
//!
//! Each stage is its own `TransformImpl` with a hand-written
//! `FromElement`. The finalizer's address is the top-level variant
//! address (e.g. `//variants/aws-k8s-1.29`) so downstream tooling
//! addressing the variant sees no change. Sub-stages sit under
//! `<addr>/rootfs`, `<addr>/disk`, `<addr>/grub`.
//!
//! **Where the split lives.**
//! - [`options`]: the `[variant]` TOML block, shared by every stage.
//! - [`hash`]: [`HashInputs`](hash::HashInputs) plus one subset-hash
//!   function per stage. Keeps hash-input drift review-friendly (one
//!   place to add fields, four places to update selection).
//! - [`layout`]: layout math ([`image_layout`](layout::image_layout),
//!   [`parse_arch`](layout::parse_arch),
//!   [`partition_entry`](layout::partition_entry)).
//! - [`scope`]: source-scope helpers (`resolve_optional`,
//!   `pick_first_in`, `resolve_file_contexts`, `copy_partition`).
//! - [`media_types`]: intermediate + final artifact media types.
//! - [`error`]: shared error type for every stage.
//! - [`image`]: [`VariantImageTransform`], the finalizer.

pub mod disk;
pub mod error;
pub mod grub;
pub mod hash;
pub mod image;
pub mod layout;
pub mod media_types;
pub mod options;
pub mod rootfs;
pub mod sbom;
pub mod scope;
pub mod secure_boot;
pub mod staging;

pub use disk::VariantDiskTransform;
pub use grub::VariantGrubTransform;
pub use image::VariantImageTransform;
pub use rootfs::VariantRootfsTransform;
