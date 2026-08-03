use std::collections::{BTreeSet, HashMap};

use base64::Engine;
use clap::Parser;
use edo::{
    context::Addr,
    storage::{Id, MediaType as EdoMediaType},
    util::Reader,
};
use jiff::Timestamp;
use ocilot::{
    image::Image,
    index::Index,
    layer::Layer,
    models::{Compression, Config, MediaType, Platform},
    registry::Registry,
    repository::Repository,
    uri::{RegistryUri, Uri},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt};
use tokio::io::AsyncWriteExt;

use crate::{Args, Result, error};

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "publish a kit or variant", long_about = None)]
pub struct Publish {
    #[clap(subcommand)]
    command: PublishCommand,
}

#[derive(Parser, Debug, Clone)]
enum PublishCommand {
    Kit(PublishKit),
}

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "publish a kit to an oci registry", long_about = None)]
pub struct PublishKit {
    name: String,
    version: semver::Version,
    uri: String,
}

impl Publish {
    pub async fn run(&self, args: Args) -> Result<()> {
        match &self.command {
            PublishCommand::Kit(kit) => kit.run(args).await,
        }
    }
}

impl PublishKit {
    pub async fn run(&self, args: Args) -> Result<()> {
        let addr = Addr::parse(&format!("//kits/{}/build", self.name))?;
        let ctx = super::create_context(&args, &addr.to_string(), HashMap::default(), true, false)
            .await?;
        // We get the transform for the kit from the ctx
        let transform = ctx
            .get_transform(&addr)
            .context(error::NoKitSnafu { addr })?;
        let handle = ctx.get_handle();
        let id = transform.get_unique_id(&handle).await?;

        // Create the Ocilot Registry handler
        let uri = Uri::new(&self.uri).await?;
        let mut images = Vec::new();
        // Now find any artifacts that match
        let local_artifacts: BTreeSet<Id> = ctx.storage().safe_list().await?;
        for id in local_artifacts
            .iter()
            .filter(|x| x.name() == id.name() && x.version() == id.version())
        {
            // Now check for the artifact
            if ctx.storage().has_local(&id).await? {
                let artifact = ctx.storage().safe_open(&id).await?;

                // We want to upload each layer needed for the image.
                //
                // For every artifact layer we compute two things: the
                // ocilot media type to declare on the pushed manifest
                // (derived from the artifact layer's *stored*
                // compression, so gzip/zstd payloads aren't mislabeled
                // as uncompressed) and the OCI diff_id (SHA256 of the
                // *uncompressed* tar bytes) that goes into the image
                // config's `rootfs.diff_ids`. For uncompressed layers
                // diff_id == blob digest and we skip the extra read.
                let mut oci_layers = Vec::new();
                let mut diff_ids: Vec<String> = Vec::new();
                for artifact_layer in artifact.layers() {
                    edo::ui_start_task!(
                        subsystem = "kit",
                        component = "layer",
                        op = "publish";
                        &artifact_layer.digest().digest(),
                        edo::ui::UiTaskStatus::Running,
                        None
                    );

                    let ocilot_media = ocilot_media_type_for(artifact_layer.media_type())
                        .with_context(|| error::UnsupportedPublishLayerMediaTypeSnafu {
                            media_type: format!("{:?}", artifact_layer.media_type()),
                        })?;

                    let compressed_digest = artifact_layer.digest().digest();
                    let compressed_digest_ref = format!("sha256:{compressed_digest}");
                    let mut reader = ctx.storage().safe_read(artifact_layer).await?;
                    oci_layers.push(
                        if let Some(mut oci_layer) = Layer::create(
                            &uri,
                            &ocilot_media,
                            *artifact_layer.size(),
                            Some(compressed_digest_ref.clone()),
                            None,
                        )
                        .await?
                        {
                            tokio::io::copy(&mut reader, &mut oci_layer)
                                .await
                                .context(error::IoSnafu)?;
                            oci_layer.flush().await.context(error::IoSnafu)?;
                            oci_layer.shutdown().await.context(error::IoSnafu)?;
                            oci_layer.layer().await?
                        } else {
                            Layer::builder()
                                .media_type(ocilot_media.clone())
                                .size(*artifact_layer.size())
                                .digest(compressed_digest_ref.clone())
                                .build()
                        },
                    );

                    // diff_id: uncompressed tar digest. For a `Tar(None)`
                    // layer the compressed bytes *are* the uncompressed
                    // bytes, so the layer's own digest is the diff_id
                    // and no extra work is required. For compressed
                    // layers we open a second reader and stream through
                    // a decompressor whose SHA256 is exposed via
                    // `Reader::finish`.
                    let diff_id = if artifact_layer.media_type().is_compressed() {
                        let raw_reader = ctx.storage().safe_read(artifact_layer).await?;
                        let compression = artifact_layer.media_type().compression();
                        let mut decompress = Reader::with_decompression(raw_reader, &compression);
                        tokio::io::copy(&mut decompress, &mut tokio::io::sink())
                            .await
                            .context(error::IoSnafu)?;
                        decompress.finish()
                    } else {
                        compressed_digest.clone()
                    };
                    diff_ids.push(format!("sha256:{diff_id}"));

                    edo::ui_update_task!(
                        subsystem = "kit",
                        component = "layer",
                        op = "publish";
                        &artifact_layer.digest().digest(),
                        edo::ui::UiTaskStatus::Success,
                        None
                    );
                }

                // Construct the kit metadata from the artifact manifest
                let artifact_metadata = artifact.config().metadata();
                let sdk_block = artifact_metadata.get("sdk").unwrap();
                let metadata = json!({
                    "kit": [],
                    "name": self.name,
                    "sdk": sdk_block,
                    "version": &self.version
                });
                let metadata_bytes = serde_json::to_vec_pretty(&metadata).unwrap();
                let metadata_str = base64::engine::general_purpose::STANDARD.encode(metadata_bytes);
                // Now we wnat to translate the artifact manifest into two things: the image config, then the image manifest
                let config = json!({
                    "architecture": &id.arch().unwrap(),
                    "config": {
                        "Env": [],
                        "WorkingDir": "/",
                        "OnBuild": null,
                        "Labels": {
                            "dev.bottlerocket.kit.v3": metadata_str
                        }
                    },
                    "created": &Timestamp::now(),
                    "history": [],
                    "os": "linux",
                    "rootfs": {
                        "type": "layers",
                        "diff_ids": diff_ids
                    }
                });
                let config_bytes = serde_json::to_vec(&config).unwrap();
                let mut config_layer =
                    Layer::create(&uri, &MediaType::Config, config_bytes.len(), None, None)
                        .await?
                        .unwrap();
                config_layer
                    .write_all(config_bytes.as_slice())
                    .await
                    .context(error::IoSnafu)?;
                config_layer.flush().await.context(error::IoSnafu)?;
                config_layer.shutdown().await.context(error::IoSnafu)?;
                let config_layer = config_layer.layer().await?;

                // Now we can create the Image manifest
                let image = Image::builder()
                    .schema_version(2usize)
                    .media_type(MediaType::Manifest)
                    .config(config_layer)
                    .layers(oci_layers)
                    .platform(
                        Platform::builder()
                            .os("linux")
                            .architecture(if id.arch().unwrap() == "aarch64" {
                                "arm64"
                            } else {
                                "amd64"
                            })
                            .build(),
                    )
                    .build();

                // Push the image
                images.push(image.push(&uri).await?);
            }
        }

        if !images.is_empty() {
            // If we uploaded at least one image we create and update the index
            let index = Index::builder()
                .schema_version(2usize)
                .media_type(MediaType::ImageIndex)
                .manifests(images)
                .build();
            index.push(&uri).await?;
        }

        Ok(())
    }
}

/// Map an edo artifact layer's media type to the ocilot media type we
/// declare on the pushed OCI manifest.
///
/// Only `Tar(_)` layers are meaningful in a kit-publish context: kit
/// artifacts always carry tar payloads (locally-built kits emit
/// `Tar(None)`; `KitImageSource` mirrors upstream layers as `Tar(_)`
/// preserving the upstream compression). Any other media type is a
/// programming error at this call site, so we surface it explicitly
/// rather than silently mislabel the blob.
fn ocilot_media_type_for(mt: &EdoMediaType) -> Option<MediaType> {
    let edo_compression = match mt {
        EdoMediaType::Tar(c) => c,
        _ => return None,
    };
    let ocilot_compression = match edo_compression {
        edo::storage::Compression::None => Compression::None,
        edo::storage::Compression::Gzip => Compression::Gzip,
        edo::storage::Compression::Bzip2 => Compression::Bzip2,
        edo::storage::Compression::Lz4 => Compression::Lz4,
        edo::storage::Compression::Xz => Compression::Xz,
        edo::storage::Compression::Zstd => Compression::Zstd,
        // Lzma has no `application/vnd.oci.image.layer.v1.tar.lzma`
        // in ocilot's serializer; if we ever encounter it we surface
        // it rather than aliasing to another codec.
        edo::storage::Compression::Lzma => return None,
    };
    Some(MediaType::Layer(ocilot_compression))
}

#[cfg(test)]
mod tests {
    use super::*;
    use edo::storage::{Compression as EdoCompression, MediaType as EdoMediaType};

    /// Every stored `Tar(compression)` variant must map to the
    /// matching ocilot layer media type so the pushed manifest
    /// agrees with the actual blob compression. Non-tar variants must
    /// return `None` — the caller surfaces them as a hard error.
    #[test]
    fn ocilot_media_type_covers_tar_variants() {
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::None)),
            Some(MediaType::Layer(Compression::None))
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Gzip)),
            Some(MediaType::Layer(Compression::Gzip))
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Zstd)),
            Some(MediaType::Layer(Compression::Zstd))
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Bzip2)),
            Some(MediaType::Layer(Compression::Bzip2))
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Xz)),
            Some(MediaType::Layer(Compression::Xz))
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Lz4)),
            Some(MediaType::Layer(Compression::Lz4))
        );
    }

    /// Non-tar / unsupported edo media types must surface as `None`
    /// so we never mislabel a non-tar blob as an OCI tar layer.
    #[test]
    fn ocilot_media_type_rejects_non_tar_and_lzma() {
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Tar(EdoCompression::Lzma)),
            None
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::File(EdoCompression::None)),
            None
        );
        assert_eq!(
            ocilot_media_type_for(&EdoMediaType::Oci(EdoCompression::None)),
            None
        );
        assert_eq!(ocilot_media_type_for(&EdoMediaType::Manifest), None);
    }
}
