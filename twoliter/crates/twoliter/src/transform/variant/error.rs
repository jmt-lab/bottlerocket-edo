//! Shared error type for every `variant-*` transform.
//!
//! Every transform in the `variant/` module tree reports failures
//! through this one enum so callers see a consistent shape whether the
//! failure came from `variant-rootfs`, `variant-disk`, `variant-grub`,
//! or the `variant-image` finalizer. The [`BuildStep`](Error::BuildStep)
//! variant is used by every transform for typed build-step failures;
//! its `step` field carries the enclosing transform's short name plus
//! the failing step (e.g. `"variant-disk::mkfs-root"`) so a log entry
//! locates the failure at a glance.

use edo::{
    context::{Addr, ContextError},
    transform::TransformError,
};
use snafu::Snafu;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(transparent)]
    Context { source: ContextError },
    #[snafu(display("dependent transform not found: {addr}"))]
    Depend { addr: Addr },
    #[snafu(display(
        "variant transform requires exactly one source pointing at the SDK image; none was declared"
    ))]
    MissingSdkSource,
    #[snafu(display(
        "variant transform requires exactly one source pointing at the SDK image; multiple sources were declared"
    ))]
    TooManySdkSources,
    #[snafu(display("br-image failure: {source}"))]
    BrImage {
        #[snafu(source(from(crate::image::Error, Box::new)))]
        source: Box<crate::image::Error>,
    },
    #[snafu(display("json serialization error: {source}"))]
    Serialization { source: serde_json::Error },
    #[snafu(display("unrecognized arch `{arch}` (expected `x86_64` or `aarch64`)"))]
    InvalidArch { arch: String },
    #[snafu(display("variant build step `{step}` failed: {reason}"))]
    BuildStep { step: String, reason: String },
    #[snafu(display(
        "variant transform source scope `{scope}` accepts at most one address; got {count}"
    ))]
    TooManyScopeSources { scope: String, count: usize },
}

impl From<Error> for TransformError {
    fn from(value: Error) -> Self {
        Self::Implementation {
            source: Box::new(value),
        }
    }
}

impl From<Error> for ContextError {
    fn from(value: Error) -> Self {
        Self::Component {
            source: Box::new(value),
        }
    }
}
