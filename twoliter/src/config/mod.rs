use std::{fs::read_dir, path::Path};

use edo::context::{Addr, Project, Schema};
use serde::{Serialize, Deserialize};


mod package;
mod kit;
mod twoliter;

use package::Package;
use twoliter::Twoliter;
use kit::Kit;
use snafu::{OptionExt, ResultExt};

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
}

enum Expect {
    Package,
    Kit
}

impl Config {
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Project, error::Error> {
        let path = path.as_ref();
        let mut project = Project::new(path);
        // 1. If this is a twoliter project we should have a project toml
        let project_schema = Self::twoliter_toml(path)?.context(error::NotProjectSnafu { path })?;
        project.schema_mut().merge(&project_schema);
        // 2. Check if we have a kits/ directory defining we have kits in-project
        let (kit_dir, kit_addr) = (path.join("kits"), Addr::parse("//kits")?);
        if kit_dir.try_exists().context(error::IoSnafu)? {
            Self::walk(&Expect::Kit, &kit_addr, &kit_dir, project.schema_mut())?;
        }
        let (pkg_dir, pkg_addr) = (path.join("packages"), Addr::parse("//packages")?);
        if pkg_dir.try_exists().context(error::IoSnafu)? {
            Self::walk(&Expect::Package, &pkg_addr, &pkg_dir, project.schema_mut())?;
        }
        Ok(project)
    }

    fn walk<P: AsRef<Path>>(expect: &Expect, namespace: &Addr, path: P, schema: &mut Schema) -> Result<(), error::Error> {
        let read = read_dir(path).context(error::IoSnafu)?;
        for entry in read {
            let entry = entry.context(error::IoSnafu)?;
            let path = entry.path();
            let dirname = path.file_name().and_then(|x| x.to_str()).unwrap();
            if path.is_dir() {
                // Check if there is a definition here, if so merge it up
                let child_schema = match expect {
                    Expect::Kit => Self::kit_toml(namespace, &path)?,
                    Expect::Package => Self::package_toml(namespace, &path)?
                };
                if let Some(child_schema) = child_schema.as_ref() {
                    schema.merge(child_schema);
                }

                let child_ns = namespace.join(&Addr::parse(dirname)?);
                Self::walk(expect, &child_ns, &path, schema)?;
            }
        }
        Ok(())
    }

    fn twoliter_toml<P: AsRef<Path>>(path: P) -> Result<Option<Schema>, error::Error> {
        let twoliter_toml = path.as_ref().join("twoliter.toml");
        if twoliter_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&twoliter_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu { path: twoliter_toml.clone() })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Project(project) => Ok(project.to_edo()?),
                    ConfigType::Package(_) => error::ExpectedSnafu { expected: "project", actual: "package" }.fail(),
                    ConfigType::Kit(_) => error::ExpectedSnafu { expected: "project", actual: "kit" }.fail(),
                }
            }?;

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            return Ok(Some(schema))
        }
        // If there is a edo.toml in the path return this instead
        if let Some(mut schema) = Self::edo_toml(path)? {
            schema.propagate();
            Ok(Some(schema))
        } else {
            Ok(None)
        }
    }

    fn kit_toml<P: AsRef<Path>>(namespace: &Addr, path: P) -> Result<Option<Schema>, error::Error> {
        let kit_toml = path.as_ref().join("kit.toml");
        if kit_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&kit_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu { path: kit_toml.clone() })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Kit(kit) => Ok(kit.to_edo(namespace)?),
                    ConfigType::Package(_) => error::ExpectedSnafu { expected: "kit", actual: "package" }.fail(),
                    ConfigType::Project(_) => error::ExpectedSnafu { expected: "kit", actual: "project" }.fail(),
                }
            }?;

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            schema.with_namespace(namespace);
            return Ok(Some(schema))
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

    fn package_toml<P: AsRef<Path>>(namespace: &Addr, path: P) -> Result<Option<Schema>, error::Error> {
        let package_toml = path.as_ref().join("package.toml");
        if package_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&package_toml).context(error::IoSnafu)?;
            let config: Config = toml::from_str(&toml_schema).context(error::DeserializeSnafu { path: package_toml.clone() })?;
            let mut schema = match config {
                Config::V1(element) => match element {
                    ConfigType::Package(pkg) => Ok(pkg.to_edo(namespace)?),
                    ConfigType::Kit(_) => error::ExpectedSnafu { expected: "package", actual: "kit" }.fail(),
                    ConfigType::Project(_) => error::ExpectedSnafu { expected: "package", actual: "kit" }.fail(),
                }
            }?;

            // If there is a edo.toml in the path as well merge it in
            if let Some(edo_schema) = Self::edo_toml(path)? {
                schema.merge(&edo_schema);
            }
            schema.propagate();
            schema.with_namespace(namespace);
            return Ok(Some(schema))
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

    fn edo_toml<P: AsRef<Path>>(path: P) -> Result<Option<Schema>, error::Error> {
        let edo_toml = path.as_ref().join("edo.toml");
        if edo_toml.try_exists().context(error::IoSnafu)? {
            let toml_schema = std::fs::read_to_string(&edo_toml).context(error::IoSnafu)?;
            return Ok(Some(toml::from_str(&toml_schema).context(error::DeserializeSnafu { path: edo_toml.clone() })?))
        }
        Ok(None)
    }
}

pub mod error {
    use std::path::PathBuf;
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(display("failed to deserialize {path:?}: {source}"))]
        Deserialize { path: PathBuf, source: toml::de::Error },
        #[snafu(display("expected a {expected} configuration but received a {actual} configuration."))]
        Expected { expected: String, actual: String },
        #[snafu(display("io error occured loading configuration: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("not a twoliter project, no root level twoliter.toml or edo.toml found at {path:?}"))]
        NotProject { path: PathBuf },
    }
}
