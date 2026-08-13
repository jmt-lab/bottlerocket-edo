//! `variant.toml` schema: one per directory under `variants/`.
//!
//! Emits two elements per variant, sharing an address prefix
//! `//variants/<name>/`:
//!
//! | Address | Kind | Transform |
//! |---|---|---|
//! | `//variants/<name>/repo` | `variant-repo` | [`RepoTransform`] |
//! | `//variants/<name>` | `variant-image` | [`RPM2ImageTransform`] |
//!
//! The top-level address (`//variants/<name>`) is the image finalizer,
//! matching pre-decomposition downstream conventions (`twoliter publish`
//! still addresses `//variants/<name>`).
//!
//! **Shared TOML surface.** The `[variant]` block is deserialized
//! once, then a nested [`VariantOptions`]-shaped JSON config is
//! composed and emitted on each stage. The repo stage carries the
//! transform-graph deps for in-repo packages (`//packages/<n>/rpm`)
//! plus the install-name list on `os.packages`; the image stage
//! carries a single upstream dep on the repo element plus the list of
//! guest images.
//!
//! [`RepoTransform`]: crate::variant::RepoTransform
//! [`RPM2ImageTransform`]: crate::variant::RPM2ImageTransform
//! [`VariantOptions`]: crate::variant::VariantOptions

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use edo::context::{Addr, ContextResult, Element, Schema, SourceMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::variant::ImageFormat;

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
    /// dependency on the repo element, plus an entry in the
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
    #[serde(default)]
    pub features: BTreeMap<String, bool>,
}

impl Variant {
    /// Emit the two-element chain at a namespace rooted at
    /// `//variants/<name>/`.
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
    ///   repo cache for *every* variant.
    /// - The scheduler waits on all kit requirements to resolve before
    ///   the repo stage's `FromElement` runs (same guarantee the SDK
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
    /// therefore attaches them to the repo element's `source` field
    /// under a `"kits"` scope — parallel to the SDK's `"default"`
    /// scope — where `RepoTransform::new` resolves them via
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
    ///   `deps/rpms/` through the repo element's `kits` source scope.
    ///   Emitted verbatim into the repo stage's `os.packages` field
    ///   (the flat dnf install list).
    ///
    /// - **In-repo** names live under
    ///   `[variant.local-packages].packages`. Each becomes a
    ///   `//packages/<name>/rpm` transform-graph dependency on the
    ///   repo element (via the config-side `depends` field);
    ///   `RepoTransform::stage` then causes those RPMs to build and
    ///   stage into `deps/rpms/`. The set is **transitively closed**
    ///   here via each package's `[package].depends` list (supplied
    ///   to this fn as `project_packages: name -> depends`) so a
    ///   user only enumerates the top-level packages they care about.
    ///   Every name in the closure is also appended to the dnf
    ///   install list so it is actually installed, not just staged.
    ///
    /// Names in `local-packages` that don't appear as a project
    /// package are dropped from the transform-dep list (they still
    /// end up in the install list — dnf can then produce a clear
    /// "nothing provides" error if the RPM never lands). Names that
    /// appear in both blocks are deduped in the install list.
    ///
    /// **Wire shape for the image stage.** The image element depends
    /// on a single upstream: the repo element's address. Guest-image
    /// addrs live on `guest-images` and are staged into the image's
    /// `images/` scratch dir at build time.
    /// `variant_dir` is the on-disk path to `variants/<name>/` — used
    /// solely to locate a sibling `template.ovf`, following old-twoliter's
    /// convention at `Makefile.toml:173`
    /// (`${BUILDSYS_ROOT_DIR}/variants/${BUILDSYS_VARIANT}/template.ovf`).
    /// The path is threaded through to `RPM2ImageTransform` unconditionally
    /// even when no template exists on disk; consumers only fault it in
    /// for VMDK builds (see `imghelper.sh::sanity_checks`).
    pub fn to_edo(
        &self,
        namespace: &Addr,
        variant_dir: &std::path::Path,
        kits: &[Addr],
        project_packages: &BTreeMap<String, Vec<String>>,
    ) -> ContextResult<Schema> {
        let variant_addr = namespace.join(&Addr::parse(&self.name)?);
        let repo_addr = variant_addr.join(&Addr::parse("repo")?);

        // Every stage runs in `//sdk-isolated` (matches kit + package
        // transforms).
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
        // added downstream in `RepoTransform`). Dedupe via BTreeSet
        // so a package that appears in both blocks doesn't land in
        // the dnf command line twice.
        let mut install_names: BTreeSet<String> = BTreeSet::new();
        install_names.extend(self.included_packages.packages.iter().cloned());
        install_names.extend(local_closure.iter().cloned());
        let os_packages: Vec<String> = install_names.into_iter().collect();

        // Transform-graph deps: one `//packages/<name>/rpm` per name
        // in `local_closure` that actually corresponds to an in-repo
        // package. Names outside `project_packages` are dropped here
        // (dnf will surface the missing-provider error at install
        // time). Sorted by iteration over BTreeSet — stable output.
        let package_deps: Vec<Addr> = local_closure
            .iter()
            .filter(|name| project_packages.contains_key(name.as_str()))
            .map(|name| Addr::parse(&format!("//packages/{name}/rpm")))
            .collect::<Result<Vec<_>, _>>()?;

        // Guest images: each entry's `addr` verbatim.
        let guest_images: Vec<Addr> = self.guest_image.iter().map(|g| g.addr.clone()).collect();

        let mut transforms = BTreeMap::new();

        // --- Repo stage ---
        // The repo element carries:
        //   - transform-graph deps on `//packages/<n>/rpm` via `depends`
        //   - the flat dnf install list via `os.packages`
        //   - the SDK under `default` and project kits under `kits`
        // Guest-images and image sizing/format live on the repo
        // element too because `VariantOptions` is shared; only the
        // image stage consults them.
        // Old-twoliter convention: `variants/<name>/template.ovf`. Emit
        // unconditionally; whether the file exists on disk is only checked
        // when `image.format = "vmdk"` by the downstream shell script.
        let template_ovf: std::path::PathBuf = variant_dir.join("template.ovf");
        let repo_config =
            self.variant_options_config(&package_deps, &guest_images, &os_packages, &template_ovf);
        let mut repo_scopes: BTreeMap<String, Vec<Addr>> =
            BTreeMap::from([("default".to_string(), vec![sdk_addr.clone()])]);
        if !kits.is_empty() {
            repo_scopes.insert("kits".to_string(), kits.to_vec());
        }
        transforms.insert(
            repo_addr.clone(),
            Element::builder()
                .addr(repo_addr.clone())
                .kind("variant-repo")
                .environment(sdk_env.clone())
                .source(SourceMap::Unresolved(repo_scopes))
                .config(repo_config)
                .build(),
        );

        // --- Image stage ---
        // The image element depends on a single upstream: the repo
        // element's address. Guest-image addrs stay on `guest-images`
        // for the image stage to iterate at build time.
        // `os.packages` is emptied — the image stage doesn't run dnf.
        //
        // Kits are also attached under a `"kits"` source scope — parallel
        // to the repo element — so the image transform can stage them
        // under `kits/<vendor>/<name>/<arch>/` and pass
        // `--external-kits-path=` to `rpm2img.sh`. The script's
        // `generate_application_inventory` helper reads each kit's
        // `repodata/` to rewrite `bottlerocket-`-prefixed package names
        // to their kit-native names and set the `Publisher` field to
        // the kit's vendor (see `imghelper.sh::generate_application_inventory`
        // and the `EXTERNAL_KIT_METADATA_PATH` block).
        let image_config = self.variant_options_config(
            &[repo_addr.clone()],
            &guest_images,
            &[],
            &template_ovf,
        );
        let image_element = if kits.is_empty() {
            Element::builder()
                .addr(variant_addr.clone())
                .kind("variant-image")
                .environment(sdk_env.clone())
                .config(image_config)
                .build()
        } else {
            let image_scopes: BTreeMap<String, Vec<Addr>> =
                BTreeMap::from([("kits".to_string(), kits.to_vec())]);
            Element::builder()
                .addr(variant_addr.clone())
                .kind("variant-image")
                .environment(sdk_env.clone())
                .source(SourceMap::Unresolved(image_scopes))
                .config(image_config)
                .build()
        };
        transforms.insert(variant_addr.clone(), image_element);

        Ok(Schema::builder().transform(transforms).build())
    }

    /// Compose a nested `VariantOptions`-shaped JSON block. Every
    /// stage carries a self-describing options blob keyed to the
    /// transform-side deserializer (`variant/options.rs`).
    ///
    /// - `depends`: transform-graph upstreams. For the repo stage
    ///   these are `//packages/<n>/rpm` addrs; for the image stage
    ///   this is a single-element `[repo_addr]`.
    /// - `guest_images`: guest-image transform addrs, staged by the
    ///   image transform into `images/`.
    /// - `os_packages`: flat dnf install list. Populated on the repo
    ///   stage; empty on the image stage (which doesn't run dnf).
    ///
    /// Kits are intentionally absent from this blob — they flow
    /// through the repo element's `source` field, not `config`.
    fn variant_options_config(
        &self,
        depends: &[Addr],
        guest_images: &[Addr],
        os_packages: &[String],
        template_ovf: &std::path::Path,
    ) -> BTreeMap<String, Value> {
        let mut cfg = BTreeMap::new();
        cfg.insert("name".to_string(), json!(&self.name));
        cfg.insert("version".to_string(), json!(self.version.to_string()));
        if let Some(arch) = &self.arch {
            cfg.insert("arch".to_string(), json!(arch));
        }
        cfg.insert(
            "depends".to_string(),
            json!(depends.iter().map(|a| a.to_string()).collect::<Vec<_>>()),
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
        cfg.insert("image".to_string(), self.image_options(template_ovf));
        cfg.insert("os".to_string(), self.os_options(os_packages));
        cfg
    }

    /// Compose the `[image]` sub-block matching `variant::ImageOptions`.
    /// Fields the caller left unset are omitted; the transform's
    /// deserializer defaults them (`format` and `partitions` have no
    /// defaults so they must be set in `variant.toml` for a real
    /// build — tests using the emitter without them will simply emit
    /// a partial block that fails downstream deserialization if
    /// consumed by the transform).
    fn image_options(&self, template_ovf: &std::path::Path) -> Value {
        let mut image = serde_json::Map::new();
        if let Some(fmt) = &self.image.format {
            image.insert("format".to_string(), json!(fmt.to_string()));
        }
        if let Some(plan) = &self.image.partition_plan {
            image.insert("partitions".to_string(), json!(plan));
        }
        if let Some(size) = self.image.os_image_size_gib {
            image.insert("os-size".to_string(), json!(size));
        }
        if let Some(size) = self.image.data_image_size_gib {
            image.insert("data-size".to_string(), json!(size));
        }
        // On-host path to the OVF template. Emitted unconditionally
        // (mirrors `build.Dockerfile:442`); the transform side only
        // stages the file if it actually exists, and only VMDK builds
        // ever read its contents.
        image.insert("ovf-template".to_string(), json!(template_ovf));
        // `ImageFeatures` deserializes from `BTreeMap<ImageFeature, bool>`;
        // we always emit the (possibly empty) table so the transform
        // side gets a definite value rather than falling back to
        // struct defaults.
        //
        // Apply old-twoliter's silent-default semantics
        // (`tools/buildsys/src/manifest.rs::ManifestInfo::image_features`):
        // for a full Bottlerocket image (neither `standalone-image` nor
        // `image-format = "eif"`), `in-place-updates`, `host-containers`,
        // and `external-kmod-development` default to on unless the
        // variant explicitly disables them. The stripped-down build
        // (standalone-image or EIF) instead defaults to `standalone-image`
        // + `external-kmod-development` only. Without this, a variant
        // that omits `in-place-updates` from `[variant.image.features]`
        // ends up with the metadata RPM providing
        // `image-feature(no-in-place-updates)`, which suppresses the
        // core-kit's `bork` dependency in `packages/os/os.spec`
        // (`Requires: (bork or image-feature(no-in-place-updates))`).
        // `bork` then never lands in the image, `sundog` at first boot
        // fails `bork seed`, and `preconfigured.target` collapses.
        image.insert("features".to_string(), json!(&self.resolved_features()));
        Value::Object(image)
    }

    /// Compute the effective image-feature map that should be shipped
    /// on the wire, applying old-twoliter's silent-default seeding.
    ///
    /// Rules (matching upstream `buildsys::manifest::ManifestInfo::image_features`):
    ///
    /// - Base seed = `{in-place-updates, host-containers,
    ///   external-kmod-development}` for a full image, or
    ///   `{standalone-image, external-kmod-development}` when the user
    ///   set `standalone-image = true` **or** picked
    ///   `image-format = "eif"` (EIF is inherently a stripped-down
    ///   single-bank image).
    /// - Explicit entries in `[variant.image.features]` override the
    ///   seed: `true` inserts, `false` removes.
    ///
    /// The output is a `BTreeMap<String, bool>` with `true` for every
    /// resolved-enabled feature — matching the wire shape emitted
    /// before this change (an all-`true` table). Features that end up
    /// disabled are omitted rather than emitted as `false`; the
    /// transform side treats missing entries as `false`, so the two
    /// encodings are equivalent.
    fn resolved_features(&self) -> BTreeMap<String, bool> {
        // Feature names we might seed. Kept as strings because
        // `VariantImageBlock::features` is `BTreeMap<String, bool>`
        // (the transform side is the only consumer of the strongly
        // typed `ImageFeature` enum).
        const IN_PLACE_UPDATES: &str = "in-place-updates";
        const HOST_CONTAINERS: &str = "host-containers";
        const EXTERNAL_KMOD_DEVELOPMENT: &str = "external-kmod-development";
        const STANDALONE_IMAGE: &str = "standalone-image";

        let standalone_explicit = self
            .image
            .features
            .get(STANDALONE_IMAGE)
            .copied()
            .unwrap_or(false);
        let is_eif = matches!(&self.image.format, Some(ImageFormat::Eif));
        let stripped = standalone_explicit || is_eif;

        let mut resolved: BTreeMap<String, bool> = BTreeMap::new();
        if stripped {
            resolved.insert(STANDALONE_IMAGE.to_string(), true);
            resolved.insert(EXTERNAL_KMOD_DEVELOPMENT.to_string(), true);
        } else {
            resolved.insert(IN_PLACE_UPDATES.to_string(), true);
            resolved.insert(HOST_CONTAINERS.to_string(), true);
            resolved.insert(EXTERNAL_KMOD_DEVELOPMENT.to_string(), true);
        }
        for (feature, flag) in &self.image.features {
            if *flag {
                resolved.insert(feature.clone(), true);
            } else {
                resolved.remove(feature);
            }
        }
        resolved
    }

    /// Compose the `[os]` sub-block matching `variant::OsOptions`.
    /// `os_packages` is the flat dnf install list; the repo stage
    /// hands it to `dnf install`, the image stage carries an empty
    /// list.
    fn os_options(&self, os_packages: &[String]) -> Value {
        json!({
            "kernel-parameters": &self.kernel_parameters,
            "packages": os_packages,
        })
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

    /// Pull the `depends` string list out of a stage's config.
    fn depends_of(elem: &Element) -> Vec<String> {
        elem.config
            .get("depends")
            .expect("depends field present")
            .as_array()
            .expect("depends is an array")
            .iter()
            .map(|v| v.as_str().expect("depends entry is a string").to_string())
            .collect()
    }

    /// Pull `os.packages` out of a stage's config.
    fn os_packages_of(elem: &Element) -> Vec<String> {
        elem.config
            .get("os")
            .expect("os block present")
            .get("packages")
            .expect("os.packages present")
            .as_array()
            .expect("os.packages is an array")
            .iter()
            .map(|v| v.as_str().expect("package entry is a string").to_string())
            .collect()
    }

    #[test]
    fn emits_two_transforms_at_expected_addresses() {
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
                std::path::Path::new("."),
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&[]),
            )
            .expect("to_edo");
        let addrs: BTreeSet<String> = schema.transforms().keys().map(|a| a.to_string()).collect();
        assert_eq!(
            addrs,
            BTreeSet::from([
                "//variants/aws-k8s-1.29/repo".to_string(),
                "//variants/aws-k8s-1.29".to_string(),
            ]),
            "expected exactly the repo + image finalizer pair",
        );
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
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        let by_kind: BTreeMap<String, String> = schema
            .transforms()
            .iter()
            .map(|(a, e)| (a.to_string(), e.kind.clone()))
            .collect();
        assert_eq!(by_kind["//variants/x/repo"], "variant-repo");
        assert_eq!(by_kind["//variants/x"], "variant-image");
    }

    /// The image stage's `depends` list is exactly `[<repo_addr>]`.
    /// The image transform stages that single upstream's artifact
    /// (the filtered dnf-install tarball) into its `rpms/` scratch
    /// dir; anything else routed here would produce spurious edges
    /// and duplicate stagings.
    #[test]
    fn image_stage_depends_on_the_repo_stage() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert_eq!(depends_of(image), vec!["//variants/x/repo".to_string()]);
    }

    #[test]
    fn local_packages_land_as_repo_stage_depends_and_install_names() {
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
        // dep on the repo stage AND lands it on `os.packages`.
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&["pkg-e"]),
            )
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        assert_eq!(depends_of(repo), vec!["//packages/pkg-e/rpm".to_string()]);
        assert_eq!(os_packages_of(repo), vec!["pkg-e".to_string()]);

        // Kits never appear in any stage's config map — they flow
        // through the repo element's `source` field instead.
        for stage_addr in ["//variants/x/repo", "//variants/x"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(stage_addr).unwrap())
                .unwrap();
            assert!(
                elem.config.get("kits").is_none(),
                "stage {stage_addr} must not carry a config-side `kits` field",
            );
        }

        // The image stage carries an empty `os.packages` (it doesn't
        // run dnf) and no `//packages/<n>/rpm` transform deps.
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert!(
            os_packages_of(image).is_empty(),
            "image stage must not carry the dnf install list",
        );
        assert_eq!(
            depends_of(image),
            vec!["//variants/x/repo".to_string()],
            "image stage depends only on the repo stage",
        );
    }

    /// Kit-supplied names (from `[variant.included-packages]`) never
    /// generate transform-graph deps. They flow through the repo
    /// stage's `os.packages` install list to the dnf command line,
    /// and only there.
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
                std::path::Path::new("."),
                &kits(&["//kits/bottlerocket/core-kit"]),
                &pkgs(&[]),
            )
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        assert!(
            depends_of(repo).is_empty(),
            "kit-supplied names must not synthesise `//packages/<n>/rpm` addrs; got {:?}",
            depends_of(repo),
        );
        // Alphabetically sorted (via BTreeSet dedupe).
        assert_eq!(
            os_packages_of(repo),
            vec!["kernel-6.18".to_string(), "release".to_string()],
        );
    }

    /// A mixed configuration splits by block: `included-packages`
    /// stays kit-supplied (no transform dep), `local-packages`
    /// resolves to a transform dep, and the flat install list on
    /// the repo stage is the union of both.
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
                std::path::Path::new("."),
                &[],
                &pkgs(&["settings-defaults"]),
            )
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();

        // Only the in-repo name (from `[local-packages]`) becomes a
        // transform dep.
        assert_eq!(
            depends_of(repo),
            vec!["//packages/settings-defaults/rpm".to_string()],
        );

        // Install list = union of both blocks, sorted by BTreeSet.
        assert_eq!(
            os_packages_of(repo),
            vec!["release".to_string(), "settings-defaults".to_string()],
        );
    }

    /// `[variant.local-packages]` is transitively closed via each
    /// in-repo package's `[package].depends`. A user listing only
    /// the top-level `release` pulls in `os`, `storewolf`, and
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
                std::path::Path::new("."),
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
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();

        // BTreeSet sort → alphabetical.
        assert_eq!(
            depends_of(repo),
            vec![
                "//packages/os/rpm".to_string(),
                "//packages/release/rpm".to_string(),
                "//packages/settings-defaults/rpm".to_string(),
                "//packages/storewolf/rpm".to_string(),
            ],
        );

        assert_eq!(
            os_packages_of(repo),
            vec![
                "os".to_string(),
                "release".to_string(),
                "settings-defaults".to_string(),
                "storewolf".to_string(),
            ],
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
    /// packages at all, the repo config still carries an empty
    /// `depends` list and an empty `os.packages` list rather than a
    /// missing field.
    #[test]
    fn included_packages_absent_leaves_config_empty_not_missing() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        assert!(depends_of(repo).is_empty());
        assert!(os_packages_of(repo).is_empty());
    }

    /// Kits are attached to the repo element under the `"kits"`
    /// source scope; the image stage carries no source scopes at
    /// all. The SDK stays under `"default"`.
    #[test]
    fn kits_appear_on_repo_source_scope_not_config() {
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
                std::path::Path::new("."),
                &project_kits,
                &pkgs(&[]),
            )
            .unwrap();

        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(repo);

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

        // The image stage also carries the `kits` scope so
        // `RPM2ImageTransform` can stage each kit into
        // `kits/<vendor>/<name>/<arch>/` and pass
        // `--external-kits-path=` to `rpm2img.sh` for the
        // application-inventory rewrite. Kit ordering matches the
        // repo stage (both derive from the same caller slice), and
        // no `default` scope is emitted here because the image stage
        // does not consume the SDK source directly.
        let image_elem = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let image_scopes = unresolved_scopes(image_elem);
        let image_kits_scope = image_scopes.get("kits").expect("image kits scope present");
        let image_kit_addrs: Vec<String> =
            image_kits_scope.iter().map(|a| a.to_string()).collect();
        assert_eq!(
            image_kit_addrs,
            vec![
                "//kits/bottlerocket/core-kit".to_string(),
                "//kits/my-vendor/extra-3-kit".to_string(),
            ]
        );
        assert!(!image_scopes.contains_key("default"));
    }

    /// Empty project-kit slice must not emit an empty `"kits"` scope on
    /// the image element either (parity with the repo element).
    #[test]
    fn no_kit_scope_emitted_on_image_when_project_has_no_kits() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &[],
                &pkgs(&[]),
            )
            .unwrap();
        let image_elem = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        assert!(!unresolved_scopes(image_elem).contains_key("kits"));
    }

    /// Regression guard: an empty project-kit slice must not emit an
    /// empty `"kits"` scope on the repo element.
    #[test]
    fn no_kit_scope_emitted_when_project_has_no_kits() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(repo);
        assert!(!source_map.contains_key("kits"));
        assert!(source_map.contains_key("default"));
    }

    /// Kernel parameters land under `os.kernel-parameters` on every
    /// stage (mirroring `VariantOptions::os`). Both stages carry
    /// them verbatim; only the repo stage bakes them into the
    /// metadata RPM, but the image stage's uniqueness digest still
    /// wants the full list (via `VariantOptions::os`).
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
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        for stage_addr in ["//variants/x/repo", "//variants/x"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(stage_addr).unwrap())
                .unwrap();
            let kp = elem
                .config
                .get("os")
                .unwrap()
                .get("kernel-parameters")
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(kp.len(), 2, "stage {stage_addr}");
        }
    }

    /// Old-twoliter silently seeds `in-place-updates`, `host-containers`,
    /// and `external-kmod-development` when the variant does not opt
    /// into a stripped-down image (`standalone-image` or `image-format
    /// = "eif"`). Losing that seed makes the core-kit's `os` package
    /// skip `bork` (its `Requires: (bork or image-feature(no-in-place-updates))`
    /// is satisfied by the metadata RPM instead), which explodes on
    /// first boot as `sundog: Failed to start generator 'bork seed'`.
    #[test]
    fn full_image_silently_seeds_in_place_updates_and_host_containers() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image]
format = "raw"

[image.features]
uefi-secure-boot = false
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &[],
                &pkgs(&[]),
            )
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let features = image.config.get("image").unwrap().get("features").unwrap();
        assert_eq!(
            features.get("in-place-updates").and_then(|v| v.as_bool()),
            Some(true),
            "in-place-updates must be silently seeded for a full image"
        );
        assert_eq!(
            features.get("host-containers").and_then(|v| v.as_bool()),
            Some(true),
            "host-containers must be silently seeded for a full image"
        );
        assert_eq!(
            features
                .get("external-kmod-development")
                .and_then(|v| v.as_bool()),
            Some(true),
            "external-kmod-development must be silently seeded for a full image"
        );
    }

    /// An explicit `false` in the variant's `[image.features]` overrides
    /// the silent seed. Mirrors old-twoliter's dev-variant convention.
    #[test]
    fn explicit_false_overrides_silent_seed() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image]
format = "raw"

[image.features]
in-place-updates = false
external-kmod-development = false
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &[],
                &pkgs(&[]),
            )
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let features = image.config.get("image").unwrap().get("features").unwrap();
        assert!(
            features.get("in-place-updates").is_none(),
            "explicit false must strip in-place-updates from the resolved set"
        );
        assert!(
            features.get("external-kmod-development").is_none(),
            "explicit false must strip external-kmod-development from the resolved set"
        );
        // `host-containers` was not touched by the user, so the silent
        // seed must survive.
        assert_eq!(
            features.get("host-containers").and_then(|v| v.as_bool()),
            Some(true),
        );
    }

    /// `standalone-image = true` swaps the seed: no `in-place-updates`,
    /// no `host-containers` (both would collide with the stripped
    /// layout).
    #[test]
    fn standalone_image_seeds_a_stripped_set() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image]
format = "raw"

[image.features]
standalone-image = true
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &[],
                &pkgs(&[]),
            )
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let features = image.config.get("image").unwrap().get("features").unwrap();
        assert_eq!(
            features.get("standalone-image").and_then(|v| v.as_bool()),
            Some(true),
        );
        assert!(features.get("in-place-updates").is_none());
        assert!(features.get("host-containers").is_none());
    }

    /// `image-format = "eif"` triggers the same stripped seed as
    /// `standalone-image = true`.
    #[test]
    fn eif_format_seeds_a_stripped_set() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image]
format = "eif"
"#,
        );
        let schema = v
            .to_edo(
                &Addr::parse("//variants").unwrap(),
                std::path::Path::new("."),
                &[],
                &pkgs(&[]),
            )
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let features = image.config.get("image").unwrap().get("features").unwrap();
        assert_eq!(
            features.get("standalone-image").and_then(|v| v.as_bool()),
            Some(true),
        );
        assert!(features.get("in-place-updates").is_none());
        assert!(features.get("host-containers").is_none());
    }

    #[test]
    fn image_features_pass_through() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[image.features]
uefi-secure-boot = true
standalone-image = true
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        let image = schema
            .transforms()
            .get(&Addr::parse("//variants/x").unwrap())
            .unwrap();
        let features = image.config.get("image").unwrap().get("features").unwrap();
        assert_eq!(
            features.get("uefi-secure-boot").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn top_level_variant_addr_matches_pre_decomp_convention() {
        // Downstream tooling addresses variants at `//variants/<name>`;
        // this must remain the finalizer's (RPM2ImageTransform)
        // address after decomposition or every `twoliter publish`
        // call breaks.
        let v = parse(
            r#"
name = "aws-k8s-1.29"
version = "0.1.0"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
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
        // `[[kit]]` list, and land on the repo element's `"kits"`
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
                std::path::Path::new("."),
                &project_kits,
                &pkgs(&[]),
            )
            .unwrap();
        let repo = schema
            .transforms()
            .get(&Addr::parse("//variants/x/repo").unwrap())
            .unwrap();
        let source_map = unresolved_scopes(repo);
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
                .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
                .unwrap();
            let repo = schema
                .transforms()
                .get(&Addr::parse("//variants/x/repo").unwrap())
                .unwrap();
            assert_eq!(
                repo.config
                    .get("image")
                    .unwrap()
                    .get("format")
                    .and_then(|v| v.as_str()),
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

    /// Guest-image addrs appear on `guest-images` for both stages
    /// (`VariantOptions::guest_images` is shared), but only the image
    /// stage stages them into `images/` at build time.
    #[test]
    fn guest_images_appear_on_every_stage() {
        let v = parse(
            r#"
name = "x"
version = "0.1.0"

[[guest-image]]
addr = "//guest-images/kata"

[[guest-image]]
addr = "//guest-images/firecracker"
"#,
        );
        let schema = v
            .to_edo(&Addr::parse("//variants").unwrap(), std::path::Path::new("."), &[], &pkgs(&[]))
            .unwrap();
        for stage_addr in ["//variants/x/repo", "//variants/x"] {
            let elem = schema
                .transforms()
                .get(&Addr::parse(stage_addr).unwrap())
                .unwrap();
            let gi: Vec<&str> = elem
                .config
                .get("guest-images")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(
                gi,
                vec!["//guest-images/kata", "//guest-images/firecracker"],
                "stage {stage_addr}",
            );
        }
    }
}
