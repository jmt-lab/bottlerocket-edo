use crate::publish::config::InfraConfig;
use crate::{
    cmd::create_context,
    publish::{Args, config, kit},
};
use base64::Engine;
use clap::Parser;
use edo::context::Addr;
use edo::ui_info;
use indicatif::MultiProgress;
use jiff::Timestamp;
use ocilot::{
    image::Image,
    index::Index,
    layer::Layer,
    models::{Config, ImageConfig, MediaType, Platform},
    progress::IndicatifReporter,
    registry::Registry,
    uri::{Reference, RegistryUri, Uri},
};
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ResultExt, ensure};
use std::{collections::HashMap, str::FromStr};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, trace};
use twoliter::model::Arch;

const KIT_METADATA_VERSION: &'static str = "dev.bottlerocket.kit.v3";

/// Takes a local kit built using buildsys and publishes it to a vendor specified in Infra.toml
#[derive(Debug, Parser)]
pub(crate) struct PublishKitArgs {
    /// The kit name to publish
    #[arg(long)]
    kit: String,

    /// Vendor to publish kit to
    #[arg(long)]
    vendor: String,

    /// Optionally push the kit a different repository name
    #[arg(long)]
    repo: Option<String>,
}

pub(crate) async fn run(
    args: &crate::Args,
    publish_args: &crate::publish::Args,
    publish_kit_args: &PublishKitArgs,
) -> Result<()> {
    // If a lock file exists, use that, otherwise use Infra.toml
    let infra_config = InfraConfig::from_path_or_lock(&publish_args.infra_config_path, false)
        .context(error::ConfigSnafu)?;
    trace!("Parsed infra config: {infra_config:?}");

    publish_kit(&args, infra_config, publish_kit_args).await
}

async fn publish_kit(
    args: &crate::Args,
    infra_config: InfraConfig,
    publish_kit_args: &PublishKitArgs,
) -> Result<()> {
    // Fetch the vendor container registry uri
    let vendor = infra_config
        .vendor
        .as_ref()
        .context(error::NoVendorsSnafu)?
        .get(&publish_kit_args.vendor)
        .context(error::VendorNotFoundSnafu {
            name: publish_kit_args.vendor.clone(),
        })?;
    let vendor_registry_uri = vendor.registry.clone();
    debug!("Found vendor container registry at uri: {vendor_registry_uri}");

    let repository_target = match publish_kit_args.repo.as_ref() {
        Some(repo) => repo.clone(),
        None => publish_kit_args.kit.clone(),
    };
    // Open the ocilot target
    let registry_uri =
        RegistryUri::from_str(&vendor_registry_uri).context(error::PublishKitSnafu)?;
    let registry = Registry::new(&registry_uri)
        .await
        .context(error::PublishKitSnafu)?;
    let layer_type = MediaType::Layer(ocilot::models::Compression::None);
    let progress = MultiProgress::new();
    let reporter = IndicatifReporter::new(progress);

    let mut platform_images = Vec::new();
    let mut version = None;
    for arch in [Arch::Aarch64, Arch::X86_64] {
        // Build the Edo Context for the architecture
        let addr_s = format!("//kits/{}/build", publish_kit_args.kit);
        let addr = Addr::parse(&addr_s)?;
        // Load the configuration shape
        let ctx = create_context(
            &args,
            &addr_s,
            HashMap::from([("arch".to_string(), arch.to_string())]),
            true,
            false,
        )
        .await?;
        let handle = ctx.get_handle();
        let transform = handle.get(&addr).context(error::InvalidKitSnafu {
            name: publish_kit_args.kit.clone(),
        })?;
        let id = transform.get_unique_id(&handle).await?;
        let kit_version = id.version().unwrap(); // TODO: error handle
        version = Some(kit_version.clone());

        // Get the Artifact Manifest, the metadata in the config will have
        // everything we need for the metadata label.
        //
        // Strip `packages` before encoding: legacy twoliter deserializes
        // this label with `deny_unknown_fields` and only knows `name`,
        // `version`, `sdk`, `kit`. Emitting `packages` on the published
        // label breaks legacy AMI builds that consume the kit. We keep it
        // on the local artifact metadata so in-project tooling can still
        // enumerate kit contents without parsing layers.
        let artifact = ctx.storage().safe_open(&id).await?;
        let mut metadata = artifact.config().metadata().clone();
        if let Some(obj) = metadata.as_object_mut() {
            obj.remove("packages");
        }
        let metadata_label = serde_json::to_vec(&metadata).context(error::SerializeSnafu)?;
        let metadata_label = base64::engine::general_purpose::STANDARD.encode(&metadata_label);

        // Construct the URI target
        let uri = Uri::builder()
            .registry(registry.clone())
            .repository(repository_target.clone())
            .reference(Reference::Tag(format!("v{kit_version}-{arch}")))
            .build();
        let image_platform = Platform::builder()
            .os("linux")
            .architecture(arch.goarch())
            .build();
        ui_info!(subsystem = "twoliter", component = "kit", op = "publish"; "pushing kit image for platform {arch} to {uri:?}");

        // We now will want to upload the layers
        // TODO: handle a safe parallelization
        let mut image_layers = Vec::new();
        for layer in artifact.layers() {
            // The layer in edo is 1:1 the oci layer with a small media_type change
            let mut writer = Layer::create(
                &uri,
                &layer_type,
                *layer.size(),
                Some(layer.digest().to_string()),
                Some(&reporter),
            )
            .await
            .context(error::PublishKitSnafu)?;
            if let Some(mut writer) = writer.as_mut() {
                let mut reader = ctx.storage().safe_read(layer).await?;
                tokio::io::copy(&mut reader, &mut writer)
                    .await
                    .context(error::CopyLayerSnafu)?;
                writer.flush().await.context(error::CopyLayerSnafu)?;
                writer.shutdown().await.context(error::CopyLayerSnafu)?;
                image_layers.push(writer.layer().await.context(error::PublishKitSnafu)?);
            } else {
                edo::ui_info!(subsystem = "twoliter", component = "kit", op = "publish"; "layer with digest {} already exists", layer.digest());
                // Layer entries in an OCI image manifest must not carry a
                // `platform` field; that belongs on manifest entries of an
                // image index. ECR's PutImage rejects the whole manifest
                // (HTTP 405 "Invalid JSON syntax") when it sees it here.
                image_layers.push(
                    Layer::builder()
                        .media_type(layer_type.clone())
                        .size(*layer.size())
                        .digest(layer.digest().to_string())
                        .build(),
                );
            }
        }

        // Create the Image Config
        let image_config = ImageConfig::builder()
            .architecture(arch.goarch())
            .config(
                Config::builder()
                    .env(vec![])
                    .working_dir("/")
                    .args_escaped(false)
                    .labels(HashMap::from([(
                        KIT_METADATA_VERSION.to_string(),
                        metadata_label.clone(),
                    )]))
                    .cmd(vec![])
                    .build(),
            )
            .created(Timestamp::now())
            .history(vec![])
            .os("linux")
            .build();
        // We now serialize and push this as a layer
        let config_bytes = serde_json::to_vec(&image_config).context(error::SerializeSnafu)?;
        let mut writer = Layer::create(
            &uri,
            &ocilot::models::MediaType::Config,
            config_bytes.len(),
            None,
            None,
        )
        .await
        .context(error::PublishKitSnafu)?;
        let config_layer = if let Some(writer) = writer.as_mut() {
            writer
                .write_all(&config_bytes)
                .await
                .context(error::CopyLayerSnafu)?;
            writer.flush().await.context(error::CopyLayerSnafu)?;
            writer.shutdown().await.context(error::CopyLayerSnafu)?;
            writer.layer().await.context(error::PublishKitSnafu)?
        } else {
            let config_hash = Sha256::digest(&config_bytes);
            let config_encoded = base16::encode_lower(config_hash.as_slice());
            Layer::builder()
                .media_type(ocilot::models::MediaType::Config)
                .size(config_bytes.len())
                .digest(format!("sha256:{config_encoded}"))
                .build()
        };

        // Now we make the image
        let image = Image::create(&config_layer, &image_layers, Some(image_platform.clone())).await;
        // Push the image
        platform_images.push(image.push(&uri).await.context(error::PublishKitSnafu)?);
    }
    ui_info!(subsystem = "twoliter", component = "kit", op = "publish"; "pushing oci image index");
    ensure!(!platform_images.is_empty(), error::NoArchiveSnafu);

    // Now we construct the index to push
    let index_uri = Uri::builder()
        .registry(registry.clone())
        .repository(repository_target.clone())
        .reference(Reference::Tag(format!("v{}", version.clone().unwrap())))
        .build();
    let target_uri = format!(
        "{vendor_registry_uri}/{repository_target}:v{}",
        version.clone().unwrap()
    );

    info!("Pushing kit to {}", target_uri);
    let index = Index::new(&platform_images).await;
    index
        .push(&index_uri)
        .await
        .context(error::PublishKitSnafu)?;
    info!("Successfully published kit to {target_uri}");

    Ok(())
}

mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(transparent)]
        Root { source: crate::error::Error },
        #[snafu(display("Error reading config: {}", source))]
        Config {
            source: crate::publish::config::Error,
        },

        #[snafu(display("Failed to upload layer: {source}"))]
        CopyLayer { source: std::io::Error },

        #[snafu(display("Could not convert {} to docker architecture: {}", arch, source))]
        InvalidArchitecture {
            source: ocilot::error::Error,
            arch: String,
        },

        #[snafu(display("There is no kit in project named {name}"))]
        InvalidKit { name: String },

        #[snafu(display("No kit archive(s) exist in build cache"))]
        NoArchive,

        #[snafu(display("No vendors specified in Infra.toml, you must specify at least one"))]
        NoVendors,

        #[snafu(display("Could not publish kit: {}", source))]
        PublishKit { source: ocilot::error::Error },

        #[snafu(display("Vendor '{}' not specified in Infra.toml", name))]
        VendorNotFound { name: String },

        #[snafu(transparent)]
        Context { source: edo::context::ContextError },

        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },

        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },

        #[snafu(display("Failed to serialize kit metadata: {source}"))]
        Serialize { source: serde_json::Error },
    }
}

pub(crate) use error::Error;

type Result<T> = std::result::Result<T, Error>;
