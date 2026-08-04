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

use crate::image::ImageFormat;

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

    /// Arches this variant is allowed to build on. Absent = any arch.
    /// Emitted verbatim into every stage's config as `supported-arches`;
    /// enforcement is the transform's responsibility.
    #[serde(default)]
    pub supported_arches: Option<Vec<String>>,

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

    /// The `[variant.local-packages]` sub-table: names of in-repo
    /// packages (i.e. those with a `packages/<dir>/package.toml`)
    /// that should be built into the variant.
    ///
    /// Every name here becomes a `//packages/<name>/rpm` transform
    /// dependency on the rootfs element, plus an entry in the
    /// dnf-install selection so the RPM is actually installed (not
    /// just staged in the local repo).
    ///
    /// The set is **transitively closed** at emit time via each
    /// package's `[package].depends` list. Users only need to
    /// enumerate the top-level packages they care about — reachable
    /// deps (`storewolf`, `settings-defaults`, …) come along for
    /// free. Names naming packages not in the project are dropped
    /// with a warning at emit time (see `Config::walk`).
    ///
    /// Rationale: `[variant.included-packages]` is for kit-supplied
    /// names — their RPMs arrive via the kit-image source scope and
    /// there is no `//packages/<name>/rpm` transform to depend on.
    /// Mixing the two in one list forced the emitter to guess which
    /// bucket a name belongs to; splitting them makes the intent
    /// explicit and lets the emitter fail loudly when a local name
    /// is misspelled.
    #[serde(default)]
    pub local_packages: LocalPackagesBlock,
}

fn default_version() -> semver::Version {
    semver::Version::new(0, 0, 0)
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

/// The `[variant.local-packages]` sub-table — see the doc comment on
/// [`Variant::local_packages`].
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LocalPackagesBlock {
    #[serde(default)]
    pub packages: Vec<String>,
}

/// The `[variant.image]` sub-table plus its nested `features`.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct VariantImageBlock {
    /// Image format on disk. One of `raw`, `vmdk`, `qcow2`, `eif`.
    #[serde(default)]
    pub format: Option<ImageFormat>,
    /// Partition-plan; `split` or `unified`.
    #[serde(default)]
    pub partition_plan: Option<String>,
    #[serde(default)]
    pub os_image_size_gib: Option<u16>,
    #[serde(default)]
    pub data_image_size_gib: Option<u16>,
    /// Single publish-size hint (in GiB). At emit time this is
    /// broadcast to both `os-image-publish-size-gib` and
    /// `data-image-publish-size-gib` on the transform config; the
    /// transform side (`transform/variant/layout.rs::image_layout`)
    /// treats the two fields as one hint via `.or()` fallback.
    #[serde(default)]
    pub publish_image_size_hint_gib: Option<u16>,
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
    ///
    /// `kits` is the resolved list of `//kits/<vendor>/<name>`
    /// addresses drawn from the project-level `[[kit]]` list in
    /// `Twoliter.toml`. Every variant implicitly receives every
    /// project kit; the caller (`config/mod.rs::Config::variant_toml`)
    /// is responsible for the resolution.
    ///
    /// **Design note.** This is a deliberate simplification of old
    /// twoliter's per-variant kit selection (which flowed through
    /// Cargo `[build-dependencies]`). The tradeoff:
    /// - Every variant depends on every project kit as a `kit-image`
    ///   source, so a change to *any* project kit invalidates the
    ///   rootfs cache for *every* variant
    ///   (`transform/variant/hash.rs::rootfs_hash_changes_on_kit_digest_change`).
    /// - The scheduler waits on all kit requirements to resolve before
    ///   the rootfs stage's `FromElement` runs (same guarantee the SDK
    ///   source relies on).
    /// - Config authors get one place to declare kits
    ///   (`Twoliter.toml`), not two.
    ///
    /// Acceptable for projects with a small kit set where every kit
    /// is used by every variant (the Bottlerocket house layout). If a
    /// project outgrows this, add a `kits = ["name", ...]` narrowing
    /// field on `Variant` that filters this slice by kit name.
    ///
    /// **Wire shape for kits.** Kits enter the schema as `kit-image`
    /// source `Requirement`s at `//kits/<vendor>/<name>` (emitted by
    /// `Twoliter::to_edo`). They are *not* transforms. This emitter
    /// therefore attaches them to the rootfs element's `source` field
    /// under a `"kits"` scope — parallel to the SDK's `"default"`
    /// scope — where `VariantRootfsTransform::new` resolves them via
    /// `ctx.add_source(...)` into a `Vec<Source>`. Kits never appear
    /// in any stage's `config` map or `depends()` output.
    ///
    /// **Wire shape for included / local packages.** Entries fall into
    /// two disjoint buckets, sourced from two separate blocks so the
    /// emitter never has to guess:
    ///
    /// - **Kit-supplied** names live under
    ///   `[variant.included-packages].packages`. No transform exists
    ///   at `//packages/<name>/rpm` for these; their RPMs arrive in
    ///   `deps/rpms/` through the rootfs element's `kits` source
    ///   scope. Emitted verbatim into every stage's
    ///   `included-packages` config field (stripped to empty on
    ///   non-rootfs stages by `strip_deps`) so
    ///   `VariantRootfsTransform` can hand them to `dnf install`.
    ///
    /// - **In-repo** names live under
    ///   `[variant.local-packages].packages`. Each becomes a
    ///   `//packages/<name>/rpm` transform-graph dependency on the
    ///   rootfs element (via the config-side `packages` field);
    ///   `VariantRootfsTransform::stage` then causes those RPMs to
    ///   build and stage into `deps/rpms/`. The set is **transitively
    ///   closed** here via each package's `[package].depends` list
    ///   (supplied to this fn as `project_packages: name -> depends`)
    ///   so a user only enumerates the top-level packages they care
    ///   about. Every name in the closure is also appended to the
    ///   dnf install list so it is actually installed, not just
    ///   staged.
    ///
    /// Names in `local-packages` that don't appear as a project
    /// package are dropped from the transform-dep list (they still
    /// end up in the install list — dnf can then produce a clear
    /// "nothing provides" error if the RPM never lands). Names that
    /// appear in both blocks are deduped in the install list.
    pub fn to_edo(
        &self,
        namespace: &Addr,
        kits: &[Addr],
        project_packages: &BTreeMap<String, Vec<String>>,
    ) -> ContextResult<Schema> {
        let variant_addr = namespace.join(&Addr::parse(&self.name)?);
        let rootfs_addr = variant_addr.join(&Addr::parse("rootfs")?);
        let disk_addr = variant_addr.join(&Addr::parse("disk")?);
        let grub_addr = variant_addr.join(&Addr::parse("grub")?);

        // The SDK addr is `//<sdk.name>` per `config/twoliter.rs`.
        // Every stage runs in `//sdk-isolated` (matches kit +
        // package transforms).
        let sdk_env = Addr::parse("//sdk-isolated")?;
        let sdk_addr = Addr::parse("//bottlerocket-sdk")?;

        // Transitively close `[variant.local-packages].packages` over
        // the in-repo `[package].depends` graph. BFS keeps ordering
        // stable and short-circuits cycles.
        let local_closure: BTreeSet<String> =
            close_local_packages(&self.local_packages.packages, project_packages);

        // Full install-name list = kit-supplied `included-packages`
        // + every in-repo package in the transitive closure. Both
        // buckets carry short names (the `bottlerocket-` prefix is
        // added downstream in `VariantRootfsTransform`). Dedupe via
        // BTreeSet so a package that appears in both blocks doesn't
        // land in the dnf command line twice.
        let mut install_names: BTreeSet<String> = BTreeSet::new();
        install_names.extend(self.included_packages.packages.iter().cloned());
        install_names.extend(local_closure.iter().cloned());
        let all_included: Vec<String> = install_names.into_iter().collect();

        // Transform-graph deps: one `//packages/<name>/rpm` per name
        // in `local_closure` that actually corresponds to an in-repo
        // package. Names outside `project_packages` are dropped here
        // (dnf will surface the missing-provider error at install
        // time). Sorted by iteration over BTreeSet — stable output.
        let packages: Vec<Addr> = local_closure
            .iter()
            .filter(|name| project_packages.contains_key(name.as_str()))
            .map(|name| Addr::parse(&format!("//packages/{name}/rpm")))
            .collect::<Result<Vec<_>, _>>()?;

        // Guest images: each entry's `addr` verbatim.
        let guest_images: Vec<Addr> = self.guest_image.iter().map(|g| g.addr.clone()).collect();

        // Assemble the shared `VariantImageOptions` config block used
        // by every stage's element.
        let base_options = self.base_options(&all_included, &packages, &guest_images);

        let mut transforms = BTreeMap::new();

        // Rootfs element. Every stage carries the union
        // `VariantImageOptions`, but only the rootfs stage actually
        // reads `packages`/`guest-images` for its `depends()`. The
        // others fill those fields with empty lists so their
        // `depends()` doesn't see spurious edges. Kits are attached
        // separately as `kit-image` sources under the `"kits"` scope
        // — see the `to_edo` doc-comment above.
        let mut rootfs_scopes: BTreeMap<String, Vec<Addr>> =
            BTreeMap::from([("default".to_string(), vec![sdk_addr.clone()])]);
        if !kits.is_empty() {
            rootfs_scopes.insert("kits".to_string(), kits.to_vec());
        }
        transforms.insert(
            rootfs_addr.clone(),
            Element::builder()
                .addr(rootfs_addr.clone())
                .kind("variant-rootfs")
                .environment(sdk_env.clone())
                .source(SourceMap::Unresolved(rootfs_scopes))
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
    ///
    /// Kits are intentionally absent here: they flow through the
    /// rootfs element's `source` field as `kit-image` sources, not
    /// through the config-side dep list. See `to_edo`'s doc-comment.
    ///
    /// - `all_included` is the full unfiltered `included-packages`
    ///   name list (both in-repo and kit-supplied). Emitted verbatim
    ///   under the `included-packages` key as a flat `Vec<String>`.
    /// - `packages` is the *filtered* list of `//packages/<name>/rpm`
    ///   transform addresses (in-repo only).
    fn base_options(
        &self,
        all_included: &[String],
        packages: &[Addr],
        guest_images: &[Addr],
    ) -> BTreeMap<String, Value> {
        let mut cfg = BTreeMap::new();
        cfg.insert("name".to_string(), json!(&self.name));
        cfg.insert("version".to_string(), json!(self.version.to_string()));
        if let Some(arch) = &self.arch {
            cfg.insert("arch".to_string(), json!(arch));
        }
        if let Some(arches) = &self.supported_arches {
            cfg.insert("supported-arches".to_string(), json!(arches));
        }
        if let Some(fmt) = &self.image.format {
            cfg.insert("image-format".to_string(), json!(fmt.to_string()));
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
        // A single user-facing `publish-image-size-hint-gib` is
        // broadcast to both fields the transform side still consumes
        // (`transform/variant/layout.rs::image_layout` treats them as
        // one hint via `.or()` fallback).
        if let Some(size) = self.image.publish_image_size_hint_gib {
            cfg.insert("os-image-publish-size-gib".to_string(), json!(size));
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
            cfg.insert("image-features".to_string(), json!(&self.image.features));
        }
        // Always emit `included-packages` as a flat `Vec<String>` (an
        // empty array when the variant declares none). Downstream
        // consumers (`VariantImageOptions::included_packages`) rely on
        // the field being a plain string list rather than a nested
        // `{ packages: [...] }` block.
        cfg.insert("included-packages".to_string(), json!(all_included));
        cfg.insert(
            "packages".to_string(),
            json!(packages.iter().map(|a| a.to_string()).collect::<Vec<_>>()),
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

/// BFS the transitive closure of `roots` over the in-repo package
/// dependency graph.
///
/// - `roots`: names from `[variant.local-packages].packages`.
/// - `graph`: `package.name -> [package.depends]` collected during
///   the packages walk.
///
/// Returns the sorted (`BTreeSet`) closure. Missing edges (a dep
/// naming a package the project doesn't have) are ignored here — the
/// missing name is still added to the set (so it lands in the dnf
/// install list) but no transform dep is emitted for it. Cycles are
/// tolerated: nodes already in the closure are skipped.
///
/// Kept module-scoped rather than as an inherent method because it
/// doesn't touch any of the surrounding [`Variant`] state.
fn close_local_packages(
    roots: &[String],
    graph: &BTreeMap<String, Vec<String>>,
) -> BTreeSet<String> {
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<String> = roots.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        if !closure.insert(name.clone()) {
            continue;
        }
        if let Some(deps) = graph.get(&name) {
            for dep in deps {
                if !closure.contains(dep) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }
    closure
}

/// Return a copy of the base config with `packages`, `guest-images`,
/// and `included-packages` set to empty arrays.
///
/// The disk, grub, and image stages don't need to see those dep
/// addresses (they take single upstream-stage addresses instead), and
/// their `TransformImpl::depends()` iterates over the options'
/// packages/guest-images fields — so we strip them here to keep
/// each stage's `depends()` accurate to the transform-graph shape.
///
/// `included-packages` is also zeroed: only the rootfs stage will
/// hand the name list to `dnf install` (see `to_edo`'s doc-comment).
///
/// Kits are not represented in the config map at all; they live on
/// the rootfs element's `source` field only.
fn strip_deps(base: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut out = base.clone();
    out.insert("packages".to_string(), json!(Vec::<String>::new()));
    out.insert("guest-images".to_string(), json!(Vec::<String>::new()));
    out.insert("included-packages".to_string(), json!(Vec::<String>::new()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Variant {
        toml::from_str(toml_str).expect("valid variant config")
    }

    fn kits(addrs: &[&str]) -> Vec<Addr> {
        addrs.iter().map(|a| Addr::parse(a).unwrap()).collect()
    }

    /// Build a `BTreeMap` of in-repo package short names → their
    /// declared `[package].depends` list — the same shape the
    /// packages walk builds. Every entry here uses an empty deps
    /// vector; tests that need a real graph should construct it
    /// inline.
    fn pkgs(names: &[&str]) -> BTreeMap<String, Vec<String>> {
        names.iter().map(|s| (s.to_string(), Vec::new())).collect()
    }

    /// Build a `BTreeMap<name, depends>` from `(name, [deps])` tuples.
    fn pkgs_with_deps(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(name, deps)| {
                (
                    name.to_string(),
                    deps.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    /// Read the unresolved-side scope table from an `Element`. Every
    /// element this emitter builds is pre-resolution, so a
    /// `Resolved` variant here would be a bug.
    fn unresolved_scopes(elem: &Element) -> BTreeMap<String, Vec<Addr>> {
        match elem.source.as_ref() {
            Some(SourceMap::Unresolved(table)) => table.clone(),
            Some(SourceMap::Resolved(_)) => {
                panic!("emitter should produce Unresolved source maps")
            }
            None => BTreeMap::new(),
        }
    }

    #[test]
    fn emits_four_transforms_at_expected_addresses() {
        let v = parse(
            r#"
name = "aws-k8s-1.29"
version = "0.1.0"

[image]
format = "raw"
partition-plan = "split"
"#,
        );
        let namespace = Addr::parse("//variants").unwrap();
        let schema = v
            .to_edo(
                &namespace,
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&[]),
            )
            .expect("to_edo");
        let addrs: Vec<String> = schema.transforms().keys().map(|a| a.to_string()).collect();
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
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
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
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
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
    fn packages_only_appear_on_rootfs_stage() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[local-packages]
packages = ["pkg-e"]
"#,
        );
        // `pkg-e` is in-repo (present in `project_packages`), so the
        // emitter resolves it to a `//packages/pkg-e/rpm` transform
        // dep AND lands it in the flat `included-packages` install
        // list.
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&["pkg-e"]),
            )
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let packages = rootfs.config.get("packages").unwrap().as_array().unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].as_str(), Some("//packages/pkg-e/rpm"));

        // Full install list flows through the flat `included-packages`
        // config field on the rootfs stage.
        let included = rootfs
            .config
            .get("included-packages")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].as_str(), Some("pkg-e"));

        // Kits never appear in any stage's config map — they flow
        // through the rootfs element's `source` field instead.
        for stage in ["rootfs", "disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            assert!(
                elem.config.get("kits").is_none(),
                "stage {stage} must not carry a config-side `kits` field",
            );
        }
        let image_elem = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert!(image_elem.config.get("kits").is_none());

        // The non-rootfs stages have empty packages and empty
        // included-packages so their `depends()` doesn't return
        // spurious edges and dnf on downstream stages sees nothing.
        for stage in ["disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            let pkgs_arr = elem.config.get("packages").unwrap().as_array().unwrap();
            assert!(
                pkgs_arr.is_empty(),
                "stage {stage} should have empty packages"
            );
            let included_arr = elem
                .config
                .get("included-packages")
                .unwrap()
                .as_array()
                .unwrap();
            assert!(
                included_arr.is_empty(),
                "stage {stage} should have empty included-packages",
            );
        }
    }

    /// Kit-supplied names (from `[variant.included-packages]`) never
    /// generate transform-graph deps. They flow through the config's
    /// flat `included-packages` install list to the rootfs stage's
    /// dnf command line, and only there.
    #[test]
    fn kit_supplied_packages_do_not_become_transform_addrs() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[included-packages]
packages = ["release", "kernel-6.18"]
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&[]),
            )
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let packages = rootfs.config.get("packages").unwrap().as_array().unwrap();
        assert!(
            packages.is_empty(),
            "kit-supplied names must not synthesise `//packages/<name>/rpm` addrs; got {packages:?}",
        );
        let included: Vec<&str> = rootfs
            .config
            .get("included-packages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Alphabetically sorted (via BTreeSet dedupe).
        assert_eq!(included, vec!["kernel-6.18", "release"]);
    }

    /// A mixed configuration splits by block: `included-packages`
    /// stays kit-supplied (no transform dep), `local-packages`
    /// resolves to a transform dep, and the flat install list on
    /// the rootfs stage is the union of both.
    #[test]
    fn mixed_in_repo_and_kit_packages_split_correctly() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[included-packages]
packages = ["release"]

[local-packages]
packages = ["settings-defaults"]
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &[],
                &pkgs(&["settings-defaults"]),
            )
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();

        // Only the in-repo name (from `[local-packages]`) becomes a
        // transform dep.
        let packages: Vec<&str> = rootfs
            .config
            .get("packages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(packages, vec!["//packages/settings-defaults/rpm"]);

        // Install list = union of both blocks, sorted by BTreeSet.
        let included: Vec<&str> = rootfs
            .config
            .get("included-packages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(included, vec!["release", "settings-defaults"]);
    }

    /// `[variant.local-packages]` is transitively closed via each
    /// in-repo package's `[package].depends`. A user listing only
    /// the top-level `release` pulls in `os` and `storewolf` and
    /// `settings-defaults` automatically.
    #[test]
    fn local_packages_transitively_closed_via_depends_graph() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[local-packages]
packages = ["release"]
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &[],
                &pkgs_with_deps(&[
                    ("release", &["os"]),
                    ("os", &["storewolf"]),
                    ("storewolf", &["settings-defaults"]),
                    ("settings-defaults", &[]),
                    ("unrelated", &[]),
                ]),
            )
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();

        let packages: Vec<&str> = rootfs
            .config
            .get("packages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // BTreeSet sort → alphabetical.
        assert_eq!(
            packages,
            vec![
                "//packages/os/rpm",
                "//packages/release/rpm",
                "//packages/settings-defaults/rpm",
                "//packages/storewolf/rpm",
            ],
        );

        let included: Vec<&str> = rootfs
            .config
            .get("included-packages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            included,
            vec!["os", "release", "settings-defaults", "storewolf"],
        );
    }

    /// Cycles in the dep graph don't hang the closure.
    #[test]
    fn local_packages_closure_tolerates_cycles() {
        let closure = close_local_packages(
            &["a".to_string()],
            &pkgs_with_deps(&[("a", &["b"]), ("b", &["a"])]),
        );
        let names: Vec<&String> = closure.iter().collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    /// Regression guard: when the variant declares no included
    /// packages at all, the rootfs config still carries an
    /// `included-packages` key with an empty array (the transform's
    /// deserializer defaults to `Vec::new()` either way, but the
    /// present-but-empty shape is what we explicitly emit).
    #[test]
    fn included_packages_absent_leaves_config_empty_or_off() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        assert!(
            rootfs
                .config
                .get("packages")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            rootfs
                .config
                .get("included-packages")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    /// Kits are attached to the rootfs element under the `"kits"`
    /// source scope; no other stage carries a kit source. The SDK
    /// stays under `"default"`.
    #[test]
    fn kits_appear_on_rootfs_source_scope_not_config() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let project_kits = kits(&[
            "//kits/bottlerocket/core-kit",
            "//kits/my-vendor/extra-3-kit",
        ]);
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &project_kits,
                &pkgs(&[]),
            )
            .unwrap();

        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(rootfs);

        // SDK still under `default`, unchanged.
        let default_scope = source_map.get("default").expect("default scope present");
        assert_eq!(default_scope.len(), 1);
        assert_eq!(default_scope[0].to_string(), "//bottlerocket-sdk");

        // Kits under `kits`, in the order supplied by the caller.
        let kits_scope = source_map.get("kits").expect("kits scope present");
        let kit_addrs: Vec<String> = kits_scope.iter().map(|a| a.to_string()).collect();
        assert_eq!(
            kit_addrs,
            vec![
                "//kits/bottlerocket/core-kit".to_string(),
                "//kits/my-vendor/extra-3-kit".to_string(),
            ]
        );

        // Non-rootfs stages carry no `kits` scope.
        for stage in ["disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            assert!(
                !unresolved_scopes(elem).contains_key("kits"),
                "stage {stage} must not carry a kits source scope",
            );
        }
        let image_elem = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert!(!unresolved_scopes(image_elem).contains_key("kits"));
    }

    /// Regression guard: an empty project-kit slice must not emit an
    /// empty `"kits"` scope on the rootfs element.
    #[test]
    fn no_kit_scope_emitted_when_project_has_no_kits() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(rootfs);
        assert!(!source_map.contains_key("kits"));
        assert!(source_map.contains_key("default"));
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
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
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
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
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
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        let finalizer_addr = Addr::parse("//variants/aws-k8s-1.29").unwrap();
        let elem = schema
            .transforms()
            .get(&finalizer_addr)
            .expect("finalizer at unchanged top-level address");
        assert_eq!(elem.kind, "variant-image");
    }

    #[test]
    fn kits_come_from_caller_slice_not_variant_toml() {
        // The variant.toml carries no `[[kit]]` block. Kits flow in
        // via `Variant::to_edo`'s `kits` argument, resolved by
        // `config/mod.rs::Config::variant_toml` from the project-level
        // `[[kit]]` list, and land on the rootfs element's `"kits"`
        // source scope (not in the config JSON).
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let project_kits = kits(&[
            "//kits/bottlerocket/core-kit",
            "//kits/my-vendor/extra-3-kit",
        ]);
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                &project_kits,
                &pkgs(&[]),
            )
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(rootfs);
        let ks: Vec<String> = source_map
            .get("kits")
            .expect("kits scope present")
            .iter()
            .map(|a| a.to_string())
            .collect();
        assert_eq!(
            ks,
            vec![
                "//kits/bottlerocket/core-kit".to_string(),
                "//kits/my-vendor/extra-3-kit".to_string(),
            ]
        );
    }

    #[test]
    fn variant_kit_block_is_a_hard_parse_error() {
        // Legacy `[[kit]]` block must not silently deserialize into
        // anything — `deny_unknown_fields` catches it.
        let err = toml::from_str::<Variant>(
            r#"
name = "x"
version = "0.1.0"

[[kit]]
addr = "//kits/bottlerocket/core-kit"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("kit"),
            "expected error to mention the unknown `kit` field, got: {err}"
        );
    }

    #[test]
    fn supported_arches_are_carried_on_every_stage_when_set() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
supported-arches = ["x86_64", "aarch64"]
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        for stage in ["rootfs", "disk", "grub"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(&format!("//variants/x/{stage}")).unwrap())
                .unwrap();
            let arches: Vec<&str> = elem
                .config
                .get("supported-arches")
                .unwrap_or_else(|| panic!("stage {stage} missing supported-arches"))
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(arches, vec!["x86_64", "aarch64"], "stage {stage}");
        }
    }

    #[test]
    fn supported_arches_absent_leaves_field_off() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        assert!(
            rootfs.config.get("supported-arches").is_none(),
            "field must be absent (not null) when unset"
        );
    }

    #[test]
    fn publish_size_hint_is_broadcast_to_both_transform_fields() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image]
publish-image-size-hint-gib = 22
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
            .unwrap();
        let rootfs = schema
            .transforms()
            .get(&Addr::parse("//variants/x/rootfs").unwrap())
            .unwrap();
        assert_eq!(
            rootfs
                .config
                .get("os-image-publish-size-gib")
                .and_then(|v| v.as_u64()),
            Some(22)
        );
        assert_eq!(
            rootfs
                .config
                .get("data-image-publish-size-gib")
                .and_then(|v| v.as_u64()),
            Some(22)
        );
    }

    #[test]
    fn image_format_accepts_all_four_variants() {
        for fmt in ["raw", "vmdk", "qcow2", "eif"] {
            let toml = format!(
                r#"
name = "x"
version = "0.1.0"

[image]
format = "{fmt}"
"#
            );
            let v = parse(&toml);
            let schema = v
                .to_edo(&Addr::parse("//variants").unwrap(), &[], &pkgs(&[]))
                .unwrap();
            let rootfs = schema
                .transforms()
                .get(&Addr::parse("//variants/x/rootfs").unwrap())
                .unwrap();
            assert_eq!(
                rootfs.config.get("image-format").and_then(|v| v.as_str()),
                Some(fmt),
                "format={fmt}"
            );
        }
    }

    #[test]
    fn image_format_rejects_bogus_values() {
        let err = toml::from_str::<Variant>(
            r#"
name = "x"
version = "0.1.0"

[image]
format = "bogus"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("bogus")
                || err.to_string().to_lowercase().contains("variant"),
            "expected format-enum error, got: {err}"
        );
    }
}
