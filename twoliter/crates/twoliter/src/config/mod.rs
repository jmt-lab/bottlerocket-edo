use std::{
    collections::BTreeMap,
    fs::read_dir,
    path::{Path, PathBuf},
};

use edo::context::{Addr, Project, Schema};
use serde::{Deserialize, Serialize};

mod kit;
mod overrides;
mod package;
pub mod twoliter;
pub mod variant;

use kit::Kit;
use overrides::TwoliterOverride;
use package::Package;
use snafu::{OptionExt, ResultExt};
use twoliter::Twoliter;
use variant::Variant;

/// Default filename for the auto-discovered project-root override.
///
/// Additional overlay files can be passed via the `--override <path>`
/// CLI flag and are merged after this file.
const TWOLITER_OVERRIDE_FILE: &str = "Twoliter.override.toml";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "schema-version")]
pub enum Config {
    #[serde(rename = "3")]
    V1(ConfigType),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigType {
    Project(Twoliter),
    Package(Package),
    Kit(Kit),
    Variant(Variant),
}

enum Expect {
    Package,
    Kit,
    Variant,
}

impl Config {
    pub async fn load<P: AsRef<Path>>(
        path: P,
        overrides: &[PathBuf],
        inline_vendor_overrides: &[(String, String)],
    ) -> Result<Project, error::Error> {
        let path = path.as_ref();
        let mut project = Project::new(path);
        // 1. If this is a twoliter project we should have a project toml
        let (project_schema, base_project) =
            Self::twoliter_toml(path, overrides, inline_vendor_overrides)?
                .context(error::NotProjectSnafu { path })?;
        project.schema_mut().merge(&project_schema);

        // Resolve the project-level `[[kit]]` list to
        // `//kits/<vendor>/<name>` addresses. Every variant implicitly
        // receives this full list — variants no longer declare kits
        // individually. If this project loaded from `edo.toml` only
        // (no `Twoliter.toml`), `base_project` is `None` and the list
        // is empty.
        let project_kits: Vec<Addr> = base_project
            .as_ref()
            .map(|p| {
                p.kit
                    .iter()
                    .map(|k| Addr::parse(&format!("//kits/{}/{}", k.vendor, k.name)))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        // The in-repo package dependency graph, populated during the
        // packages walk below. Keyed by the parsed `[package].name`
        // field (not dirname — see `Package::to_edo`, which builds
        // its address from `name`); each value is the package's
        // `[package].depends` list verbatim.
        //
        // The variants walk consults this graph to transitively
        // close each `[variant.local-packages].packages` list —
        // users list only the top-level packages they care about
        // and the closure pulls in every reachable in-repo dep
        // (matches old-twoliter's Cargo-graph-based
        // `PACKAGE_DEPENDENCIES` behavior).
        let mut project_packages: BTreeMap<String, Vec<String>> = BTreeMap::new();

        // 2. Check if we have a kits/ directory defining we have kits in-project
        let (kit_dir, kit_addr) = (path.join("kits"), Addr::parse("//kits")?);
        if kit_dir.try_exists().context(error::IoSnafu)? {
            Self::walk(
                &Expect::Kit,
                &kit_addr,
                &kit_dir,
                &project_kits,
                &mut project_packages,
                project.schema_mut(),
            )?;
        }
        let (pkg_dir, pkg_addr) = (path.join("packages"), Addr::parse("//packages")?);
        if pkg_dir.try_exists().context(error::IoSnafu)? {
            Self::walk(
                &Expect::Package,
                &pkg_addr,
                &pkg_dir,
                &project_kits,
                &mut project_packages,
                project.schema_mut(),
            )?;
        }
        // 3. `variants/<name>/variant.toml` walked at `//variants`.
        //    Each variant emits four elements
        //    (`//variants/<name>/{rootfs,disk,grub}` + the finalizer
        //    at `//variants/<name>`) via `Variant::to_edo`, receiving
        //    the project-wide kit list resolved above and the set of
        //    in-repo package names collected during the packages walk.
        let (variant_dir, variant_addr) = (path.join("variants"), Addr::parse("//variants")?);
        if variant_dir.try_exists().context(error::IoSnafu)? {
            Self::walk(
                &Expect::Variant,
                &variant_addr,
                &variant_dir,
                &project_kits,
                &mut project_packages,
                project.schema_mut(),
            )?;
        }
        Ok(project)
    }

    fn walk<P: AsRef<Path>>(
        expect: &Expect,
        namespace: &Addr,
        path: P,
        project_kits: &[Addr],
        project_packages: &mut BTreeMap<String, Vec<String>>,
        schema: &mut Schema,
    ) -> Result<(), error::Error> {
        let read = read_dir(path).context(error::IoSnafu)?;
        for entry in read {
            let entry = entry.context(error::IoSnafu)?;
            let path = entry.path();
            let dirname = path.file_name().and_then(|x| x.to_str()).unwrap();
            if path.is_dir() {
                // Check if there is a definition here, if so merge it up
                let child_schema = match expect {
                    Expect::Kit => Self::kit_toml(namespace, &path)?,
                    Expect::Package => {
                        // The packages walk also populates
                        // `project_packages` with each parsed
                        // package's `name` field so the later variants
                        // walk can distinguish in-repo names from
                        // kit-supplied ones. `project_kits` flows in
                        // so `Package::to_edo` can attach the
                        // project-wide kit list under the RPM
                        // element's `"kit"` source scope (parallel
                        // to `Variant::to_edo`'s rootfs `"kits"`
                        // scope). Without this, kit-supplied
                        // BuildRequires (e.g. `bottlerocket-glibc-devel`
                        // from the core kit) never land in the
                        // package's local dnf repo.
                        Self::package_toml(namespace, &path, project_kits, project_packages)?
                    }
                    Expect::Variant => {
                        Self::variant_toml(namespace, &path, project_kits, project_packages)?
                    }
                };
                if let Some(child_schema) = child_schema.as_ref() {
                    schema.merge(child_schema);
                }

                let child_ns = namespace.join(&Addr::parse(dirname)?);
                Self::walk(
                    expect,
                    &child_ns,
                    &path,
                    project_kits,
                    project_packages,
                    schema,
                )?;
            }
        }
        Ok(())
    }

    fn twoliter_toml<P: AsRef<Path>>(
        path: P,
        override_paths: &[PathBuf],
        inline_vendor_overrides: &[(String, String)],
    ) -> Result<Option<(Schema, Option<Twoliter>)>, error::Error> {
        let twoliter_toml = path.as_ref().join("twoliter.toml");
        if twoliter_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&twoliter_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu {
                path: twoliter_toml.clone(),
            })?;
            // Keep the parsed `Twoliter` around so overrides can validate
            // references against the base project definition, and so
            // the caller can pass the project-level `[[kit]]` list down
            // to the variant walk.
            let base_project = match config {
                Config::V1(element) => match element {
                    ConfigType::Project(project) => Ok(project),
                    ConfigType::Package(_) => error::ExpectedSnafu {
                        expected: "project",
                        actual: "package",
                    }
                    .fail(),
                    ConfigType::Kit(_) => error::ExpectedSnafu {
                        expected: "project",
                        actual: "kit",
                    }
                    .fail(),
                    ConfigType::Variant(_) => error::ExpectedSnafu {
                        expected: "project",
                        actual: "variant",
                    }
                    .fail(),
                },
            }?;
            let mut schema = base_project.to_edo()?;

            // Apply overlay overrides (`Twoliter.override.toml`, `--override
            // <path>`, `--vendor <name>=<registry>`) before merging the
            // adjacent `edo.toml`. This ordering lets `edo.toml` remain
            // the escape hatch of last resort even when overrides are in
            // effect.
            let merged_overrides =
                Self::collect_overrides(path.as_ref(), override_paths, inline_vendor_overrides)?;
            if !merged_overrides.is_empty() {
                merged_overrides.apply(&base_project, &mut schema)?;
            }

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            return Ok(Some((schema, Some(base_project))));
        }
        // If there is a edo.toml in the path return this instead
        if let Some(mut schema) = Self::edo_toml(path)? {
            schema.propagate();
            Ok(Some((schema, None)))
        } else {
            Ok(None)
        }
    }

    /// Compose the effective override set: auto-discovered
    /// `Twoliter.override.toml`, then each explicit `--override <path>`
    /// in order, then the inline `--vendor` pairs. Later entries win.
    fn collect_overrides(
        project_dir: &Path,
        override_paths: &[PathBuf],
        inline_vendor_overrides: &[(String, String)],
    ) -> Result<TwoliterOverride, error::Error> {
        let mut merged = TwoliterOverride::default();
        let auto = project_dir.join(TWOLITER_OVERRIDE_FILE);
        if auto.try_exists().context(error::IoSnafu)? {
            merged.merge(Self::read_override_file(&auto)?);
        }
        for p in override_paths {
            if !p.try_exists().context(error::IoSnafu)? {
                return error::OverrideFileNotFoundSnafu { path: p.clone() }.fail();
            }
            merged.merge(Self::read_override_file(p)?);
        }
        if !inline_vendor_overrides.is_empty() {
            merged.merge(TwoliterOverride::from_cli(inline_vendor_overrides));
        }
        Ok(merged)
    }

    fn read_override_file(path: &Path) -> Result<TwoliterOverride, error::Error> {
        let body = std::fs::read_to_string(path).context(error::IoSnafu)?;
        toml::from_str(&body).context(error::DeserializeSnafu {
            path: path.to_path_buf(),
        })
    }

    fn kit_toml<P: AsRef<Path>>(namespace: &Addr, path: P) -> Result<Option<Schema>, error::Error> {
        let kit_toml = path.as_ref().join("kit.toml");
        if kit_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&kit_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu {
                path: kit_toml.clone(),
            })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Kit(kit) => Ok(kit.to_edo(namespace)?),
                    ConfigType::Package(_) => error::ExpectedSnafu {
                        expected: "kit",
                        actual: "package",
                    }
                    .fail(),
                    ConfigType::Project(_) => error::ExpectedSnafu {
                        expected: "kit",
                        actual: "project",
                    }
                    .fail(),
                    ConfigType::Variant(_) => error::ExpectedSnafu {
                        expected: "kit",
                        actual: "variant",
                    }
                    .fail(),
                },
            }?;

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            schema.with_namespace(namespace);
            return Ok(Some(schema));
        }
        // If there is a edo.toml in the path return this instead
        if let Some(mut schema) = Self::edo_toml(path)? {
            schema.propagate();
            schema.with_namespace(namespace);
            Ok(Some(schema))
        } else {
            Ok(None)
        }
    }

    fn package_toml<P: AsRef<Path>>(
        namespace: &Addr,
        path: P,
        project_kits: &[Addr],
        project_packages: &mut BTreeMap<String, Vec<String>>,
    ) -> Result<Option<Schema>, error::Error> {
        let package_toml = path.as_ref().join("package.toml");
        if package_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&package_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu {
                path: package_toml.clone(),
            })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Package(pkg) => {
                        // Record the parsed name (not the dirname —
                        // `Package::to_edo` builds its address from
                        // `Package.name`) alongside its declared
                        // `[package].depends` list. Duplicate names
                        // overwrite (last one wins) and are rare in
                        // practice; a well-formed project has one
                        // package.toml per name.
                        project_packages.insert(pkg.name.clone(), pkg.depends.clone());
                        Ok(pkg.to_edo(namespace, project_kits)?)
                    }
                    ConfigType::Kit(_) => error::ExpectedSnafu {
                        expected: "package",
                        actual: "kit",
                    }
                    .fail(),
                    ConfigType::Project(_) => error::ExpectedSnafu {
                        expected: "package",
                        actual: "kit",
                    }
                    .fail(),
                    ConfigType::Variant(_) => error::ExpectedSnafu {
                        expected: "package",
                        actual: "variant",
                    }
                    .fail(),
                },
            }?;

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            schema.with_namespace(namespace);
            return Ok(Some(schema));
        }
        // If there is a edo.toml in the path return this instead
        if let Some(mut schema) = Self::edo_toml(path)? {
            schema.propagate();
            schema.with_namespace(namespace);
            Ok(Some(schema))
        } else {
            Ok(None)
        }
    }

    fn variant_toml<P: AsRef<Path>>(
        namespace: &Addr,
        path: P,
        project_kits: &[Addr],
        project_packages: &BTreeMap<String, Vec<String>>,
    ) -> Result<Option<Schema>, error::Error> {
        let variant_toml = path.as_ref().join("variant.toml");
        if variant_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&variant_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu {
                path: variant_toml.clone(),
            })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Variant(v) => {
                        // `path` is `variants/<name>/`; `to_edo` uses this to
                        // locate `template.ovf` (old-twoliter convention:
                        // `Makefile.toml:173`).
                        Ok(v.to_edo(namespace, path.as_ref(), project_kits, project_packages)?)
                    }
                    ConfigType::Kit(_) => error::ExpectedSnafu {
                        expected: "variant",
                        actual: "kit",
                    }
                    .fail(),
                    ConfigType::Package(_) => error::ExpectedSnafu {
                        expected: "variant",
                        actual: "package",
                    }
                    .fail(),
                    ConfigType::Project(_) => error::ExpectedSnafu {
                        expected: "variant",
                        actual: "project",
                    }
                    .fail(),
                },
            }?;

            // If there is an edo.toml alongside, merge it in.
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            schema.with_namespace(namespace);
            return Ok(Some(schema));
        }
        if let Some(mut schema) = Self::edo_toml(path)? {
            schema.propagate();
            schema.with_namespace(namespace);
            Ok(Some(schema))
        } else {
            Ok(None)
        }
    }

    fn edo_toml<P: AsRef<Path>>(path: P) -> Result<Option<Schema>, error::Error> {
        let edo_toml = path.as_ref().join("edo.toml");
        if edo_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&edo_toml).context(error::IoSnafu)?;
            return Ok(Some(toml::from_str(&toml_schema).context(
                error::DeserializeSnafu {
                    path: edo_toml.clone(),
                },
            )?));
        }
        Ok(None)
    }
}

pub mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(display("failed to deserialize {path:?}: {source}"))]
        Deserialize {
            path: PathBuf,
            source: toml::de::Error,
        },
        #[snafu(display(
            "expected a {expected} configuration but received a {actual} configuration."
        ))]
        Expected { expected: String, actual: String },
        #[snafu(display("io error occured loading configuration: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display(
            "not a twoliter project, no root level twoliter.toml or edo.toml found at {path:?}"
        ))]
        NotProject { path: PathBuf },
        #[snafu(display("override file not found: {path:?}"))]
        OverrideFileNotFound { path: PathBuf },
    }
}
