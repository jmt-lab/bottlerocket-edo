use std::collections::{BTreeSet, HashMap};

use base64::Engine;
use clap::Parser;
use edo::{context::Addr, storage::Id};
use jiff::Timestamp;
use ocilot::{image::Image, index::Index, layer::Layer, models::{Compression, Config, MediaType, Platform}, registry::Registry, repository::Repository, uri::{RegistryUri, Uri}};
use serde_json::json;
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt};
use tokio::io::AsyncWriteExt;

use crate::{Args, Result, error};

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "publish a kit or variant", long_about = None)]
pub struct Publish {
    #[clap(subcommand)]
    command: PublishCommand
}

#[derive(Parser, Debug, Clone)]
enum PublishCommand {
    Kit(PublishKit)
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
            PublishCommand::Kit(kit) => kit.run(args).await
        }
    }
}

impl PublishKit {
    pub async fn run(&self, args: Args) -> Result<()> {
        let addr = Addr::parse(&format!("//kits/{}/build", self.name))?;
        let ctx = super::create_context(
            &args,
            &addr.to_string(),
            HashMap::default(),
            true,
            false
        ).await?;
        // We get the transform for the kit from the ctx
        let transform = ctx.get_transform(&addr).context(error::NoKitSnafu { addr })?;
        let handle = ctx.get_handle();
        let id = transform.get_unique_id(&handle).await?;

        // Create the Ocilot Registry handler
        let uri = Uri::new(&self.uri).await?;
        let mut images = Vec::new();
        // Now find any artifacts that match
        let local_artifacts: BTreeSet<Id> = ctx.storage().safe_list().await?;
        for id in local_artifacts.iter().filter(|x| x.name() == id.name() && x.version() == id.version()) {
            // Now check for the artifact
            if ctx.storage().has_local(&id).await? {
                let artifact = ctx.storage().safe_open(&id).await?;

                // We want to upload each layer needed for the image
                let mut oci_layers = Vec::new();
                for artifact_layer in artifact.layers() {
                    println!("writing layer: {}", artifact_layer.digest().digest());
                    let mut reader = ctx.storage().safe_read(artifact_layer).await?;
                    oci_layers.push(if let Some(mut oci_layer) = Layer::create(
                        &uri,
                        &MediaType::Layer(Compression::None),
                        *artifact_layer.size(),
                        Some(format!("sha256:{}", artifact_layer.digest().digest())),
                        None
                    ).await? {
                        tokio::io::copy(&mut reader, &mut oci_layer).await.context(error::IoSnafu)?;
                        oci_layer.flush().await.context(error::IoSnafu)?;
                        oci_layer.shutdown().await.context(error::IoSnafu)?;
                        oci_layer.layer().await?
                    } else {
                        Layer::builder()
                            .media_type(MediaType::Layer(Compression::None))
                            .size(*artifact_layer.size())
                            .digest(format!("sha256:{}", artifact_layer.digest().digest()))
                            .build()
                    });
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
                let diff_ids = oci_layers.iter().map(|x| x.digest().strip_prefix("sha256:").unwrap_or(x.digest())).collect::<Vec<_>>();
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
                println!("writing config");
                let mut config_layer = Layer::create(&uri, &MediaType::Config, config_bytes.len(), None, None).await?.unwrap();
                config_layer.write_all(config_bytes.as_slice()).await.context(error::IoSnafu)?;
                config_layer.flush().await.context(error::IoSnafu)?;
                config_layer.shutdown().await.context(error::IoSnafu)?;
                let config_layer = config_layer.layer().await?;

                // Now we can create the Image manifest
                let image = Image::builder()
                    .schema_version(2usize)
                    .media_type(MediaType::Manifest)
                    .config(config_layer)
                    .layers(oci_layers)
                    .platform(Platform::builder()
                        .os("linux")
                        .architecture(if id.arch().unwrap() == "aarch64" {
                            "arm64"
                        } else {
                            "amd64"
                        })
                        .build()
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
