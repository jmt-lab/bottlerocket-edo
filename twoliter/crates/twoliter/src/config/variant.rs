//! `variant.toml` schema: one per directory under `variants/`.
//!
//! Emits four elements per variant, sharing an address prefix
//! `//variants/<name>/`:
//!
//! | Address | Kind |
//! |---|---|
//! | `//variants/<name>/rootfs` | `variant-rootfs` |
//! | `//variants/<name>/disk` | `variant-disk` |
//! | `//variants/<name>/grub` | `variant-grub` |
//! | `//variants/<name>` | `variant-image` (finalizer) |
//!
//! The top-level address (`//variants/<name>`) stays the same as
//! today's monolithic transform, so downstream tooling addressing the
//! variant sees no change.
//!
//! **Shared TOML surface.** The `[variant]` block is deserialized
//! once and emitted verbatim on every stage's element. Fields that
//! only affect one stage (e.g. `kernel-parameters` — grub-only) are
//! still carried on the other stages' elements; they're ignored by
//! those stages' `FromElement`. This keeps the emitter uniform (one
//! codepath per stage instead of four different per-field allowlists)
//! at the cost of slightly larger config blobs.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use edo::context::{Addr, ContextResult, Element, Schema, SourceMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The parsed `variant.toml` config block, extracted from the outer
/// [`Config::V1(ConfigType::Variant(...))`] discriminator.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Variant {
    /// Variant name (e.g. `"aws-k8s-1.29"`). Must match the directory
    /// name under `variants/` so a lookup by directory path is
    /// unambiguous.
    pub name: String,
    /// Variant version. Defaults to a `0.0.0` sentinel if the caller
    /// leaves it out — real projects always set this.
    #[serde(default = "default_version")]
    pub version: semver::Version,
    /// Explicit arch override. Absent = host arch (or CLI
    /// `--arch <arch>` on `twoliter build`).
    #[serde(default)]
    pub arch: Option<String>,

    /// Kits this variant includes. Each entry references a
    /// `//kits/<vendor>/<name>` element declared in `Twoliter.toml`.
    #[serde(default)]
    pub kit: Vec<VariantKitRef>,

    /// Kernel parameters passed to the built image. Order-sensitive:
    /// grub concatenates the strings verbatim.
    #[serde(default)]
    pub kernel_parameters: Vec<String>,

    /// The `[variant.image]` sub-table.
    #[serde(default)]
    pub image: VariantImageBlock,

    /// The `[[variant.guest-image]]` array.
    #[serde(default)]
    pub guest_image: Vec<VariantGuestImageRef>,

    /// The `[variant.included-packages]` sub-table.
    #[serde(default)]
    pub included_packages: IncludedPackagesBlock,
}

fn default_version() -> semver::Version {
    semver::Version::new(0, 0, 0)
}

/// One `[[variant.kit]]` entry.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VariantKitRef {
    /// The kit's transform address (e.g.
    /// `//kits/bottlerocket/core-kit`).
    pub addr: Addr,
}

/// One `[[variant.guest-image]]` entry.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VariantGuestImageRef {
    /// The guest image transform address.
    pub addr: Addr,
    /// The path in the rootfs where this guest image's artifacts
    /// should land. Currently informational — the transform copies
    /// into a fixed `/local/guest-images/<arch-name>/<version>/`
    /// tree regardless.
    #[serde(default)]
    pub install_path: Option<PathBuf>,
}

/// The `[variant.included-packages]` sub-table.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct IncludedPackagesBlock {
    #[serde(default)]
    pub packages: Vec<String>,
}

/// The `[variant.image]` sub-table plus its nested `features`.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VariantImageBlock {
    /// Image format on disk. Serialized as one of `raw`, `vmdk`.
    #[serde(default)]
    pub format: Option<String>,
    /// Partition-plan; `split` or `unified`.
    #[serde(default)]
    pub partition_plan: Option<String>,
    #[serde(default)]
    pub os_image_size_gib: Option<u16>,
    #[serde(default)]
    pub data_image_size_gib: Option<u16>,
    #[serde(default)]
    pub os_image_publish_size_gib: Option<u16>,
    #[serde(default)]
    pub data_image_publish_size_gib: Option<u16>,
    #[serde(default)]
    pub features: BTreeMap<String, bool>,
}

impl Variant {
    /// Emit the four-element chain plus any wiring at a namespace
    /// rooted at `//variants/<name>/`.
    ///
    /// `namespace` is the incoming `//variants` prefix from
    /// [`Config::walk`]. The variant's name from the toml is used
    /// verbatim as the next segment.
    pub fn to_edo(&self, namespace: &Addr) -> ContextResult<Schema> {
        let variant_addr = namespace.join(&Addr::parse(&self.name)?);
        let rootfs_addr = variant_addr.join(&Addr::parse("rootfs")?);
        let disk_addr = variant_addr.join(&Addr::parse("disk")?);
        let grub_addr = variant_addr.join(&Addr::parse("grub")?);

        // The SDK addr is `//<sdk.name>` per `config/twoliter.rs`.
        // Every stage runs in `//sdk-isolated` (matches kit +
        // package transforms).
        let sdk_env = Addr::parse("//sdk-isolated")?;
        let sdk_addr = Addr::parse("//bottlerocket-sdk")?;

        // Resolve `kits` addresses. The kit's transform address is
        // the `<addr>/build` sub-address by convention (see
        // `config/kit.rs`); we accept both the base kit addr and
        // the `/build` sub-addr for future-proofing.
        let kits: Vec<Addr> = self.kit.iter().map(|k| k.addr.clone()).collect();

        // Packages come from `included-packages.packages`.
        // Historically these are RPM build transforms at
        // `//packages/<name>/rpm`.
        let packages: Vec<Addr> = self
            .included_packages
            .packages
            .iter()
            .map(|name| Addr::parse(&format!("//packages/{name}/rpm")))
            .collect::<Result<Vec<_>, _>>()?;

        // Guest images: each entry's `addr` verbatim.
        let guest_images: Vec<Addr> = self.guest_image.iter().map(|g| g.addr.clone()).collect();

        // Assemble the shared `VariantImageOptions` config block used
        // by every stage's element.
        let base_options = self.base_options(&kits, &packages, &guest_images);

        let mut transforms = BTreeMap::new();

        // Rootfs element. Every stage carries the union
        // `VariantImageOptions`, but only the rootfs stage actually
        // reads `kits`/`packages`/`guest-images` for its
        // `depends()`. The others fill those fields with empty lists
        // so their `depends()` doesn't see spurious edges.
        transforms.insert(
            rootfs_addr.clone(),
            Element::builder()
                .addr(rootfs_addr.clone())
                .kind("variant-rootfs")
                .environment(sdk_env.clone())
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![sdk_addr.clone()],
                )])))
                .config(base_options.clone())
                .build(),
        );

        // Disk element.
        let mut disk_config = strip_deps(&base_options);
        disk_config.insert("rootfs-addr".to_string(), json!(rootfs_addr.to_string()));
        transforms.insert(
            disk_addr.clone(),
            Element::builder()
                .addr(disk_addr.clone())
                .kind("variant-disk")
                .environment(sdk_env.clone())
                .config(disk_config)
                .build(),
        );

        // Grub element.
        let mut grub_config = strip_deps(&base_options);
        grub_config.insert("rootfs-addr".to_string(), json!(rootfs_addr.to_string()));
        grub_config.insert("disk-addr".to_string(), json!(disk_addr.to_string()));
        transforms.insert(
            grub_addr.clone(),
            Element::builder()
                .addr(grub_addr.clone())
                .kind("variant-grub")
                .environment(sdk_env.clone())
                .config(grub_config)
                .build(),
        );

        // Image (finalizer) element.
        let mut image_config = strip_deps(&base_options);
        image_config.insert("grub-addr".to_string(), json!(grub_addr.to_string()));
        transforms.insert(
            variant_addr.clone(),
            Element::builder()
                .addr(variant_addr.clone())
                .kind("variant-image")
                .environment(sdk_env.clone())
                .config(image_config)
                .build(),
        );

        Ok(Schema::builder().transform(transforms).build())
    }

    /// Compose the `VariantImageOptions`-shaped JSON block used by
    /// every stage's element.
    fn base_options(
        &self,
        kits: &[Addr],
        packages: &[Addr],
        guest_images: &[Addr],
    ) -> BTreeMap<String, Value> {
        let mut cfg = BTreeMap::new();
        cfg.insert("name".to_string(), json!(&self.name));
        cfg.insert("version".to_string(), json!(self.version.to_string()));
        if let Some(arch) = &self.arch {
            cfg.insert("arch".to_string(), json!(arch));
        }
        if let Some(fmt) = &self.image.format {
            cfg.insert("image-format".to_string(), json!(fmt));
        }
        if let Some(plan) = &self.image.partition_plan {
            cfg.insert("partition-plan".to_string(), json!(plan));
        }
        if let Some(size) = self.image.os_image_size_gib {
            cfg.insert("os-image-size-gib".to_string(), json!(size));
        }
        if let Some(size) = self.image.data_image_size_gib {
            cfg.insert("data-image-size-gib".to_string(), json!(size));
        }
        if let Some(size) = self.image.os_image_publish_size_gib {
            cfg.insert("os-image-publish-size-gib".to_string(), json!(size));
        }
        if let Some(size) = self.image.data_image_publish_size_gib {
            cfg.insert("data-image-publish-size-gib".to_string(), json!(size));
        }
        cfg.insert(
            "kernel-parameters".to_string(),
            json!(&self.kernel_parameters),
        );
        // features: mirror the `SerializableFeatures` bitset. Any
        // feature not named in the toml stays at its default (all
        // `false` except `first-party-stack` which defaults to
        // `true`, per `options.rs::SerializableFeatures::default`).
        // We only emit keys the user explicitly set — the transform's
        // deserializer fills the rest from `Default`.
        if !self.image.features.is_empty() {
            cfg.insert(
                "image-features".to_string(),
                json!(&self.image.features),
            );
        }
        if !self.included_packages.packages.is_empty() {
            cfg.insert(
                "included-packages".to_string(),
                json!({ "packages": &self.included_packages.packages }),
            );
        }
        cfg.insert(
            "kits".to_string(),
            json!(
                kits.iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
            ),
        );
        cfg.insert(
            "packages".to_string(),
            json!(
                packages
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
            ),
        );
        cfg.insert(
            "guest-images".to_string(),
            json!(
                guest_images
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
            ),
        );
        cfg
    }
}

/// Return a copy of the base config with `kits`, `packages`, and
/// `guest-images` set to empty arrays.
///
/// The disk, grub, and image stages don't need to see those dep
/// addresses (they take single upstream-stage addresses instead), and
/// their `TransformImpl::depends()` iterates over the options'
/// kits/packages/guest-images fields — so we strip them here to keep
/// each stage's `depends()` accurate to the transform-graph shape.
fn strip_deps(base: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut out = base.clone();
    out.insert("kits".to_string(), json!(Vec::<String>::new()));
    out.insert("packages".to_string(), json!(Vec::<String>::new()));
    out.insert("guest-images".to_string(), json!(Vec::<String>::new()));
    out
}

// Silence unused-import warning when BTreeSet is only referenced in tests below.
#[allow(dead_code)]
fn _unused_set_import() -> BTreeSet<String> {
    BTreeSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Variant {
        toml::from_str(toml_str).expect("valid variant config")
    }

    #[test]
    fn emits_four_transforms_at_expected_addresses() {
        let v = parse(
            r#"
name = "aws-k8s-1.29"
version = "0.1.0"

[[kit]]
addr = "//kits/bottlerocket/core-kit"

[image]
format = "raw"
partition-plan = "split"
"#,
        );
        let namespace = Addr::parse("//variants").unwrap();
        let schema = v.to_edo(&namespace).expect("to_edo");
        let addrs: Vec<String> =
            schema.transforms().keys().map(|a| a.to_string()).collect();
        assert!(addrs.contains(&"//variants/aws-k8s-1.29/rootfs".to_string()));
        assert!(addrs.contains(&"//variants/aws-k8s-1.29/disk".to_string()));
        assert!(addrs.contains(&"//variants/aws-k8s-1.29/grub".to_string()));
        assert!(addrs.contains(&"//variants/aws-k8s-1.29".to_string()));
    }

    #[test]
    fn kinds_match_the_transform_registry() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        let by_kind: BTreeMap<String, String> = schema
            .transforms()
            .iter()
            .map(|(a, e)| (a.to_string(), e.kind.clone()))
            .collect();
        assert_eq!(by_kind["//variants/x/rootfs"], "variant-rootfs");
        assert_eq!(by_kind["//variants/x/disk"], "variant-disk");
        assert_eq!(by_kind["//variants/x/grub"], "variant-grub");
        assert_eq!(by_kind["//variants/x"], "variant-image");
    }

    #[test]
    fn dep_addrs_are_wired_in_config() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        let disk = schema
            .transforms()
            .get(&Addr::parse("//variants/x/disk").unwrap())
            .unwrap();
        assert_eq!(
            disk.config.get("rootfs-addr").and_then(|v| v.as_str()),
            Some("//variants/x/rootfs")
        );
        let grub = schema
            .transforms()
            .get(&Addr::parse("//variants/x/grub").unwrap())
            .unwrap();
        assert_eq!(
            grub.config.get("rootfs-addr").and_then(|v| v.as_str()),
            Some("//variants/x/rootfs")
        );
        assert_eq!(
            grub.config.get("disk-addr").and_then(|v| v.as_str()),
            Some("//variants/x/disk")
        );
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert_eq!(
            image.config.get("grub-addr").and_then(|v| v.as_str()),
            Some("//variants/x/grub")
        );
    }

    #[test]
    fn kits_and_packages_only_appear_on_rootfs_stage() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[[kit]]
addr = "//kits/bottlerocket/core-kit"

[included-packages]
packages = ["pkg-e"]
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let kits = rootfs.config.get("kits").unwrap().as_array().unwrap();
        assert_eq!(kits.len(), 1);
        let packages = rootfs.config.get("packages").unwrap().as_array().unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].as_str(), Some("//packages/pkg-e/rpm"));

        // The other stages have empty kits/packages so their
        // `depends()` doesn't return spurious edges.
        for stage in ["disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            let kits = elem.config.get("kits").unwrap().as_array().unwrap();
            assert!(kits.is_empty(), "stage {stage} should have empty kits");
            let pkgs = elem.config.get("packages").unwrap().as_array().unwrap();
            assert!(pkgs.is_empty(), "stage {stage} should have empty packages");
        }
        // The finalizer also has empty kits/packages.
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let kits = image.config.get("kits").unwrap().as_array().unwrap();
        assert!(kits.is_empty());
    }

    #[test]
    fn kernel_parameters_are_carried_verbatim_on_every_stage() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
kernel-parameters = ["quiet", "console=ttyS0"]
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        for stage in ["rootfs", "disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            let kp = elem
                .config
                .get("kernel-parameters")
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(kp.len(), 2, "stage {stage}");
        }
    }

    #[test]
    fn image_features_pass_through() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image.features]
uefi-secure-boot = true
first-party-stack = true
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        let grub = schema
            .transforms()
            .get(&Addr::parse("//variants/x/grub").unwrap())
            .unwrap();
        let features = grub.config.get("image-features").unwrap();
        assert_eq!(
            features.get("uefi-secure-boot").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn top_level_variant_addr_matches_pre_decomp_convention() {
        // Downstream tooling addresses variants at `//variants/<name>`;
        // this must remain the finalizer's address after
        // decomposition or every `twoliter publish` call breaks.
        let v = parse(
            r#"
name = "aws-k8s-1.29"
version = "0.1.0"
"#,
        );
        let schema = v.to_edo(&Addr::parse("//variants").unwrap()).unwrap();
        let finalizer_addr = Addr::parse("//variants/aws-k8s-1.29").unwrap();
        let elem = schema
            .transforms()
            .get(&finalizer_addr)
            .expect("finalizer at unchanged top-level address");
        assert_eq!(elem.kind, "variant-image");
    }
}
