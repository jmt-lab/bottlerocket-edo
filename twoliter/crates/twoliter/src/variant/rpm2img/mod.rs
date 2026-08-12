mod constants;

use std::path::Path;

use crate::{
    model::Arch,
    variant::{
        ImageFormat, ImageOptions, OsOptions, PartitionPlan, Variant, VariantOptions,
        rpm2img::constants::{IMGHELPER, PARTYPLANNER, RPM2IMG},
    },
};
use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log}, environment::{Environment, Vfs}, storage::{Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType}, transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ensure};
use tracing::trace;

pub struct RPM2ImageTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: VariantOptions,
}

#[async_trait]
impl FromElement for RPM2ImageTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let options: VariantOptions = element.get()?;
        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
        })
    }
}

#[async_trait]
impl TransformImpl for RPM2ImageTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let mut digest = Sha256::new();

        // (1) All artifacts coming from dependent transforms
        for addr in self
            .options
            .depends
            .iter()
            .chain(self.options.guest_images.iter())
        {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            digest.update(id.digest().as_bytes());
        }

        // (2) All settings that could alter the output images
        let kernel_param_str = self.options.os.kernel_parameters.join(", ");
        digest.update(kernel_param_str.as_bytes());
        digest.update(match self.options.image.format {
            ImageFormat::Eif => b"eif00",
            ImageFormat::Qcow2 => b"qcow2",
            ImageFormat::Raw => b"raw00",
            ImageFormat::Vmdk => b"vmdk0",
        });
        digest.update(match self.options.image.partitions {
            PartitionPlan::Split => b"split00",
            PartitionPlan::Unified => b"unified",
        });
        digest.update(self.options.image.os_size.unwrap_or(1u16).to_be_bytes());
        digest.update(self.options.image.data_size.unwrap_or(1u16).to_be_bytes());
        digest.update(self.options.image.features.as_bytes());
        let digest = digest.finalize();
        let digest = base16::encode_lower(digest.as_slice());
        let id = Id::builder()
            .name(format!("{}-img", self.options.name))
            .digest(digest)
            .version(self.options.version.clone())
            .arch(self.options.arch.to_string())
            .build();
        trace!(
            component = "transform",
            type = "rpm2img",
            "id is calculated to be {id}"
        );
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self
            .options
            .depends
            .iter()
            .chain(self.options.guest_images.iter())
            .cloned()
            .collect())
    }

    async fn prepare(&self, _log: &Log, _ctx: &Handle) -> TransformResult<()> {
        Ok(())
    }

    async fn needs_prepare(&self, _ctx: &Handle) -> TransformResult<bool> {
        Ok(false)
    }

    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let id = self.get_unique_id(ctx).await?;
        let rpm_root = Path::new("rpms");
        let bin_root = Path::new("bin");
        let images_root = Path::new("images");
        env.create_dir(rpm_root).await?;
        env.create_dir(bin_root).await?;
        env.create_dir(images_root).await?;

        // Stage our rpm repository
        for addr in self.options.depends.iter() {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(
                component = "transform",
                type = "rpm2img",
                "staging rpm repository {addr} with id {id}",
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(rpm_root)
                    .build(),
            )
            .await?;
        }

        // Stage our guest images
        for addr in self.options.guest_images.iter() {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(
                component = "transform",
                type = "rpm2img",
                "staging guest image {addr} with id {id}"
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(images_root)
                    .build(),
            )
            .await?;
        }

        // Now Stage and make executable our scripts that drive the transform
        for (script, contents) in [
            ("partyplanner", PARTYPLANNER),
            ("imghelper", IMGHELPER),
            ("rpm2img", RPM2IMG),
        ] {
            let script_path = bin_root.join(script);
            env.write_bytes(&script_path, contents.as_bytes()).await?;
            ensure!(
                env.execute(log, &id, &bin_root, &format!("chmod +x {script}"))
                    .await?,
                error::ExecutableSnafu { script }
            );
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            // Staged assets
            let rpm_root = vfs.entry("rpms").await;
            let kits_root = vfs.entry("kits").await;
            let images_root = vfs.entry("images").await;
            let bin_root = vfs.entry("bin").await;

            // Working area
            let build_root = vfs.create_dir("build-root").await?;

            let mut args = vec![
                format!("--package-dir={:?}", rpm_root.path()),
                format!("--output-dir={:?}", build_root.path()),
                format!("--output-fmt={}", self.options.image.format),
                // TODO: format!("--external-kits-path={:?}", kits_root.path()),
                format!("--os-image-size-gib={}", self.options.image.os_size.unwrap_or(2)),
                format!("--data-image-size-gib={}", self.options.image.data_size.unwrap_or(1)),
                format!("--partition-plan={}", self.options.image.partitions),
                // TODO: format!("--ovf-template="),
                format!("--guest-images={:?}", images_root.path()),
                // TODO: format!("--sbom-package-dir={}"),
            ];
            args.append(&mut self.options.image.features.to_args());

            // Now we run rpm2img :D
            vfs.command("rpm2img", "bin/rpm2img", args).await?;

            // We then just archive the output in build_root!
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(build_root.path(), writer.clone()).await?;
            let layer = ctx.storage()
                .safe_finish_layer(&writer, &LayerOptions::builder()
                .media_type(MediaType::Tar(Compression::None))
                .build()).await?;
            let mut artifact = Artifact::builder()
                .config(
                    Config::builder()
                        .id(id.clone())
                        .build()
                )
                .media_type(MediaType::Manifest)
                .layers(vec![layer])
                .build();
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact.clone())
        }.await {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e.into())
        }
    }

    fn can_shell(&self) -> bool {
        true
    }

    fn shell(&self, env: &Environment) -> TransformResult<()> {
        env.shell(Path::new("."))?;
        Ok(())
    }
}

pub mod error {
    use edo::context::{Addr, ContextError};
    use edo::transform::{Transform, TransformError};
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: ContextError },
        #[snafu(display("missing dependency {addr}"))]
        Depend { addr: Addr },
        #[snafu(display("failed to make {script} executable"))]
        Executable { script: String },
        #[snafu(transparent)]
        Environment {
            source: edo::environment::EnvironmentError,
        },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },
    }

    impl From<Error> for TransformError {
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
