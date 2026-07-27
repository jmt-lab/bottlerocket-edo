use std::collections::BTreeMap;

use edo::context::{Addr, ContextResult, Element, Requirement, Schema, SourceMap};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use serde_json::json;


/// Top-level `[project]` table of a `Twoliter.toml`.
///
/// Every project must declare its SDK (`[sdk]`) and at least one vendor
/// (`[vendor.<name>]`). Kits are optional and appear as `[[kit]]` tables.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Twoliter {
    /// The SDK the project builds against.
    pub sdk: SdkSpec,
    /// The set of OCI registries this project may pull kits from,
    /// keyed by the vendor's short name (e.g. `"bottlerocket"`).
    pub vendor: BTreeMap<String, VendorSpec>,
    /// Kits pulled into every variant.
    #[serde(default)]
    pub kit: Vec<KitDep>,
}

/// `[sdk]` table.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SdkSpec {
    /// The SDK image name (e.g. `"bottlerocket-sdk"`). This is also the
    /// address the SDK requirement is emitted at (`//<name>`).
    pub name: String,
    /// Which `[vendor.<name>]` this SDK comes from.
    pub vendor: String,
    /// Semver constraint on the SDK version.
    pub version: VersionReq,
}

/// `[vendor.<name>]` table.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VendorSpec {
    /// Base registry URI (e.g. `"public.ecr.aws/bottlerocket"`).
    pub registry: String,
}

/// One `[[kit]]` entry.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct KitDep {
    pub name: String,
    pub vendor: String,
    pub version: VersionReq,
}

impl Twoliter {
    pub fn to_edo(&self) -> ContextResult<Schema> {
        if self.vendor.is_empty() {
            return Err(error::Error::NoVendors.into());
        }
        if !self.vendor.contains_key(&self.sdk.vendor) {
            return Err(error::Error::UnknownSdkVendor {
                vendor: self.sdk.vendor.clone(),
                sdk: self.sdk.name.clone(),
            }
            .into());
        }

        let mut vendors = BTreeMap::new();
        let mut requires = BTreeMap::new();
        let mut environments = BTreeMap::new();

        // 1. One `kit-image` vendor per `[vendor.<name>]` entry, at
        //    `//vendor/<name>`.
        for (name, spec) in self.vendor.iter() {
            let addr = Addr::parse(&format!("//vendor/{name}"))?;
            vendors.insert(
                addr.clone(),
                Element::builder()
                    .addr(addr)
                    .kind("kit-image")
                    .config(BTreeMap::from([
                        ("uri".to_string(), json!(&spec.registry)),
                        ("name".to_string(), json!(name)),
                    ]))
                    .build(),
            );
        }

        // 2. SDK requirement at `//<sdk.name>`. Kind is `kit-image` (the
        //    SDK image also carries kit metadata; a dedicated `sdk-image`
        //    kind is a follow-up).
        let sdk_addr = Addr::parse(&format!("//{}", self.sdk.name))?;
        requires.insert(
            sdk_addr.clone(),
            Requirement {
                kind: "kit-image".to_string(),
                at: self.sdk.version.clone(),
                vendor: Some(self.sdk.vendor.clone()),
            },
        );

        // 3. Kit requirements at `//kits/<vendor>/<name>`, one per `[[kit]]`.
        for kit in self.kit.iter() {
            if !self.vendor.contains_key(&kit.vendor) {
                return Err(error::Error::UnknownKitVendor {
                    vendor: kit.vendor.clone(),
                    kit: kit.name.clone(),
                }
                .into());
            }
            let addr = Addr::parse(&format!("//kits/{}/{}", kit.vendor, kit.name))?;
            requires.insert(
                addr,
                Requirement {
                    kind: "kit-image".to_string(),
                    at: kit.version.clone(),
                    vendor: Some(kit.vendor.clone()),
                },
            );
        }

        // 4. Two sdk environments, both sourced from the resolved SDK
        //    image at `//<sdk.name>`.
        let sdk_isolated = Addr::parse("//sdk-isolated")?;
        environments.insert(
            sdk_isolated.clone(),
            Element::builder()
                .addr(sdk_isolated.clone())
                .kind("container")
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![sdk_addr.clone()],
                )])))
                .config(BTreeMap::from([("user".to_string(), json!("root"))]))
                .build(),
        );
        let sdk_open = Addr::parse("//sdk-open")?;
        environments.insert(
            sdk_open.clone(),
            Element::builder()
                .addr(sdk_open.clone())
                .kind("container")
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![sdk_addr.clone()],
                )])))
                .config(BTreeMap::from([
                    ("user".to_string(), json!("root")),
                    (
                        "config".to_string(),
                        json!({
                            "network": true
                        }),
                    ),
                ]))
                .build(),
        );

        // 5. A local source cache slot for pulled kit archives. Any
        //    matching `[cache.source.kit-image]` in an `edo.toml`
        //    overlay wins because `Schema::add_source_cache` is
        //    or-insert.
        let cache_addr = Addr::parse("//edo-source-cache/kit-image")?;
        let mut schema = Schema::builder()
            .vendor(vendors)
            .requires(requires)
            .environment(environments)
            .build();
        schema.add_source_cache(
            "kit-image",
            Element::builder()
                .addr(cache_addr)
                .kind("local")
                .config(BTreeMap::from([(
                    "path".to_string(),
                    json!("target/edo/source-cache/kit-image"),
                )]))
                .build(),
        );
        Ok(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Twoliter {
        toml::from_str(toml_str).expect("valid twoliter config")
    }

    #[test]
    fn parses_full_example_and_emits_expected_addresses() {
        let cfg = parse(
            r#"
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
        );
        let schema = cfg.to_edo().expect("to_edo");
        let requires: Vec<_> = schema
            .requires()
            .keys()
            .map(|a| a.to_string())
            .collect();
        assert!(requires.contains(&"//bottlerocket-sdk".to_string()));
        assert!(requires.contains(&"//kits/bottlerocket/core-kit".to_string()));
        assert!(requires.contains(&"//kits/my-vendor/extra-kit".to_string()));
        // Vendors emitted at //vendor/<name>.
        let vendors: Vec<_> = schema.vendors().keys().map(|a| a.to_string()).collect();
        assert!(vendors.contains(&"//vendor/bottlerocket".to_string()));
        assert!(vendors.contains(&"//vendor/my-vendor".to_string()));
        // Cache slot present.
        assert!(schema.get_source_caches().contains_key("kit-image"));
        // Requirements pin the vendor.
        let sdk_req = schema
            .requires()
            .get(&Addr::parse("//bottlerocket-sdk").unwrap())
            .unwrap();
        assert_eq!(sdk_req.vendor.as_deref(), Some("bottlerocket"));
        assert_eq!(sdk_req.kind, "kit-image");
    }

    #[test]
    fn rejects_kit_referencing_unknown_vendor() {
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[[kit]]
name = "extra-kit"
vendor = "ghost"
version = "=0.1.0"
"#,
        );
        let err = cfg.to_edo().expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "error should name the vendor: {msg}");
    }

    #[test]
    fn rejects_missing_vendor_table() {
        let err: Result<Twoliter, _> = toml::from_str(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"
"#,
        );
        // With no `[vendor.*]` at all, deserialization should fail
        // because `vendor` is required (no `serde(default)`).
        assert!(err.is_err());
    }

    #[test]
    fn rejects_sdk_referencing_unknown_vendor() {
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "ghost"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"
"#,
        );
        let err = cfg.to_edo().expect_err("should fail");
        assert!(err.to_string().contains("ghost"));
    }
}

pub mod error {
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display(
            "Twoliter.toml must declare at least one `[vendor.<name>]` table"
        ))]
        NoVendors,
        #[snafu(display(
            "sdk `{sdk}` references unknown vendor `{vendor}`; add a `[vendor.{vendor}]` table"
        ))]
        UnknownSdkVendor { vendor: String, sdk: String },
        #[snafu(display(
            "kit `{kit}` references unknown vendor `{vendor}`; add a `[vendor.{vendor}]` table"
        ))]
        UnknownKitVendor { vendor: String, kit: String },
    }

    impl From<Error> for edo::context::ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}

