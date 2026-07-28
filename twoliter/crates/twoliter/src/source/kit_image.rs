//! `kit-image` source: a thin wrapper around edo-core's `ImageSource`
//! that additionally verifies a Bottlerocket kit metadata label
//! (`dev.bottlerocket.kit.v3`) is present on the fetched image
//! config and decodes it into a [`KitMetadata`] struct.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use edo::context::{Context, Element, FromElement, Log};
use edo::record;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};
use ocilot::index::Index;
use ocilot::models::Platform;
use ocilot::progress::SharedProgress;
use ocilot::uri::Uri;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt, ensure};
use std::collections::BTreeSet;

/// The OCI image-config label that carries the base64-encoded
/// [`KitMetadata`] blob written by the `kit` transform.
///
/// This must match the label emitted by `cmd::publish` and the label
/// used on all published Bottlerocket kit images.
pub const KIT_METADATA_LABEL: &str = "dev.bottlerocket.kit.v3";

/// A reference to another kit or the SDK, as recorded in [`KitMetadata`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct KitRef {
    pub name: String,
    pub vendor: String,
    pub version: Version,
}

/// The subset of the Bottlerocket kit metadata document the source and
/// vendor consume.
///
/// The full schema is documented at `doc/transforms/kit-metadata.md`.
/// Additional fields on the on-disk document are ignored so the source
/// stays forward-compatible with additions that don't affect resolution.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct KitMetadata {
    pub name: String,
    pub version: Version,
    /// The SDK the kit was built against.
    pub sdk: KitRef,
    /// Kits this kit composes on top of.
    #[serde(default)]
    pub kit: Vec<KitRef>,
}

impl KitMetadata {
    /// Decode a base64-encoded metadata label into a `KitMetadata`.
    pub fn from_label(label: &str) -> Result<Self, error::Error> {
        let bytes = BASE64
            .decode(label.trim())
            .context(error::MetadataBase64Snafu)?;
        serde_json::from_slice(&bytes).context(error::MetadataDecodeSnafu)
    }

    /// Encode this metadata as a base64 label suitable for the
    /// `dev.bottlerocket.kit.v3` OCI image-config label.
    pub fn to_label(&self) -> Result<String, error::Error> {
        let bytes = serde_json::to_vec(self).context(error::MetadataEncodeSnafu)?;
        Ok(BASE64.encode(bytes))
    }
}

/// Configuration for the `kit-image` source.
///
/// Mirrors edo-core's `ImageSource` (`uri`, `ref` digest, optional
/// `platform`) with a few extra bookkeeping fields that identify the
/// source as a kit image so error messages can be more precise. These
/// extra fields are optional so the source can be constructed from a
/// resolver-produced element (which only carries `uri` + `ref`).
#[derive(Deserialize, Debug, Clone)]
pub struct KitImageSource {
    /// The full image URI (e.g. `public.ecr.aws/bottlerocket/core-kit:v1.42.0`).
    uri: String,
    /// The image's expected merkle digest (matches edo-core's `ImageSource`).
    #[serde(rename = "ref")]
    digest: String,
    /// Optional target platform. Omitting picks the host platform.
    #[serde(default)]
    platform: Option<Platform>,
    /// Optional vendor name (for diagnostics).
    #[serde(default)]
    #[allow(dead_code)]
    vendor: Option<String>,
    /// Optional kit name (for diagnostics).
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    /// Optional kit version (for diagnostics).
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<Version>,
}

#[async_trait]
impl FromElement for KitImageSource {
    type Error = error::Error;

    async fn new(element: &Element, _ctx: &Context) -> Result<Self, error::Error> {
        element.get().map_err(Into::into)
    }
}

#[async_trait]
impl SourceImpl for KitImageSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        Ok(Id::builder()
            .name(self.uri.clone())
            .digest(self.digest.clone())
            .build())
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let uri = Uri::new(&self.uri).await.context(error::OciSnafu)?;

        // Pull the manifest index and verify the merkle digest before
        // writing anything to storage.
        let index = Index::fetch(&uri).await.context(error::OciSnafu)?;
        let mut hasher = Sha256::new();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let digest = base16::encode_lower(hasher.finalize().as_slice());
        ensure!(
            *id.digest() == digest,
            error::DigestSnafu {
                actual: id.digest().clone(),
                expected: digest.clone(),
            }
        );

        // Verify the metadata label on the platform-selected image
        // config before we materialize the layer, so a non-kit OCI image
        // fails fast without polluting the cache.
        let platform = self.platform.clone().unwrap_or_default();
        let image = index
            .fetch_image(&uri, Some(platform.clone()))
            .await
            .context(error::OciSnafu)?
            .context(error::MissingImageSnafu {
                uri: self.uri.clone(),
                platform: platform.clone(),
            })?;
        let image_config = image.fetch_config(&uri).await.context(error::OciSnafu)?;
        let label = image_config.config.labels.get(KIT_METADATA_LABEL).context(
            error::MissingMetadataSnafu {
                uri: self.uri.clone(),
            },
        )?;
        let metadata = KitMetadata::from_label(label)?;

        // Materialize a single OCI layer holding the whole archive,
        // matching edo-core's `ImageSource::fetch` shape.
        let mut artifact = Artifact::builder()
            .config(
                Config::builder()
                    .id(id)
                    .provides(BTreeSet::from_iter([self.uri.to_string()]))
                    .metadata(json!({ "kit": &metadata }))
                    .build(),
            )
            .media_type(MediaType::Manifest)
            .build();

        let writer = storage.safe_start_layer().await?;
        record!(
            log,
            "pull",
            "fetching kit oci archive for image at {} for platform {}",
            uri,
            platform
        );
        index
            .to_oci(
                &uri,
                Some(platform.clone()),
                writer.clone(),
                SharedProgress::none(),
            )
            .await
            .context(error::OciSnafu)?;
        let layer = storage
            .safe_finish_layer(
                &writer,
                &LayerOptions::builder()
                    .media_type(MediaType::Oci(Compression::None))
                    .platform(platform.clone())
                    .build(),
            )
            .await?;
        artifact.layers_mut().push(layer);
        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }
}

pub mod error {
    use ocilot::models::Platform;
    use snafu::Snafu;

    use edo::{
        context::{Addr, error::ContextError},
        source::SourceError,
    };

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context {
            #[snafu(source(from(ContextError, Box::new)))]
            source: Box<ContextError>,
        },
        #[snafu(display("kit image has digest '{actual}' when expecting '{expected}'"))]
        Digest { actual: String, expected: String },
        #[snafu(display("kit image source oci error: {source}"))]
        Oci { source: ocilot::error::Error },
        #[snafu(display("invalid kit image source at {addr}: {source}"))]
        Invalid {
            addr: Addr,
            source: serde_json::Error,
        },
        #[snafu(display("io error occured in kit image source: {source}"))]
        Io { source: std::io::Error },
        #[snafu(display(
            "kit image at {uri} is missing the '{}' label",
            crate::source::kit_image::KIT_METADATA_LABEL
        ))]
        MissingMetadata { uri: String },
        #[snafu(display("kit image at {uri} has no manifest for platform {platform}"))]
        MissingImage { uri: String, platform: Platform },
        #[snafu(display("kit metadata label is not valid base64: {source}"))]
        MetadataBase64 { source: base64::DecodeError },
        #[snafu(display("failed to decode kit metadata document: {source}"))]
        MetadataDecode { source: serde_json::Error },
        #[snafu(display("failed to serialize kit metadata document: {source}"))]
        MetadataEncode { source: serde_json::Error },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata() -> KitMetadata {
        KitMetadata {
            name: "core-kit".to_string(),
            version: Version::parse("1.42.0").unwrap(),
            sdk: KitRef {
                name: "bottlerocket-sdk".to_string(),
                vendor: "bottlerocket".to_string(),
                version: Version::parse("0.76.0").unwrap(),
            },
            kit: vec![KitRef {
                name: "base-kit".to_string(),
                vendor: "bottlerocket".to_string(),
                version: Version::parse("1.0.0").unwrap(),
            }],
        }
    }

    #[test]
    fn kit_metadata_label_round_trip() {
        let original = sample_metadata();
        let label = original.to_label().expect("encode");
        let decoded = KitMetadata::from_label(&label).expect("decode");
        assert_eq!(original, decoded);
    }

    #[test]
    fn kit_metadata_ignores_unknown_fields() {
        let json = r#"{
            "name": "core-kit",
            "version": "1.42.0",
            "sdk": {"name": "bottlerocket-sdk", "vendor": "bottlerocket", "version": "0.76.0"},
            "kit": [],
            "future_field": {"nested": 1}
        }"#;
        let label = BASE64.encode(json);
        let decoded = KitMetadata::from_label(&label).expect("decode");
        assert_eq!(decoded.name, "core-kit");
        assert_eq!(decoded.sdk.name, "bottlerocket-sdk");
        assert!(decoded.kit.is_empty());
    }

    #[test]
    fn kit_metadata_rejects_bad_base64() {
        assert!(matches!(
            KitMetadata::from_label("this is not base64!!!"),
            Err(error::Error::MetadataBase64 { .. })
        ));
    }
}
