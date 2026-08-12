//! Schema for `Infra.toml`, the configuration file consumed by every
//! `twoliter publish` subcommand that talks to AWS, VMware, or a TUF
//! repo.
//!
//! Ported (trimmed) from
//! `old-twoliter/tools/pubsys-config/src/lib.rs::InfraConfig`.
//!
//! ## MVP trims
//!
//! - No `Infra.lock` YAML loader (`from_lock_path` / `from_path_or_lock`).
//! - The `[aws]` and `[vendor]` sections are fully typed. The `[repo]`,
//!   `[vmware]`, and `[ssm]` sections are preserved as opaque
//!   `toml::Value`s so an existing `Infra.toml` with those sections still
//!   parses cleanly, but no code path in the MVP consumes them. Each
//!   becomes typed as its own subcommand port lands.
//! - No `RepoExpirationPolicy`, `SigningKeyConfig`, `KMSKeyConfig`,
//!   `S3Config`, `RepoConfig` types — deferred to the `publish repo`
//!   port.

use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level `Infra.toml` configuration.
///
/// Only `aws` and `vendor` are typed. Other sections are preserved
/// as opaque `toml::Value`s so an operator's existing `Infra.toml`
/// keeps working when parsed by the MVP.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct InfraConfig {
    /// AWS configuration consumed by `publish ami` / `publish ssm`.
    pub aws: Option<AwsConfig>,

    /// Container-registry vendors.
    pub vendor: Option<HashMap<String, Vendor>>,

    /// Preserved opaquely; consumed once `publish repo` lands.
    pub repo: Option<toml::Value>,

    /// Preserved opaquely; consumed once `publish ova` lands.
    pub vmware: Option<toml::Value>,

    /// Preserved opaquely; consumed once `publish ssm` templates land.
    pub ssm: Option<toml::Value>,
}

impl InfraConfig {
    /// Load an `Infra.toml` from disk.
    pub fn from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let text = fs::read_to_string(path).context(error::FileSnafu { path })?;
        toml::from_str(&text).context(error::InvalidTomlSnafu { path })
    }

    /// Load `Infra.toml` if it exists, otherwise return `InfraConfig::default()`.
    pub fn from_path_or_default<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        if path.as_ref().exists() {
            Self::from_path(path)
        } else {
            Ok(Self::default())
        }
    }

    /// Return the parent directory of `infra_config_path`.
    ///
    /// Kept as a helper so callers that need to resolve related paths
    /// (e.g. a launch-permissions file relative to `Infra.toml`) can
    /// do so consistently.
    pub fn parent_dir<P>(infra_config_path: P) -> Result<PathBuf>
    where
        P: AsRef<Path>,
    {
        Ok(infra_config_path
            .as_ref()
            .parent()
            .context(error::ParentSnafu {
                path: infra_config_path.as_ref(),
            })?
            .to_path_buf())
    }
}

/// Container registry vendor.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
pub struct Vendor {
    pub registry: String,
}

/// AWS-specific infrastructure configuration.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    /// Default set of regions to publish to. Ordered: the first is
    /// the base region that AMIs are registered in, then copied from.
    #[serde(default)]
    pub regions: VecDeque<String>,

    /// Optional cross-account role to assume for all regions.
    pub role: Option<String>,

    /// Optional AWS profile name.
    pub profile: Option<String>,

    /// Per-region overrides (currently just the assume-role ARN).
    #[serde(default)]
    pub region: HashMap<String, AwsRegionConfig>,

    /// Prefix for SSM parameter templates. Consumed by `publish ssm`
    /// (deferred).
    pub ssm_prefix: Option<String>,
}

/// Per-region overrides for AWS credentials.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub struct AwsRegionConfig {
    pub role: Option<String>,
}

pub mod error {
    use snafu::Snafu;
    use std::io;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub enum Error {
        #[snafu(display("Failed to read '{}': {}", path.display(), source))]
        File { path: PathBuf, source: io::Error },

        #[snafu(display("Invalid config file at '{}': {}", path.display(), source))]
        InvalidToml {
            path: PathBuf,
            source: toml::de::Error,
        },

        #[snafu(display("Failed to get parent of path: {}", path.display()))]
        Parent { path: PathBuf },
    }
}
pub use error::Error;
pub type Result<T> = std::result::Result<T, error::Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Infra.toml` that only sets `[aws]` should parse and yield
    /// the expected typed values.
    #[test]
    fn parses_aws_only_config() {
        let toml_text = r#"
[aws]
regions = ["us-west-2", "us-east-1"]
profile = "bottlerocket"
role = "arn:aws:iam::123456789012:role/BottlerocketPublish"

[aws.region.us-east-1]
role = "arn:aws:iam::999999999999:role/RegionalRole"
"#;
        let cfg: InfraConfig = toml::from_str(toml_text).unwrap();
        let aws = cfg.aws.expect("aws section present");
        assert_eq!(
            aws.regions.iter().cloned().collect::<Vec<_>>(),
            vec!["us-west-2".to_string(), "us-east-1".to_string()]
        );
        assert_eq!(aws.profile.as_deref(), Some("bottlerocket"));
        assert_eq!(
            aws.role.as_deref(),
            Some("arn:aws:iam::123456789012:role/BottlerocketPublish")
        );
        assert_eq!(
            aws.region.get("us-east-1").and_then(|r| r.role.as_deref()),
            Some("arn:aws:iam::999999999999:role/RegionalRole")
        );
    }

    /// Acceptance-criterion driver: an `Infra.toml` carrying the full
    /// old-pubsys shape (with `[repo]`, `[vmware]`, `[ssm]` sections)
    /// must parse without error. Only the `[aws]` section is
    /// consumed; the rest is preserved opaquely.
    #[test]
    fn parses_full_old_pubsys_shape_opaquely() {
        let toml_text = r#"
[aws]
regions = ["us-west-2"]

[repo.my-project]
metadata_base_url = "https://updates.example.com/{variant}"
targets_url = "https://updates.example.com/{variant}/targets"
root_role_path = "Infra/root.json"

[repo.my-project.signing_keys.root]
type = "local"
path = "Infra/keys/root.pem"

[vmware.my-dc]
url = "https://vcenter.example.com/sdk"
datacenter = "DC1"

[ssm.templates]
"/aws/service/bottlerocket/{{variant}}/{{arch}}/latest/image_id" = "{{image-id}}"

[vendor.my-vendor]
registry = "public.ecr.aws/example"
"#;
        let cfg: InfraConfig = toml::from_str(toml_text).unwrap();
        assert!(cfg.aws.is_some());
        assert!(cfg.repo.is_some(), "repo section preserved opaquely");
        assert!(cfg.vmware.is_some(), "vmware section preserved opaquely");
        assert!(cfg.ssm.is_some(), "ssm section preserved opaquely");
        let vendor = cfg.vendor.expect("vendor typed");
        assert_eq!(
            vendor.get("my-vendor").map(|v| v.registry.as_str()),
            Some("public.ecr.aws/example")
        );
    }

    /// The empty document is the trivial InfraConfig::default().
    #[test]
    fn parses_empty_config() {
        let cfg: InfraConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, InfraConfig::default());
    }

    /// Unknown fields inside the typed `[aws]` block are still
    /// rejected (deny_unknown_fields), which catches typos in region
    /// override tables early.
    #[test]
    fn rejects_unknown_fields_in_aws() {
        let toml_text = r#"
[aws]
regions = ["us-west-2"]
unknown_field = "oops"
"#;
        let err = toml::from_str::<InfraConfig>(toml_text).unwrap_err();
        assert!(
            err.to_string().contains("unknown_field"),
            "expected unknown-field error, got: {err}"
        );
    }
}
