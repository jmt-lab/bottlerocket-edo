//! Top-level `br-image` error type.
//!
//! Every fallible operation in `br-image` runs against an
//! [`edo::environment::Vfs`], so I/O failures are surfaced as
//! [`edo::environment::EnvironmentError`] wrapped in the
//! [`Error::Env`] variant. The remaining variants describe pure
//! logic-layer failures (invalid layouts, missing artifacts, dm-verity
//! math errors) that do not originate in the environment.

use snafu::Snafu;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

// The Error variants themselves are `pub` (this is the crate's public
// error type), but the snafu-generated *context selectors*
// (`EnvSnafu`, etc.) are `pub(crate)`. Downstream consumers should
// match on the `Error` enum and read fields; they have no need to
// construct a br-image error via a selector.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// A failure surfaced by the underlying edo [`Environment`]. This
    /// covers every I/O and subprocess failure `br-image` can produce:
    /// missing files, non-zero exit statuses from staged commands,
    /// storage read/write errors, and template rendering.
    ///
    /// [`Environment`]: edo::environment::Environment
    #[snafu(transparent)]
    Env {
        source: edo::environment::EnvironmentError,
    },

    #[snafu(display("invalid image-features / layout combination for {context}:{reason}"))]
    InvalidLayout { context: String, reason: String },

    #[snafu(display(
        "invalid artifact path {}: expected suffix `{expected_suffix}`",
        path.display()
    ))]
    InvalidArtifact {
        path: PathBuf,
        expected_suffix: String,
    },

    #[snafu(display("dm-verity computation failed: {reason}"))]
    Verity { reason: String },

    #[snafu(display("partition layout error ({plan}, feature `{feature}`): {reason}"))]
    Partition {
        plan: String,
        feature: String,
        reason: String,
    },

    #[snafu(display("failed to parse `{what}` as an image-feature"))]
    ParseImageFeature { what: String },

    #[snafu(display("no guest image artifacts found under {}", src.display()))]
    NoGuestArtifacts { src: PathBuf },

    #[snafu(display("bootconfig snippet at {}: {reason}", path.display()))]
    BootconfigInvalid { path: PathBuf, reason: String },

    #[snafu(display("failed to parse output of `{tool}`: {reason}"))]
    ParseOutput { tool: String, reason: String },

    /// A code path that is intentionally not implemented in `br-image`
    /// because it depends on a capability edo's [`Vfs`] does not yet
    /// expose (typically stdin streaming into a child process).
    ///
    /// Surfacing this as a typed error rather than a silent skip
    /// forces callers to notice that a step the shell version performed
    /// is not happening, so a variant image cannot be produced with a
    /// silently-wrong filesystem-labeling state.
    ///
    /// [`Vfs`]: edo::environment::Vfs
    #[snafu(display("unsupported in br-image: {what} (blocked on: {blocker})"))]
    Unsupported { what: String, blocker: String },

    /// A JSON (de)serialization failure. Used by
    /// [`crate::image::metadata::VariantManifest::to_json_bytes`] and by any
    /// other structured-metadata emitter added later.
    #[snafu(display("json serialization error: {source}"))]
    Serialization { source: serde_json::Error },
}
