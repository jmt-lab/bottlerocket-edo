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

use edo::context::Element;
use serde_json::json;

/// Return a clone of `element` with a `"platform"` config entry set to
/// the OCI-style translation of `target_arch`.
///
/// Kit-image sources need to know the caller's target architecture so
/// [`KitImageSource::get_unique_id`] can tag the artifact `Id` per-arch
/// and so [`KitImageSource::fetch`] picks the right platform's image
/// manifest out of the multi-arch index. The vendor's `resolve()`
/// emits the element without a platform field (it works off the whole
/// index), so upstream transforms — `variant-rootfs`, `package` — apply
/// this before registering the source.
///
/// A pre-existing `platform` key is preserved (the caller has already
/// been explicit), and an unknown `target_arch` is passed through
/// verbatim so the misuse surfaces as a legible OCI error rather than
/// a silent reinterpretation.
pub fn apply_target_arch(element: &Element, target_arch: &str) -> Element {
    let mut scoped = element.clone();
    if !scoped.config.contains_key("platform") {
        let platform = kit_image::twoliter_arch_to_oci_platform(target_arch);
        scoped
            .config
            .insert("platform".to_string(), json!(platform));
    }
    scoped
}
