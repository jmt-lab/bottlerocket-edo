//! `Twoliter.override.toml` overlay and `--vendor` CLI override.
//!
//! Overrides let a build swap a vendor registry, pin a kit or the SDK to
//! a specific digest, or point at a local OCI archive without editing
//! `Twoliter.toml`. See `doc/config/overrides.md` for the user-facing
//! spec.
//!
//! Two shapes feed into the same in-memory [`TwoliterOverride`]:
//!
//! * A `Twoliter.override.toml` file (or any file passed via
//!   `--override <path>`, repeatable).
//! * Inline `--vendor <name>=<registry>` CLI pairs, synthesized via
//!   [`TwoliterOverride::from_cli`].
//!
//! [`TwoliterOverride::apply`] then mutates the base schema:
//!
//! * `[vendor.<name>]` overrides overwrite the `uri` config on the two
//!   vendor elements twoliter emits at `//vendor/<name>` and
//!   `//vendor/<name>-sdk`.
//! * `[[kit-override]]` and `[sdk-override]` remove the corresponding
//!   `[requires.*]` entry and inject a pre-resolved source at the same
//!   address, so `Project::build` skips the resolver for that address.

use std::collections::BTreeMap;
use std::path::PathBuf;

use edo::context::{Addr, ContextResult, Element, Schema};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use snafu::ensure;

use super::twoliter::{Twoliter, VendorSpec};

/// Top-level shape of a `Twoliter.override.toml` file.
///
/// Every field defaults to empty so an override file can carry any
/// subset of the three override kinds. Unknown fields are rejected so
/// typos surface at load time.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TwoliterOverride {
    /// Vendor-registry redirects, keyed by the same short name used in
    /// `Twoliter.toml`'s `[vendor.<name>]`.
    #[serde(default)]
    pub vendor: BTreeMap<String, VendorSpec>,
    /// Per-kit pins. Each entry names a kit already declared in the
    /// base `Twoliter.toml` and supplies either a `digest` or a
    /// `local-path`.
    #[serde(default)]
    pub kit_override: Vec<KitOverride>,
    /// SDK pin. Must match the base `[sdk]` `name` and `vendor`.
    #[serde(default)]
    pub sdk_override: Option<KitOverride>,
}

/// A single `[[kit-override]]` or `[sdk-override]` entry.
///
/// Exactly one of `digest` and `local_path` must be set. Version is a
/// concrete `Version` (not a `VersionReq`) because an override pins to
/// a single, exact release.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct KitOverride {
    pub name: String,
    pub vendor: String,
    pub version: Version,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub local_path: Option<PathBuf>,
}

impl TwoliterOverride {
    /// Synthesize an override from `--vendor <name>=<registry>` CLI
    /// pairs. Later pairs win when a name is repeated, matching
    /// clap's collect-then-fold semantics for repeated flags.
    pub fn from_cli(pairs: &[(String, String)]) -> Self {
        let mut vendor = BTreeMap::new();
        for (name, registry) in pairs {
            vendor.insert(
                name.clone(),
                VendorSpec {
                    registry: registry.clone(),
                },
            );
        }
        Self {
            vendor,
            kit_override: Vec::new(),
            sdk_override: None,
        }
    }

    /// Fold another override into this one. Semantics:
    ///
    /// * Vendors: last-writer-wins per name.
    /// * Kit overrides: last-writer-wins per `(vendor, name)`; a newer
    ///   entry for the same kit replaces the older one.
    /// * SDK override: last non-`None` value wins.
    pub fn merge(&mut self, other: TwoliterOverride) {
        for (name, spec) in other.vendor {
            self.vendor.insert(name, spec);
        }
        for ko in other.kit_override {
            if let Some(pos) = self
                .kit_override
                .iter()
                .position(|existing| existing.vendor == ko.vendor && existing.name == ko.name)
            {
                self.kit_override[pos] = ko;
            } else {
                self.kit_override.push(ko);
            }
        }
        if other.sdk_override.is_some() {
            self.sdk_override = other.sdk_override;
        }
    }

    /// Returns true when this override carries no effective content.
    /// Callers can skip the apply step in that case.
    pub fn is_empty(&self) -> bool {
        self.vendor.is_empty() && self.kit_override.is_empty() && self.sdk_override.is_none()
    }

    /// Apply this override to `schema`, using `base` (the parsed
    /// `Twoliter.toml`) to validate that every override references a
    /// known vendor/kit and to look up the vendor registry when
    /// composing digest-pinned image URIs.
    pub fn apply(&self, base: &Twoliter, schema: &mut Schema) -> ContextResult<()> {
        // 1. Vendor-registry redirects: rewrite the `uri` config on the
        //    two vendor elements twoliter emitted at `//vendor/<name>`
        //    (kit-image) and `//vendor/<name>-sdk` (image). The rest of
        //    the element config is left untouched, so any downstream
        //    plugin options survive the swap.
        for (name, spec) in self.vendor.iter() {
            if !base.vendor.contains_key(name) {
                return Err(error::Error::UnknownVendor { name: name.clone() }.into());
            }
            for suffix in ["", "-sdk"] {
                let addr = Addr::parse(&format!("//vendor/{name}{suffix}"))?;
                if let Some(elem) = schema.vendor_mut(&addr) {
                    elem.config
                        .insert("uri".to_string(), json!(&spec.registry));
                }
            }
        }

        // 2. Kit overrides: verify the kit exists in the base, then
        //    remove the require and inject a pre-resolved source at the
        //    same address. `Project::build` will skip resolution for
        //    that addr (the require is gone) and `resolve_sources`
        //    will find our source when environments/transforms consume
        //    the addr.
        for ko in self.kit_override.iter() {
            validate_source_shape(ko, /* is_sdk */ false)?;
            let kit_in_base = base
                .kit
                .iter()
                .any(|k| k.name == ko.name && k.vendor == ko.vendor);
            ensure!(
                kit_in_base,
                error::OverrideKitNotFoundSnafu {
                    name: ko.name.clone(),
                    vendor: ko.vendor.clone(),
                }
            );
            let vendor_spec = base.vendor.get(&ko.vendor).ok_or_else(|| {
                error::Error::UnknownVendor {
                    name: ko.vendor.clone(),
                }
            })?;
            // Registry the base would use, unless a `[vendor.<name>]`
            // override in this same file already redirected it.
            let effective_registry = self
                .vendor
                .get(&ko.vendor)
                .map(|s| s.registry.as_str())
                .unwrap_or(vendor_spec.registry.as_str());
            let addr = Addr::parse(&format!("//kits/{}/{}", ko.vendor, ko.name))?;
            schema.remove_require(&addr);
            let element = build_override_source(&addr, "kit-image", ko, effective_registry)?;
            schema.add_source(&addr, &element);
        }

        // 3. SDK override: same shape as kit override, but the address
        //    is `//<sdk.name>` and the source kind is plain `image`
        //    (not `kit-image` — the SDK isn't a kit and doesn't carry
        //    the kit-metadata label).
        if let Some(sdk_ov) = self.sdk_override.as_ref() {
            validate_source_shape(sdk_ov, /* is_sdk */ true)?;
            ensure!(
                sdk_ov.name == base.sdk.name && sdk_ov.vendor == base.sdk.vendor,
                error::OverrideSdkMismatchSnafu {
                    got_name: sdk_ov.name.clone(),
                    got_vendor: sdk_ov.vendor.clone(),
                    want_name: base.sdk.name.clone(),
                    want_vendor: base.sdk.vendor.clone(),
                }
            );
            let vendor_spec = base.vendor.get(&sdk_ov.vendor).ok_or_else(|| {
                error::Error::UnknownVendor {
                    name: sdk_ov.vendor.clone(),
                }
            })?;
            let effective_registry = self
                .vendor
                .get(&sdk_ov.vendor)
                .map(|s| s.registry.as_str())
                .unwrap_or(vendor_spec.registry.as_str());
            let addr = Addr::parse(&format!("//{}", sdk_ov.name))?;
            schema.remove_require(&addr);
            let element = build_override_source(&addr, "image", sdk_ov, effective_registry)?;
            schema.add_source(&addr, &element);
        }

        Ok(())
    }
}

/// Ensure exactly one of `digest` or `local_path` is set.
fn validate_source_shape(ko: &KitOverride, is_sdk: bool) -> Result<(), error::Error> {
    let target = if is_sdk {
        format!("sdk-override `{}`", ko.name)
    } else {
        format!("kit-override `{}/{}`", ko.vendor, ko.name)
    };
    match (ko.digest.as_ref(), ko.local_path.as_ref()) {
        (None, None) => error::OverrideMissingSourceSnafu { target }.fail(),
        (Some(_), Some(_)) => error::OverrideConflictingSourceSnafu { target }.fail(),
        _ => Ok(()),
    }
}

/// Build the pre-resolved override element for a kit or SDK.
///
/// * `digest`: emit a `kit-image` (or `image`) element with `uri` and
///   `ref` fields matching what a resolver-produced element carries.
///   The URI is composed as `<registry>/<name>:v<version>` to mirror the
///   tag convention `KitImageVendor::resolve` uses; the `ref` is the
///   supplied digest with any `sha256:` prefix stripped.
/// * `local_path`: emit a plain `local` source element (kind `local`,
///   config `{path}`). The kit transform accepts a local source at the
///   require address; a `file://` URI is not enough for the kit-image
///   source since it goes through `ocilot`.
fn build_override_source(
    addr: &Addr,
    remote_kind: &str,
    ko: &KitOverride,
    registry: &str,
) -> ContextResult<Element> {
    if let Some(digest) = ko.digest.as_ref() {
        let uri = format!("{}/{}:v{}", registry.trim_end_matches('/'), ko.name, ko.version);
        let normalized_ref = digest.strip_prefix("sha256:").unwrap_or(digest).to_string();
        Ok(Element::builder()
            .addr(addr.clone())
            .kind(remote_kind)
            .config(BTreeMap::from([
                ("uri".to_string(), json!(uri)),
                ("ref".to_string(), json!(normalized_ref)),
            ]))
            .build())
    } else if let Some(local) = ko.local_path.as_ref() {
        Ok(Element::builder()
            .addr(addr.clone())
            .kind("local")
            .config(BTreeMap::from([(
                "path".to_string(),
                json!(local.to_string_lossy()),
            )]))
            .build())
    } else {
        // Unreachable: `validate_source_shape` runs first.
        unreachable!("validate_source_shape must gate this call")
    }
}

pub mod error {
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display(
            "override references unknown vendor `{name}`; add a `[vendor.{name}]` entry to \
             `Twoliter.toml`"
        ))]
        UnknownVendor { name: String },
        #[snafu(display(
            "kit-override references kit `{name}` under vendor `{vendor}`, but the base \
             `Twoliter.toml` does not declare that kit"
        ))]
        OverrideKitNotFound { name: String, vendor: String },
        #[snafu(display(
            "{target}: must set exactly one of `digest` or `local-path`, got neither"
        ))]
        OverrideMissingSource { target: String },
        #[snafu(display(
            "{target}: must set exactly one of `digest` or `local-path`, got both"
        ))]
        OverrideConflictingSource { target: String },
        #[snafu(display(
            "sdk-override targets `{got_vendor}/{got_name}` but the base SDK is \
             `{want_vendor}/{want_name}`"
        ))]
        OverrideSdkMismatch {
            got_name: String,
            got_vendor: String,
            want_name: String,
            want_vendor: String,
        },
    }

    impl From<Error> for edo::context::ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Twoliter` is already imported via the module-level `use
    // super::twoliter::{Twoliter, VendorSpec}` above.

    fn base_twoliter() -> Twoliter {
        toml::from_str(
            r#"
release-version = "1.42.0"

[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[vendor.my-vendor]
registry = "example.com/my-vendor"

[[kit]]
name = "core-kit"
vendor = "bottlerocket"
version = "=1.42.0"

[[kit]]
name = "extra-kit"
vendor = "my-vendor"
version = "=0.1.0"
"#,
        )
        .expect("valid base twoliter")
    }

    fn base_schema() -> Schema {
        base_twoliter().to_edo().expect("base to_edo")
    }

    #[test]
    fn parses_full_override_toml() {
        let toml = r#"
[vendor.bottlerocket]
registry = "public.ecr.aws/my-mirror/bottlerocket"

[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:abcdef"

[[kit-override]]
name = "extra-kit"
vendor = "my-vendor"
version = "0.1.0"
local-path = "./mock-kits/extra-kit.oci"

[sdk-override]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "0.76.0"
digest = "sha256:cafef00d"
"#;
        let ov: TwoliterOverride = toml::from_str(toml).expect("parse override");
        assert_eq!(ov.vendor.len(), 1);
        assert_eq!(
            ov.vendor.get("bottlerocket").unwrap().registry,
            "public.ecr.aws/my-mirror/bottlerocket"
        );
        assert_eq!(ov.kit_override.len(), 2);
        assert_eq!(ov.kit_override[0].name, "core-kit");
        assert_eq!(ov.kit_override[0].digest.as_deref(), Some("sha256:abcdef"));
        assert_eq!(
            ov.kit_override[1].local_path.as_deref().unwrap(),
            std::path::Path::new("./mock-kits/extra-kit.oci"),
        );
        let sdk = ov.sdk_override.expect("sdk-override present");
        assert_eq!(sdk.name, "bottlerocket-sdk");
        assert_eq!(sdk.digest.as_deref(), Some("sha256:cafef00d"));
    }

    #[test]
    fn rejects_unknown_field() {
        let toml = r#"
[vendor.bottlerocket]
registry = "x"
extra-junk = 42
"#;
        let res: Result<TwoliterOverride, _> = toml::from_str(toml);
        assert!(res.is_err(), "unknown field should be rejected");
    }

    #[test]
    fn apply_vendor_redirect_rewrites_registry() {
        let base = base_twoliter();
        let mut schema = base.to_edo().unwrap();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[vendor.bottlerocket]
registry = "example.com/mirror"
"#,
        )
        .unwrap();
        ov.apply(&base, &mut schema).expect("apply ok");
        let kit_vendor = schema
            .vendors()
            .get(&Addr::parse("//vendor/bottlerocket").unwrap())
            .expect("kit vendor present");
        assert_eq!(
            kit_vendor.config.get("uri").and_then(|v| v.as_str()),
            Some("example.com/mirror")
        );
        let sdk_vendor = schema
            .vendors()
            .get(&Addr::parse("//vendor/bottlerocket-sdk").unwrap())
            .expect("sdk vendor present");
        assert_eq!(
            sdk_vendor.config.get("uri").and_then(|v| v.as_str()),
            Some("example.com/mirror")
        );
        // The other vendor is unaffected.
        let my_vendor = schema
            .vendors()
            .get(&Addr::parse("//vendor/my-vendor").unwrap())
            .expect("other vendor present");
        assert_eq!(
            my_vendor.config.get("uri").and_then(|v| v.as_str()),
            Some("example.com/my-vendor")
        );
    }

    #[test]
    fn apply_kit_digest_override_replaces_require_with_source() {
        let base = base_twoliter();
        let mut schema = base.to_edo().unwrap();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:abcdef"
"#,
        )
        .unwrap();
        ov.apply(&base, &mut schema).expect("apply ok");
        let addr = Addr::parse("//kits/bottlerocket/core-kit").unwrap();
        assert!(
            !schema.requires().contains_key(&addr),
            "require should be removed"
        );
        let source = schema
            .sources()
            .get(&addr)
            .expect("override source injected");
        assert_eq!(source.kind, "kit-image");
        assert_eq!(
            source.config.get("uri").and_then(|v| v.as_str()),
            Some("public.ecr.aws/bottlerocket/core-kit:v1.42.0")
        );
        assert_eq!(
            source.config.get("ref").and_then(|v| v.as_str()),
            Some("abcdef")
        );
        // The other kit's require is untouched.
        assert!(
            schema
                .requires()
                .contains_key(&Addr::parse("//kits/my-vendor/extra-kit").unwrap()),
            "unrelated kit require should survive"
        );
    }

    #[test]
    fn apply_kit_local_path_override_emits_local_source() {
        let base = base_twoliter();
        let mut schema = base.to_edo().unwrap();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
local-path = "/tmp/kits/core-kit.oci"
"#,
        )
        .unwrap();
        ov.apply(&base, &mut schema).expect("apply ok");
        let addr = Addr::parse("//kits/bottlerocket/core-kit").unwrap();
        assert!(!schema.requires().contains_key(&addr));
        let source = schema.sources().get(&addr).expect("local source injected");
        assert_eq!(source.kind, "local");
        assert_eq!(
            source.config.get("path").and_then(|v| v.as_str()),
            Some("/tmp/kits/core-kit.oci")
        );
    }

    #[test]
    fn apply_sdk_digest_override_replaces_require_with_source() {
        let base = base_twoliter();
        let mut schema = base.to_edo().unwrap();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[sdk-override]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "0.76.0"
digest = "sha256:cafe"
"#,
        )
        .unwrap();
        ov.apply(&base, &mut schema).expect("apply ok");
        let addr = Addr::parse("//bottlerocket-sdk").unwrap();
        assert!(!schema.requires().contains_key(&addr));
        let source = schema
            .sources()
            .get(&addr)
            .expect("sdk override source injected");
        assert_eq!(source.kind, "image");
        assert_eq!(
            source.config.get("uri").and_then(|v| v.as_str()),
            Some("public.ecr.aws/bottlerocket/bottlerocket-sdk:v0.76.0")
        );
        assert_eq!(
            source.config.get("ref").and_then(|v| v.as_str()),
            Some("cafe")
        );
    }

    #[test]
    fn cli_vendor_pair_synthesizes_override() {
        let ov = TwoliterOverride::from_cli(&[(
            "bottlerocket".to_string(),
            "example.com/mirror".to_string(),
        )]);
        assert_eq!(
            ov.vendor.get("bottlerocket").unwrap().registry,
            "example.com/mirror"
        );
        assert!(ov.kit_override.is_empty());
        assert!(ov.sdk_override.is_none());
    }

    #[test]
    fn merge_last_writer_wins_for_repeated_vendor_and_kit() {
        let mut ov: TwoliterOverride = toml::from_str(
            r#"
[vendor.bottlerocket]
registry = "first"

[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:first"
"#,
        )
        .unwrap();
        let later: TwoliterOverride = toml::from_str(
            r#"
[vendor.bottlerocket]
registry = "second"

[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:second"
"#,
        )
        .unwrap();
        ov.merge(later);
        assert_eq!(ov.vendor.get("bottlerocket").unwrap().registry, "second");
        assert_eq!(ov.kit_override.len(), 1);
        assert_eq!(
            ov.kit_override[0].digest.as_deref(),
            Some("sha256:second")
        );
    }

    #[test]
    fn errors_on_kit_override_for_unknown_kit() {
        let base = base_twoliter();
        let mut schema = base_schema();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[[kit-override]]
name = "ghost-kit"
vendor = "bottlerocket"
version = "1.0.0"
digest = "sha256:aa"
"#,
        )
        .unwrap();
        let err = ov.apply(&base, &mut schema).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("ghost-kit"), "err should name the kit: {msg}");
    }

    #[test]
    fn errors_on_missing_source_fields() {
        let base = base_twoliter();
        let mut schema = base_schema();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
"#,
        )
        .unwrap();
        let err = ov.apply(&base, &mut schema).expect_err("should fail");
        assert!(err.to_string().contains("neither"), "{err}");
    }

    #[test]
    fn errors_on_both_digest_and_local_path() {
        let base = base_twoliter();
        let mut schema = base_schema();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:aa"
local-path = "/tmp/kit.oci"
"#,
        )
        .unwrap();
        let err = ov.apply(&base, &mut schema).expect_err("should fail");
        assert!(err.to_string().contains("both"), "{err}");
    }

    #[test]
    fn errors_on_unknown_vendor_in_vendor_override() {
        let base = base_twoliter();
        let mut schema = base_schema();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[vendor.ghost]
registry = "example.com/ghost"
"#,
        )
        .unwrap();
        let err = ov.apply(&base, &mut schema).expect_err("should fail");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn errors_on_sdk_override_mismatch() {
        let base = base_twoliter();
        let mut schema = base_schema();
        let ov: TwoliterOverride = toml::from_str(
            r#"
[sdk-override]
name = "different-sdk"
vendor = "bottlerocket"
version = "0.76.0"
digest = "sha256:aa"
"#,
        )
        .unwrap();
        let err = ov.apply(&base, &mut schema).expect_err("should fail");
        assert!(err.to_string().contains("different-sdk"), "{err}");
    }
}
