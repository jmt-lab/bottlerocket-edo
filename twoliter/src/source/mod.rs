//! Twoliter-specific edo sources.
//!
//! Provides `kit-image` — an OCI-image source that layers Bottlerocket
//! kit metadata verification on top of edo-core's plain `oci` source, and
//! a matching vendor kind that decodes the same metadata label to expose
//! transitive kit + SDK dependencies to the resolver.

pub mod kit_image;
pub mod kit_image_vendor;

pub use kit_image::{KIT_METADATA_LABEL, KitImageSource, KitMetadata, KitRef};
pub use kit_image_vendor::KitImageVendor;
