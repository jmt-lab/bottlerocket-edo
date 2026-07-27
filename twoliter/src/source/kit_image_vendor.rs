//! `kit-image` vendor: exposes a Bottlerocket OCI registry as an edo
//! [`Vendor`](edo::source::Vendor). Mirrors edo-core's `ImageVendor` for
//! tag/version discovery and merkle-digest resolution, but overrides
//! `get_dependencies` to decode the kit's `dev.bottlerocket.kit.v3.metadata`
//! label and surface its transitive SDK + kit requirements to resolvo.

use async_trait::async_trait;
use edo::context::{Addr, Context, Element, FromElement};
use edo::source::{SourceResult, VendorImpl};
use ocilot::index::Index;
use ocilot::registry::Registry;
use ocilot::repository::Repository;
use ocilot::uri::{Reference, RegistryUri, Uri};
use semver::{Version, VersionReq};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{ResultExt, ensure};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use super::kit_image::{KIT_METADATA_LABEL, KitMetadata};

#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct KitImageVendorOptions {
    /// Base registry URI (e.g. `public.ecr.aws/bottlerocket`).
    uri: String,
    /// Optional friendly name for diagnostics; unused today but reserved
    /// so `[vendor.<name>]` can carry the twoliter vendor label into
    /// diagnostics without a downstream break.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

/// A vendor that serves Bottlerocket kits from an OCI registry.
pub struct KitImageVendor {
    registry: Registry,
}

// `Registry` from `ocilot` is `!Send`/`!Sync` conservatively but its
// interior state is only touched behind async locks; matches the manual
// impls edo-core uses for `ImageVendor`.
unsafe impl Send for KitImageVendor {}
unsafe impl Sync for KitImageVendor {}

#[async_trait]
impl FromElement for KitImageVendor {
    type Error = error::Error;

    async fn new(element: &Element, _ctx: &Context) -> Result<Self, error::Error> {
        let options: KitImageVendorOptions = element.get()?;
        let registry_uri = RegistryUri::from_str(&options.uri).context(error::OciSnafu)?;
        Ok(Self {
            registry: Registry::new(&registry_uri)
                .await
                .context(error::OciSnafu)?,
        })
    }
}

#[async_trait]
impl VendorImpl for KitImageVendor {
    async fn get_options(&self, name: &str) -> SourceResult<HashSet<Version>> {
        let mut versions = HashSet::new();
        let repo = Repository::new(&self.registry, name);
        for tag in repo.tags().await.context(error::OciSnafu)? {
            let stag = tag.strip_prefix('v').unwrap_or(tag.as_str());
            if let Ok(version) = Version::parse(stag) {
                versions.insert(version);
            }
        }
        Ok(versions)
    }

    async fn resolve(&self, name: &str, version: &Version) -> SourceResult<Element> {
        let (uri, index) = self.fetch_index(name, version).await?;
        let mut hasher = Sha256::new();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let digest = base16::encode_lower(hasher.finalize().as_slice());
        Ok(Element::builder()
            .addr(Addr::parse(name)?)
            .kind("kit-image")
            .config([
                ("uri".to_string(), json!(uri.to_string())),
                ("ref".to_string(), json!(digest)),
            ])
            .build())
    }

    async fn get_dependencies(
        &self,
        name: &str,
        version: &Version,
    ) -> SourceResult<Option<HashMap<String, VersionReq>>> {
        // Fetch the manifest list and the first image's config blob.
        // Any missing / malformed label is treated as "no known deps"
        // (Ok(None)) to match edo-core's behaviour for non-edo artifacts,
        // but is emitted as a warning so misconfigured images are visible.
        let (uri, index) = match self.fetch_index(name, version).await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "failed to fetch manifest index; treating as no deps",
                );
                return Ok(None);
            }
        };
        let image = match index
            .fetch_image(&uri, None)
            .await
            .context(error::OciSnafu)?
        {
            Some(image) => image,
            None => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    "kit image has no platform-selectable manifest",
                );
                return Ok(None);
            }
        };
        let config = image.fetch_config(&uri).await.context(error::OciSnafu)?;
        let label = match config.config.labels.get(KIT_METADATA_LABEL) {
            Some(label) => label,
            None => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    label = KIT_METADATA_LABEL,
                    "kit image is missing metadata label; treating as no deps",
                );
                return Ok(None);
            }
        };
        let metadata = match KitMetadata::from_label(label) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    subsystem = "vendor",
                    component = "kit-image",
                    %name,
                    %version,
                    error = %err,
                    "kit metadata label failed to decode; treating as no deps",
                );
                return Ok(None);
            }
        };

        // Every referenced kit + the SDK is pinned to its exact version.
        let mut deps: HashMap<String, VersionReq> = HashMap::new();
        let sdk_req = VersionReq::parse(&format!("={}", metadata.sdk.version))
            .expect("`=<semver>` is always a valid VersionReq");
        deps.insert(metadata.sdk.name.clone(), sdk_req);
        for kit in metadata.kit.iter() {
            let req = VersionReq::parse(&format!("={}", kit.version))
                .expect("`=<semver>` is always a valid VersionReq");
            deps.insert(kit.name.clone(), req);
        }
        Ok(Some(deps))
    }
}

impl KitImageVendor {
    /// Fetch the manifest index for `<name>@<version>`. Tries the bare
    /// version tag first and falls back to a `v`-prefixed tag, matching
    /// edo-core's `ImageVendor::resolve` behaviour.
    async fn fetch_index(&self, name: &str, version: &Version) -> SourceResult<(Uri, Index)> {
        let mut uri = Uri::builder()
            .registry(self.registry.clone())
            .repository(name)
            .reference(Reference::Tag(version.to_string()))
            .build();

        let mut index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        if index.is_none() {
            uri = Uri::builder()
                .registry(self.registry.clone())
                .repository(name)
                .reference(Reference::Tag(format!("v{version}")))
                .build();
            index = Index::fetch(&uri).await.context(error::OciSnafu).ok();
        }
        ensure!(
            index.is_some(),
            error::VendedSnafu {
                name: name.to_string(),
                version: version.clone(),
            }
        );
        Ok((uri, index.unwrap()))
    }
}

pub mod error {
    use semver::Version;
    use snafu::Snafu;

    use edo::{
        context::{Addr, ContextError},
        source::SourceError,
    };

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("invalid kit-image vendor at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("io error occured interacting with oci registry: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display("failed to interact with oci registry: {source}"))]
        Oci { source: ocilot::error::Error },
        #[snafu(display("could not find a kit image matching {name}@{version}"))]
        Vended { name: String, version: Version },
    }

    impl From<Error> for SourceError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
