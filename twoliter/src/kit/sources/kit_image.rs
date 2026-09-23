//! `kit-image` source: fetch a Bottlerocket kit image from an OCI
//! registry and mirror it into the local edo cache as a native edo
//! artifact.
//!
//! ### Shape of the stored artifact
//!
//! Rather than wrap the whole pulled image in a single opaque OCI
//! archive (which edo would treat as a non-archive and dump verbatim
//! at stage time), we mirror the selected-platform image directly:
//!
//! * Each **content layer** from the OCI image manifest is copied
//!   byte-for-byte into an edo layer tagged
//!   [`MediaType::Tar`]`(compression)`. Copying preserves the OCI
//!   layer digest so [`crate::cmd::publish`] can republish the layer
//!   as a pass-through blob without recomputing anything.
//! * The kit metadata document (the base64-encoded blob from the
//!   `dev.bottlerocket.kit.v3` image-config label) is decoded and
//!   flattened onto [`Config::metadata`]. The shape matches
//!   `KitTransform::build_kit_metadata` so downstream code that
//!   reads `metadata["sdk"]` works uniformly for both external kits
//!   fetched here and kits built locally.
//!
//! Because each content layer is `MediaType::Tar`, edo's
//! `Environment::stage` decompresses + extracts them into the
//! destination directory the caller provided. That's what puts the
//! RPMs on disk under `deps/rpms/…` for the rootfs transform.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use snafu::{OptionExt, ResultExt, ensure};
use std::collections::BTreeSet;

use edo::{
    context::{Context, Element, FromElement, Log},
    record,
    source::{SourceImpl, SourceResult},
    storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage, Digest}
};
use ocilot::{
    index::Index,
    models::{Compression as OciCompression, MediaType as OciMediaType, Platform},
    uri::Uri
};

/// The OCI image-config label that carries the base64-encoded
/// [`KitMetadata`] blob written by the `kit` transform.
///
/// This must match the label emitted by `cmd::publish` and the label
/// used on all published Bottlerocket kit images.
pub const KIT_METADATA_LABEL: &str = "dev.bottlerocket.kit.v4";

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
    digest: Digest,
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
        // Fold the target architecture into the Id so cross-compile
        // builds don't cache-collide: two builds targeting different
        // arches from the same registry image otherwise share
        // `(name, digest)` and would reuse each other's mirrored
        // (arch-specific) content layers.
        //
        // The digest itself is index-wide (merkle of every platform's
        // manifest digest), so it stays stable across arches; edo's
        // `Id` is a `(name, arch, version, digest)` tuple with all
        // fields participating in equality/hash, so tagging `arch`
        // here is what separates the cache entries.
        let platform = self.platform.clone().unwrap_or_default();
        Ok(Id::builder()
            .name(self.uri.clone())
            .digest(self.digest.clone())
            .arch(oci_to_twoliter_arch(&platform))
            .build())
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let uri = Uri::new(&self.uri).await.context(error::OciSnafu)?;

        // Pull the manifest index and verify the merkle digest before
        // writing anything to storage.
        let index = Index::fetch(&uri).await.context(error::OciSnafu)?;
        let mut hasher = Digest::builder();
        for manifest in index.manifests().iter() {
            hasher.update(manifest.digest().as_bytes());
        }
        let digest = hasher.build();
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

        // Build the flattened metadata document. Mirrors the shape
        // `KitTransform::build_kit_metadata` writes for locally-built
        // kits so downstream code (e.g. `cmd::publish`) that reads
        // `metadata["sdk"]` works uniformly for both origins. We omit
        // the `packages` field: the label document does not carry
        // it, and no consumer needs it today.
        let metadata_doc = flatten_kit_metadata(&metadata);

        let mut artifact = Artifact::builder()
            .config(
                Config::builder()
                    .id(id)
                    .provides(BTreeSet::from_iter([self.uri.to_string()]))
                    .metadata(metadata_doc)
                    .build(),
            )
            .media_type(MediaType::Manifest)
            .build();

        record!(
            log,
            "pull",
            "mirroring kit image from {} for platform {} ({} content layer(s))",
            uri,
            platform,
            image.layers().len()
        );

        // Mirror each content layer as an edo `Tar(compression)`
        // layer. Copying the compressed bytes verbatim lets us pin
        // the stored digest to the OCI layer's digest so republish
        // can pass the blob through untouched.
        for image_layer in image.layers().iter() {
            let edo_compression =
                map_layer_compression(image_layer.media_type()).with_context(|| {
                    error::UnsupportedLayerMediaTypeSnafu {
                        uri: self.uri.clone(),
                        media_type: format!("{:?}", image_layer.media_type()),
                    }
                })?;

            // Strip the `sha256:` prefix if present so the digest we
            // pin on the writer matches edo's bare-hex convention.
            let oci_digest = image_layer.digest();
            let bare_digest = oci_digest
                .strip_prefix("sha256:")
                .unwrap_or(oci_digest)
                .to_string();

            let writer = storage.safe_start_layer().await?;
            record!(
                log,
                "pull",
                "mirroring layer sha256:{} ({} bytes, {:?})",
                bare_digest,
                image_layer.size(),
                image_layer.media_type()
            );

            let mut reader = image_layer
                .open(&uri, None)
                .await
                .context(error::OciSnafu)?;
            let mut writer_handle = writer.clone();
            ocilot::layer::Layer::copy(&mut reader, &mut writer_handle, image_layer.size())
                .await
                .context(error::OciSnafu)?;

            let layer = storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(edo_compression))
                        .platform(platform.clone())
                        .build(),
                )
                .await?;
            artifact.layers_mut().push(layer);
        }

        storage.safe_save(&artifact).await?;
        Ok(artifact)
    }
}

/// Flatten a decoded `KitMetadata` label into the JSON shape used on
/// the artifact's `Config::metadata`. Matches
/// `KitTransform::build_kit_metadata` for the fields it can populate
/// (`name`, `version`, `sdk`, `kit`). Unlike locally-built kits, the
/// SDK descriptor here lacks `digest` and `source` because the label
/// does not carry them.
fn flatten_kit_metadata(m: &KitMetadata) -> Value {
    json!({
        "name": m.name,
        "version": m.version.to_string(),
        "sdk": {
            "name": m.sdk.name,
            "vendor": m.sdk.vendor,
            "version": m.sdk.version.to_string(),
        },
        "kit": m
            .kit
            .iter()
            .map(|k| json!({
                "name": k.name,
                "vendor": k.vendor,
                "version": k.version.to_string(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Translate an OCI platform architecture (`amd64` / `arm64`) into
/// the Rust / twoliter arch spelling (`x86_64` / `aarch64`) used
/// throughout the rest of the crate — matches the direction of the
/// inverse translation performed by `cmd::publish` and the arch
/// argument threaded through every variant transform. Unknown values
/// pass through verbatim so we never silently drop information.
pub(crate) fn oci_to_twoliter_arch(platform: &Platform) -> String {
    match platform.architecture.as_str() {
        "amd64" => "x86_64".to_string(),
        "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    }
}

/// Inverse of [`oci_to_twoliter_arch`]: translate a twoliter/Rust
/// arch spelling into an OCI platform. Used by upstream transforms
/// when they need to synthesize a `platform` block on a resolved
/// kit-image element so the source's [`get_unique_id`] and
/// `fetch` see the caller's target arch.
pub(crate) fn twoliter_arch_to_oci_platform(arch: &str) -> Platform {
    let architecture = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    Platform::builder()
        .os("linux")
        .architecture(architecture)
        .build()
}

/// Map an ocilot content-layer media type to the corresponding edo
/// [`Compression`]. Only tarball layer types are recognized; anything
/// else returns `None` so the caller surfaces an error rather than
/// silently pretending a non-archive is an archive.
fn map_layer_compression(mt: &OciMediaType) -> Option<Compression> {
    let compression = match mt {
        OciMediaType::Layer(c) | OciMediaType::DockerImageRootfs(c) => c,
        _ => return None,
    };
    Some(match compression {
        OciCompression::None => Compression::None,
        OciCompression::Gzip => Compression::Gzip,
        OciCompression::Bzip2 => Compression::Bzip2,
        OciCompression::Lz4 => Compression::Lz4,
        OciCompression::Xz => Compression::Xz,
        OciCompression::Zstd => Compression::Zstd,
    })
}

pub mod error {
    use ocilot::models::Platform;
    use snafu::Snafu;

    use edo::{
        context::{Addr, error::ContextError},
        source::SourceError,
        storage::Digest,
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
        Digest { actual: Digest, expected: Digest },
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
            crate::kit::KIT_METADATA_LABEL
        ))]
        MissingMetadata { uri: String },
        #[snafu(display("kit image at {uri} has no manifest for platform {platform}"))]
        MissingImage { uri: String, platform: Platform },
        #[snafu(display(
            "kit image at {uri} has a content layer with unsupported media type {media_type}"
        ))]
        UnsupportedLayerMediaType { uri: String, media_type: String },
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

    /// The flattened metadata document must expose `name`, `version`,
    /// `sdk`, and `kit` at the root — the same keys locally-built kits
    /// carry, so `cmd::publish` and any future readers see one shape
    /// regardless of origin.
    #[test]
    fn flatten_kit_metadata_matches_local_shape() {
        let doc = flatten_kit_metadata(&sample_metadata());
        assert_eq!(doc["name"], "core-kit");
        assert_eq!(doc["version"], "1.42.0");
        let sdk = &doc["sdk"];
        assert_eq!(sdk["name"], "bottlerocket-sdk");
        assert_eq!(sdk["vendor"], "bottlerocket");
        assert_eq!(sdk["version"], "0.76.0");
        let kit = doc["kit"].as_array().expect("kit array");
        assert_eq!(kit.len(), 1);
        assert_eq!(kit[0]["name"], "base-kit");
        assert_eq!(kit[0]["version"], "1.0.0");
        // No `packages` key: the label document doesn't carry it and
        // we don't synthesize one here.
        assert!(doc.get("packages").is_none());
    }

    fn sample_source(platform: Option<Platform>) -> KitImageSource {
        KitImageSource {
            uri: "public.ecr.aws/bottlerocket/core-kit:v1.42.0".to_string(),
            digest: Digest::builder().update(b"aabbccdd").build(),
            platform,
            vendor: None,
            name: None,
            version: None,
        }
    }

    /// Two `KitImageSource`s pointing at the same registry image but
    /// targeting different arches must produce distinct `Id`s, or the
    /// local storage cache would key both entries the same and reuse
    /// one arch's mirrored layers for the other arch.
    #[tokio::test]
    async fn get_unique_id_separates_per_arch() {
        let amd = sample_source(Some(twoliter_arch_to_oci_platform("x86_64")));
        let arm = sample_source(Some(twoliter_arch_to_oci_platform("aarch64")));
        let amd_id = amd.get_unique_id().await.expect("amd id");
        let arm_id = arm.get_unique_id().await.expect("arm id");
        assert_eq!(amd_id.arch().as_deref(), Some("x86_64"));
        assert_eq!(arm_id.arch().as_deref(), Some("aarch64"));
        assert_ne!(amd_id, arm_id);
        // Same underlying merkle digest — arch is the discriminator.
        assert_eq!(amd_id.digest(), arm_id.digest());
    }

    /// When no `platform` is set on the source, we still tag the Id
    /// with the host arch — so a source that omits `platform`
    /// entirely still produces a distinct Id from one explicitly
    /// targeting the *other* arch.
    #[tokio::test]
    async fn get_unique_id_defaults_to_host_arch() {
        let src = sample_source(None);
        let id = src.get_unique_id().await.expect("id");
        // Host arch depends on the test machine; assert it's one of
        // the recognized OCI-to-twoliter mappings rather than pinning
        // a specific value.
        let arch = id.arch().expect("arch tag present");
        assert!(
            matches!(arch.as_str(), "x86_64" | "aarch64"),
            "unexpected host arch tag: {arch}"
        );
    }

    /// OCI ↔ twoliter arch translation must round-trip for the two
    /// arches Bottlerocket actually supports, and preserve unknown
    /// values verbatim so a config typo surfaces at use-time rather
    /// than silently becoming one of the known values.
    #[test]
    fn arch_translation_round_trips() {
        for (twoliter, oci) in [("x86_64", "amd64"), ("aarch64", "arm64")] {
            let platform = twoliter_arch_to_oci_platform(twoliter);
            assert_eq!(platform.architecture, oci);
            assert_eq!(platform.os, "linux");
            assert_eq!(oci_to_twoliter_arch(&platform), twoliter);
        }
        // Unknown arches pass through so the resulting error is
        // legible ("unknown arch riscv64") instead of a silent
        // reinterpretation.
        let weird = Platform::builder()
            .os("linux")
            .architecture("riscv64")
            .build();
        assert_eq!(oci_to_twoliter_arch(&weird), "riscv64");
    }

    /// Tarball layer types must map to `MediaType::Tar(_)` with the
    /// matching compression; unknown types must return `None` so the
    /// caller surfaces an error instead of silently pretending a
    /// non-archive is an archive.
    #[test]
    fn map_layer_compression_covers_expected_variants() {
        assert_eq!(
            map_layer_compression(&OciMediaType::Layer(OciCompression::Gzip)),
            Some(Compression::Gzip)
        );
        assert_eq!(
            map_layer_compression(&OciMediaType::Layer(OciCompression::Zstd)),
            Some(Compression::Zstd)
        );
        assert_eq!(
            map_layer_compression(&OciMediaType::Layer(OciCompression::None)),
            Some(Compression::None)
        );
        assert_eq!(
            map_layer_compression(&OciMediaType::DockerImageRootfs(OciCompression::Gzip)),
            Some(Compression::Gzip)
        );
        assert_eq!(map_layer_compression(&OciMediaType::Config), None);
        assert_eq!(map_layer_compression(&OciMediaType::Manifest), None);
        assert_eq!(map_layer_compression(&OciMediaType::ImageIndex), None);
    }
}
