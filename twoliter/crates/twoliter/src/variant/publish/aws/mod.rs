//! AWS-facing helpers shared by the register-and-copy pipeline and
//! the grant/revoke pipeline. Ported (adapted for `tracing` and the
//! trimmed `pubsys_config::InfraConfig`) from
//! `old-twoliter/tools/pubsys/src/aws/`.
pub mod client;
pub mod launch_permissions;
pub mod public;
pub mod publish_ami;
pub mod snapshot;
pub mod wait;

use aws_sdk_ec2::config::Region;

/// Wrap a region name in the SDK's `Region` newtype.
///
/// Not `impl From<&str>` because the SDK's `Region` already has that;
/// this is a named-alias helper for call-site clarity, ported from
/// `old-twoliter/tools/pubsys/src/aws/mod.rs`.
pub(crate) fn region_from_string(name: &str) -> Region {
    Region::new(name.to_owned())
}
