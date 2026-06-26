use std::{collections::BTreeMap, path::PathBuf};

use edo::context::{Addr, ContextError, Element, Schema, SourceMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub source: Vec<Source>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case", untagged, deny_unknown_fields)]
pub enum Source {
    #[serde(rename_all = "kebab-case")]
    Url {
        url: String,
        #[serde(rename = "ref")]
        digest: String,
        out: String,
        #[serde(default)]
        go_modules: Vec<String>,
        #[serde(default)]
        cargo_projects: Vec<PathBuf>,
    },
    #[serde(rename_all = "kebab-case")]
    Local {
        path: PathBuf,
        out: String,
        #[serde(default)]
        go_modules: Vec<String>,
        #[serde(default)]
        cargo_projects: Vec<PathBuf>,
    },
    #[serde(rename_all = "kebab-case")]
    Addressed {
        addr: Addr,
        #[serde(default)]
        vendored: Vec<Addr>,
    },
}

impl Package {
    pub fn to_edo(&self, addr: &Addr) -> Result<Schema, ContextError> {
        let mut sources = BTreeMap::new();
        let mut transforms = BTreeMap::new();
        let pkg_addr = addr.join(&Addr::parse(&self.name)?);
        let sdk = Addr::parse("//sdk-isolated")?;
        let sdk_open = Addr::parse("//sdk-open")?;

        // 1. The local source path and directory
        let src_addr = pkg_addr.join(&Addr::parse("package-src")?);
        sources.insert(
            src_addr.clone(),
            Element::builder()
                .addr(src_addr.clone())
                .kind("local")
                .config(BTreeMap::from([
                    ("path".to_string(), json!(&self.path)),
                    ("out".to_string(), json!(".")),
                ]))
                .build(),
        );

        // 2. Any other defined sources. Pre-size with known upper bounds.
        let mut local_sources = Vec::with_capacity(self.source.len() + 1);
        local_sources.push(src_addr.clone());
        let mut archives = Vec::with_capacity(self.source.len());
        let mut bundles: BTreeMap<Addr, String> = BTreeMap::new();
        let mut vendorings: Vec<Addr> = Vec::new();

        for (index, source) in self.source.iter().enumerate() {
            let address = pkg_addr.join(&Addr::parse(&format!("src-{index}"))?);

            // Each branch handles its own insertions; Addressed has no vendor
            // work to do, so we `continue` and skip the go/cargo loops below.
            let (out, go_modules, cargo_projects): (&str, &[String], &[PathBuf]) = match source {
                Source::Url { url, digest, out, go_modules, cargo_projects } => {
                    let element = Element::builder()
                        .addr(address.clone())
                        .kind("remote")
                        .config(BTreeMap::from([
                            ("url".to_string(), json!(url)),
                            ("ref".to_string(), json!(digest)),
                            ("out".to_string(), json!(out)),
                        ]))
                        .build();
                    archives.push(address.clone());
                    sources.insert(address.clone(), element);
                    (out.as_str(), go_modules.as_slice(), cargo_projects.as_slice())
                }
                Source::Local { path, out, go_modules, cargo_projects } => {
                    let element = Element::builder()
                        .addr(address.clone())
                        .kind("local")
                        .config(BTreeMap::from([
                            ("path".to_string(), json!(path)),
                            ("out".to_string(), json!(out)),
                        ]))
                        .build();
                    local_sources.push(address.clone());
                    sources.insert(address.clone(), element);
                    (out.as_str(), go_modules.as_slice(), cargo_projects.as_slice())
                }
                Source::Addressed { addr, vendored } => {
                    local_sources.push(addr.clone());
                    vendorings.extend(vendored.iter().cloned());
                    continue;
                }
            };

            // If there are go_modules we need to add a go_vendor transform on this source.
            // The transform must run against the upstream source (`address`) where the
            // go.mod lives — not `src_addr`, which is the package directory containing
            // the spec file but no Go code.
            for (go_index, module) in go_modules.iter().enumerate() {
                let (module, output): (&str, &str) = match module.split_once('@') {
                    Some((l, r)) => (l, r),
                    None => (module.as_str(), out),
                };
                let bundle_addr = pkg_addr
                    .join(&Addr::parse(&format!("src-{index}-go-vendored-{go_index}"))?);
                transforms.insert(
                    bundle_addr.clone(),
                    Element::builder()
                        .addr(bundle_addr.clone())
                        .kind("go-vendor")
                        .environment(sdk_open.clone())
                        .source(SourceMap::Unresolved(BTreeMap::from([(
                            "default".to_string(),
                            vec![address.clone()],
                        )])))
                        .config(BTreeMap::from([
                            ("modules".to_string(), json!([module])),
                        ]))
                        .build(),
                );
                bundles.insert(bundle_addr, format!("bundled-{output}"));
            }

            // If there are cargo projects defined we add a cargo-vendor transform.
            // Same reasoning as go-vendor: vendor the upstream source, not src_addr.
            if !cargo_projects.is_empty() {
                let bundle_addr =
                    pkg_addr.join(&Addr::parse(&format!("src-{index}-cargo-vendored"))?);
                transforms.insert(
                    bundle_addr.clone(),
                    Element::builder()
                        .addr(bundle_addr.clone())
                        .kind("cargo-vendor")
                        .environment(sdk_open.clone())
                        .source(SourceMap::Unresolved(BTreeMap::from([(
                            "default".to_string(),
                            vec![address.clone()],
                        )])))
                        .config(BTreeMap::from([
                            ("cargo_tomls".to_string(), json!(cargo_projects)),
                        ]))
                        .build(),
                );
                bundles.insert(bundle_addr, format!("bundled-{out}"));
            }
        }

        // 3. Now we can create the rpm build transform
        let rpm_addr = pkg_addr.join(&Addr::parse("rpm")?);
        let mut depends = Vec::with_capacity(self.depends.len());
        for item in self.depends.iter() {
            depends.push(format!("//packages/{item}/rpm"));
        }
        transforms.insert(
            rpm_addr.clone(),
            Element::builder()
                .addr(rpm_addr.clone())
                .kind("rpm")
                .environment(sdk.clone())
                .source(SourceMap::Unresolved(BTreeMap::from([
                    ("sources".to_string(), local_sources),
                    ("archives".to_string(), archives),
                ])))
                .config(BTreeMap::from([
                    ("package".to_string(), Value::String(self.name.clone())),
                    ("depends".to_string(), json!(depends)),
                    ("vendored".to_string(), json!(vendorings)),
                    ("bundled".to_string(), json!(bundles)),
                ]))
                .build(),
        );

        Ok(Schema::builder()
            .source(sources)
            .transform(transforms)
            .build())
    }
}
