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
    /// Optional `[lookaside]` table. When present, emits an S3-backed
    /// source cache named `"lookaside"` into the schema, so that
    /// `[[package.external-files]]` (`kind = "remote"`) fetches
    /// short-circuit to the S3 bucket before hitting the upstream URL.
    #[serde(default)]
    pub lookaside: Option<LookasideSpec>,
}

/// `[lookaside]` table.
///
/// Fields correspond 1:1 to `edo-core`'s `S3BackendOptions`:
/// `bucket` is required and `prefix` is optional. Region and credentials
/// come from the ambient AWS config (`aws_config::load_defaults`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LookasideSpec {
    /// S3 bucket that holds the shared source-cache blobs.
    pub bucket: String,
    /// Optional key prefix inside `bucket`.
    #[serde(default)]
    pub prefix: Option<String>,
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

        // Guard against name collisions between a kit vendor `<foo>` and
        // another kit vendor `<foo>-sdk`: the SDK-vendor address we emit
        // for `<foo>` (`//vendor/<foo>-sdk`) would clash with the
        // kit-vendor address for `<foo>-sdk` (`//vendor/<foo>-sdk`).
        for name in self.vendor.keys() {
            if let Some(stripped) = name.strip_suffix("-sdk") {
                if self.vendor.contains_key(stripped) {
                    return Err(error::Error::VendorNameCollision {
                        sdk_vendor: name.clone(),
                        kit_vendor: stripped.to_string(),
                    }
                    .into());
                }
            }
        }

        let mut vendors = BTreeMap::new();
        let mut requires = BTreeMap::new();
        let mut environments = BTreeMap::new();

        // 1. Per `[vendor.<name>]` entry, emit two vendor elements over
        //    the same registry URI:
        //    - `//vendor/<name>`     kind=kit-image (for kit requirements)
        //    - `//vendor/<name>-sdk` kind=image     (for the SDK requirement)
        //    The SDK image does not carry the kit-metadata label
        //    `KitImageSource::fetch` requires, so the SDK is resolved
        //    and fetched through edo-core's built-in `image` vendor and
        //    `image` source instead. Kits continue to use `kit-image`.
        for (name, spec) in self.vendor.iter() {
            let kit_addr = Addr::parse(&format!("//vendor/{name}"))?;
            vendors.insert(
                kit_addr.clone(),
                Element::builder()
                    .addr(kit_addr)
                    .kind("kit-image")
                    .config(BTreeMap::from([
                        ("uri".to_string(), json!(&spec.registry)),
                        ("name".to_string(), json!(name)),
                    ]))
                    .build(),
            );
            let sdk_vendor_addr = Addr::parse(&format!("//vendor/{name}-sdk"))?;
            vendors.insert(
                sdk_vendor_addr.clone(),
                Element::builder()
                    .addr(sdk_vendor_addr)
                    .kind("image")
                    .config(BTreeMap::from([(
                        "uri".to_string(),
                        json!(&spec.registry),
                    )]))
                    .build(),
            );
        }

        // 2. SDK requirement at `//<sdk.name>`, resolved through the
        //    `<sdk.vendor>-sdk` vendor (kind `image`).
        //
        //    `Requirement.vendor` must be the *address* the vendor is
        //    registered under, not the twoliter short name: edo's
        //    `Resolver::add_vendor` keys vendors by `element.addr()`
        //    (see `crates/edo/src/context/builder.rs`, "register vendor"),
        //    and its candidate filter is a strict string equality against
        //    `Requirement.vendor`. Emitting `"bottlerocket-sdk"` here
        //    would never match the registered key
        //    `"//vendor/bottlerocket-sdk"`, and the resolver would find
        //    no candidates and hang / fail.
        let sdk_vendor_addr = format!("//vendor/{}-sdk", self.sdk.vendor);
        let sdk_addr = Addr::parse(&format!("//{}", self.sdk.name))?;
        requires.insert(
            sdk_addr.clone(),
            Requirement {
                kind: "image".to_string(),
                at: self.sdk.version.clone(),
                vendor: Some(sdk_vendor_addr),
            },
        );

        // 3. Kit requirements at `//kits/<vendor>/<name>`, one per `[[kit]]`.
        //    Same "vendor is an address" rule as (2).
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
                    vendor: Some(format!("//vendor/{}", kit.vendor)),
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

        // 6. Optional S3-backed source cache for `[[package.external-files]]`
        //    (which are emitted as `kind = "remote"` sources by
        //    `Package::to_edo`). This is the "lookaside" concept from old
        //    buildsys, expressed as edo's native source-cache mechanism:
        //    on cache hit the blob comes from S3, on cache miss edo falls
        //    through to `RemoteSource::fetch` against the upstream URL.
        //    Any `[cache.source.lookaside]` in an `edo.toml` overlay wins
        //    because `Schema::add_source_cache` is or-insert.
        if let Some(lookaside) = self.lookaside.as_ref() {
            let lookaside_cache_addr = Addr::parse("//edo-source-cache/lookaside")?;
            let mut config = BTreeMap::from([
                ("bucket".to_string(), json!(&lookaside.bucket)),
            ]);
            if let Some(prefix) = lookaside.prefix.as_ref() {
                config.insert("prefix".to_string(), json!(prefix));
            }
            schema.add_source_cache(
                "lookaside",
                Element::builder()
                    .addr(lookaside_cache_addr)
                    .kind("s3")
                    .config(config)
                    .build(),
            );
        }

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
        // Vendors emitted at `//vendor/<name>` (kit-image) and
        // `//vendor/<name>-sdk` (image), one pair per twoliter vendor.
        let vendors: Vec<_> = schema.vendors().keys().map(|a| a.to_string()).collect();
        assert!(vendors.contains(&"//vendor/bottlerocket".to_string()));
        assert!(vendors.contains(&"//vendor/bottlerocket-sdk".to_string()));
        assert!(vendors.contains(&"//vendor/my-vendor".to_string()));
        assert!(vendors.contains(&"//vendor/my-vendor-sdk".to_string()));
        // The `-sdk` vendor element uses kind `image` and points at the
        // same registry URI as the twoliter vendor.
        let sdk_vendor_elem = schema
            .vendors()
            .get(&Addr::parse("//vendor/bottlerocket-sdk").unwrap())
            .expect("bottlerocket-sdk vendor emitted");
        assert_eq!(sdk_vendor_elem.kind, "image");
        assert_eq!(
            sdk_vendor_elem
                .config
                .get("uri")
                .and_then(|v| v.as_str()),
            Some("public.ecr.aws/bottlerocket")
        );
        // The kit-image vendor element carries the original kind.
        let kit_vendor_elem = schema
            .vendors()
            .get(&Addr::parse("//vendor/bottlerocket").unwrap())
            .expect("bottlerocket kit vendor emitted");
        assert_eq!(kit_vendor_elem.kind, "kit-image");
        // Cache slot present.
        assert!(schema.get_source_caches().contains_key("kit-image"));
        // SDK requirement resolves through the `-sdk` vendor at kind `image`.
        let sdk_req = schema
            .requires()
            .get(&Addr::parse("//bottlerocket-sdk").unwrap())
            .unwrap();
        // `Requirement.vendor` must carry the vendor element's *address*
        // (the key edo's resolver registers vendors under), not the
        // twoliter short name.
        assert_eq!(sdk_req.vendor.as_deref(), Some("//vendor/bottlerocket-sdk"));
        assert_eq!(sdk_req.kind, "image");
        // Kit requirements still route through the `kit-image` vendor;
        // pinned by address so the resolver's vendor filter matches.
        let kit_req = schema
            .requires()
            .get(&Addr::parse("//kits/bottlerocket/core-kit").unwrap())
            .unwrap();
        assert_eq!(kit_req.vendor.as_deref(), Some("//vendor/bottlerocket"));
        assert_eq!(kit_req.kind, "kit-image");
    }

    #[test]
    fn single_kit_project_emits_no_vendor_collision() {
        // A project with a single vendor and a single kit must still
        // produce a well-formed schema: one kit-image vendor and one
        // image vendor at distinct addresses, with no collision.
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[[kit]]
name = "core-kit"
vendor = "bottlerocket"
version = "=1.42.0"
"#,
        );
        let schema = cfg.to_edo().expect("to_edo");
        let vendors: Vec<_> = schema.vendors().keys().map(|a| a.to_string()).collect();
        assert_eq!(vendors.len(), 2, "one kit-image + one image vendor");
        assert!(vendors.contains(&"//vendor/bottlerocket".to_string()));
        assert!(vendors.contains(&"//vendor/bottlerocket-sdk".to_string()));
    }

    #[test]
    fn rejects_vendor_name_collision_with_sdk_suffix() {
        // If a project declares both `foo` and `foo-sdk` as vendors,
        // the SDK-vendor address twoliter emits for `foo`
        // (`//vendor/foo-sdk`) would collide with the kit-vendor
        // address for `foo-sdk`. Surface a clear error.
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "foo"
version = "=0.76.0"

[vendor.foo]
registry = "example.com/foo"

[vendor.foo-sdk]
registry = "example.com/foo-sdk"
"#,
        );
        let err = cfg.to_edo().expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("foo-sdk"), "error should name the collision: {msg}");
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
    fn lookaside_table_emits_s3_source_cache() {
        // A `[lookaside]` table with just a bucket produces a schema
        // whose `"lookaside"` source-cache element has `kind = "s3"`
        // and forwards the bucket into the element config.
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[lookaside]
bucket = "b"
"#,
        );
        let schema = cfg.to_edo().expect("to_edo");
        let cache = schema
            .get_source_caches()
            .get("lookaside")
            .expect("lookaside cache emitted");
        assert_eq!(cache.kind, "s3");
        assert_eq!(
            cache.config.get("bucket").and_then(|v| v.as_str()),
            Some("b")
        );
        assert!(
            !cache.config.contains_key("prefix"),
            "prefix should be absent when not configured"
        );
    }

    #[test]
    fn lookaside_absent_emits_no_source_cache_override() {
        // Without a `[lookaside]` table, twoliter must not seed a
        // `"lookaside"` cache entry, so an `edo.toml` overlay can
        // define one without collision.
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"
"#,
        );
        let schema = cfg.to_edo().expect("to_edo");
        assert!(
            !schema.get_source_caches().contains_key("lookaside"),
            "no lookaside cache should be seeded without a [lookaside] table"
        );
    }

    #[test]
    fn lookaside_with_prefix_forwards_prefix() {
        // Both `bucket` and `prefix` should reach the emitted element
        // config so `S3BackendOptions` can deserialize them.
        let cfg = parse(
            r#"
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.76.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[lookaside]
bucket = "bottlerocket-lookaside-v2"
prefix = "cache"
"#,
        );
        let schema = cfg.to_edo().expect("to_edo");
        let cache = schema
            .get_source_caches()
            .get("lookaside")
            .expect("lookaside cache emitted");
        assert_eq!(cache.kind, "s3");
        assert_eq!(
            cache.config.get("bucket").and_then(|v| v.as_str()),
            Some("bottlerocket-lookaside-v2")
        );
        assert_eq!(
            cache.config.get("prefix").and_then(|v| v.as_str()),
            Some("cache")
        );
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
        #[snafu(display(
            "vendor `{sdk_vendor}` collides with the SDK vendor address twoliter would emit for \
             `{kit_vendor}` (`//vendor/{kit_vendor}-sdk`); rename one of them"
        ))]
        VendorNameCollision {
            sdk_vendor: String,
            kit_vendor: String,
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

