use std::collections::BTreeMap;

use edo::context::{Addr, ContextResult, Element, Requirement, Schema, SourceMap};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct Twoliter {
    pub registry: String,
    pub sdk: VersionReq,
}

impl Twoliter {
    pub fn to_edo(&self) -> ContextResult<Schema> {
        let mut vendors = BTreeMap::new();
        let mut requires = BTreeMap::new();
        let mut environments = BTreeMap::new();
        // 1. Add the ECR Vendor
        let vendor_addr = Addr::parse("//oci-vendor")?;
        vendors.insert(
            vendor_addr.clone(),
            Element::builder()
                .addr(vendor_addr.clone())
                .kind("image")
                .config(BTreeMap::from([("uri".to_string(), json!(&self.registry))]))
                .build(),
        );

        // 2. Add the sdk requirement
        let image_addr = Addr::parse("//bottlerocket-sdk")?;
        requires.insert(
            image_addr.clone(),
            Requirement {
                kind: "image".to_string(),
                at: self.sdk.clone(),
            },
        );

        // 3. Add the Isolated sdk environment at //sdk-isolated
        let sdk_isolated = Addr::parse("//sdk-isolated")?;
        environments.insert(
            sdk_isolated.clone(),
            Element::builder()
                .addr(sdk_isolated.clone())
                .kind("container")
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![image_addr.clone()],
                )])))
                .config(BTreeMap::from([("user".to_string(), json!("root"))]))
                .build(),
        );

        // 4. Add the Network available sdk environment at //sdk-open
        let sdk_open = Addr::parse("//sdk-open")?;
        environments.insert(
            sdk_open.clone(),
            Element::builder()
                .addr(sdk_isolated.clone())
                .kind("container")
                .source(SourceMap::Unresolved(BTreeMap::from([(
                    "default".to_string(),
                    vec![image_addr.clone()],
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
        Ok(Schema::builder()
            .vendor(vendors)
            .requires(requires)
            .environment(environments)
            .build())
    }
}
