use std::{collections::BTreeMap, path::PathBuf};

use edo::context::{Addr, ContextResult, Element, Schema, SourceMap};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct Kit {
    pub name: String,
    pub version: semver::Version,
    #[serde(default)]
    pub path: Option<PathBuf>,
    pub packages: Vec<String>,
    #[serde(default)]
    pub cargo_projects: Vec<String>,
    #[serde(default)]
    pub go_modules: Vec<String>,
}

impl Kit {
    pub fn to_edo(&self, addr: &Addr) -> ContextResult<Schema> {
        let mut sources = BTreeMap::new();
        let mut transforms = BTreeMap::new();
        let kit_addr = addr.join(&Addr::parse(&self.name)?);

        // 1. The kit source
        let src_addr = kit_addr.join(&Addr::parse("kit-src")?);
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

        // 2. The kit source wide vendoring
        if !self.cargo_projects.is_empty() {
            let cargo_addr = kit_addr.join(&Addr::parse("kit-rust")?);
            transforms.insert(
                cargo_addr.clone(),
                Element::builder()
                    .addr(cargo_addr.clone())
                    .kind("cargo-vendor")
                    .source(SourceMap::Unresolved(BTreeMap::from([(
                        "default".to_string(),
                        vec![src_addr.clone()],
                    )])))
                    .config(BTreeMap::from([(
                        "cargo-tomls".to_string(),
                        json!({
                            src_addr.last().unwrap(): &self.cargo_projects
                        }),
                    )]))
                    .build(),
            );
        }
        if !self.go_modules.is_empty() {
            let go_addr = kit_addr.join(&Addr::parse("kit-go")?);
            transforms.insert(
                go_addr.clone(),
                Element::builder()
                    .addr(go_addr.clone())
                    .kind("go-vendor")
                    .source(SourceMap::Unresolved(BTreeMap::from([(
                        "default".to_string(),
                        vec![src_addr.clone()],
                    )])))
                    .config(BTreeMap::from([(
                        "modules".to_string(),
                        json!(&self.go_modules),
                    )]))
                    .build(),
            );
        }

        // 3. The kit transform
        let t_addr = kit_addr.join(&Addr::parse("build")?);
        let sdk_addr = Addr::parse("//bottlerocket-sdk")?;
        transforms.insert(
            t_addr.clone(),
            Element::builder()
                .addr(t_addr.clone())
                .kind("kit")
                .environment(Addr::parse("//sdk-isolated")?)
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![sdk_addr],
                )])))
                .config(BTreeMap::from([
                    ("name".to_string(), json!(&self.name)),
                    ("version".to_string(), json!(self.version.to_string())),
                    (
                        "packages".to_string(),
                        json!(
                            self.packages
                                .iter()
                                .map(|x| format!("//packages/{x}/rpm"))
                                .collect::<Vec<_>>()
                        ),
                    ),
                ]))
                .build(),
        );
        // 3. The bottlerocket config to sync the version and build timestamp
        let config = BTreeMap::from([(
            "bottlerocket".to_string(),
            json!({
                "release-version": format!("v{}", self.version),
                "build-timestamp": "1778705221" // TODO: Calculate this properly
            }),
        )]);
        Ok(Schema::builder()
            .config(config)
            .source(sources)
            .transform(transforms)
            .build())
    }
}
