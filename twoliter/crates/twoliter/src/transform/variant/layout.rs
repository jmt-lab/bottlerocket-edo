//! Layout math shared by every `variant-*` transform.
//!
//! Consists of:
//! - [`image_layout`]: assembles a caller-facing [`ImageLayout`] from a
//!   [`VariantImageOptions`] snapshot, folding in br-image's defaults
//!   for anything the caller left unset. Used by every stage that
//!   needs to know how big the OS/data images are or whether the plan
//!   is Split/Unified.
//! - [`parse_arch`]: parses the `arch` option string as a typed
//!   [`Arch`]. Unlike the free-standing helper in br-image, this
//!   returns a typed transform error so callers see a helpful message
//!   when a variant declares a bogus arch.
//! - [`partition_entry`]: locates a specific [`Part`] in a
//!   [`DiskLayout`], returning a typed error when the layout doesn't
//!   include the requested slot (a "should be impossible" condition
//!   we still surface cleanly).

use crate::image::{
    Arch, ImageLayout,
    partition::{DiskLayout, Part, PartitionEntry},
};
use snafu::OptionExt;

use super::error;
use super::options::VariantImageOptions;

/// Compose an [`ImageLayout`] from a [`VariantImageOptions`] snapshot,
/// folding in br-image's defaults for any field the caller left unset.
///
/// The publish-hint doubles as the `os_image_publish_size` seed; if
/// the caller specified a hint we use it, otherwise we fall back to
/// br-image's default.
pub fn image_layout(options: &VariantImageOptions) -> ImageLayout {
    let hint = options
        .os_image_publish_size_gib
        .or(options.data_image_publish_size_gib)
        .unwrap_or(crate::image::DEFAULT_PUBLISH_IMAGE_SIZE_HINT_GIB.0);
    let data = options
        .data_image_size_gib
        .unwrap_or(crate::image::DEFAULT_DATA_IMAGE_SIZE_GIB.0);
    ImageLayout::new(
        options.os_image_size_gib,
        data,
        hint,
        options.partition_plan,
    )
}

/// Parse the `arch` option string as an [`Arch`]. Returns a typed
/// [`error::Error::InvalidArch`] when the string is neither an x86_64
/// nor an aarch64 alias.
pub fn parse_arch(raw: Option<&str>) -> Result<Arch, error::Error> {
    let s = raw.unwrap_or(std::env::consts::ARCH);
    match s {
        "x86_64" | "amd64" => Ok(Arch::X86_64),
        "aarch64" | "arm64" => Ok(Arch::Aarch64),
        other => Err(error::Error::InvalidArch {
            arch: other.to_string(),
        }),
    }
}

/// Look up a specific partition slot in a fully-computed [`DiskLayout`].
///
/// Surface a typed error rather than panicking when the layout doesn't
/// contain the requested slot — this should be impossible for a
/// well-formed layout but we prefer a clean error message over an
/// `unwrap`.
pub fn partition_entry<'a>(
    disk_layout: &'a DiskLayout,
    part: Part,
) -> Result<&'a PartitionEntry, error::Error> {
    disk_layout
        .os_disk
        .get(part)
        .with_context(|| error::BuildStepSnafu {
            step: format!("partition-lookup:{}", part.short_name()),
            reason: format!("{} absent from computed OS-disk layout", part.short_name()),
        })
}
