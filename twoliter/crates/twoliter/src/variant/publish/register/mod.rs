//! Register-and-copy pipeline: locate the local `variant-image`
//! artifact for a `(variant, version, arch)`, extract `os.img` (and
//! optionally `data.img`) from its tar layer into temp files, create
//! EBS snapshots via `coldsnap`, `RegisterImage` in the base region,
//! then copy the AMI to every other region (granting cross-account
//! access first).
//!
//! Ported from `old-twoliter/tools/pubsys/src/aws/ami/`.

mod merge_toml;
mod mk_amispec;
mod orchestrator;
mod register_image;

pub use orchestrator::{Register, run};
pub use orchestrator::error::Error;
