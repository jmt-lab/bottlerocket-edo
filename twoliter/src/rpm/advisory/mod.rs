///! Advisory Checker Transform
///
/// twoliter 0.x: tools/advisory-checker
use std::path::Path;

use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Config, Id, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use snafu::OptionExt;
use tracing::trace;

use crate::rpm::advisory::checker::validate_advisories;

mod checker;
mod error;
mod model;

pub struct AdvisoryCheckTransform {
    addr: Addr,
    source: Source,
}

#[async_trait]
impl FromElement for AdvisoryCheckTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let source = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .and_then(|x| x.get("default"))
            .and_then(|x| x.first())
            .context(error::NoSourceSnafu)?;

        let source = ctx.add_source(source).await?;
        Ok(Self {
            addr: element.addr.clone(),
            source,
        })
    }
}

#[async_trait]
impl TransformImpl for AdvisoryCheckTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(Addr::parse("//default")?)
    }

    async fn get_unique_id(&self, _ctx: &Handle) -> TransformResult<Id> {
        // We share digests directly with the source because it is 1:1 change relationship
        let source_id = self.source.get_unique_id().await?;

        Ok(Id::builder()
            .name(self.addr.to_id())
            .digest(source_id.digest().clone())
            .build())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(Vec::default())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        self.source.cache(log, ctx.storage()).await?;
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        let source_id = self.source.get_unique_id().await?;
        let has_source = ctx.storage().has_local(&source_id).await?;
        Ok(!has_source)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let build_root = Path::new("build-root");
        env.create_dir(build_root).await?;
        let source_id = self.source.get_unique_id().await?;

        trace!(
            subsystem = "advisory",
            component = "checker",
            op = "staging",
            "staging {source_id}"
        );

        env.stage(
            ctx,
            ArtifactStageOptions::builder()
                .id(source_id)
                .path(build_root)
                .build(),
        )
        .await?;

        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            let build_root = vfs.entry("build-root").await;
            if !build_root.try_exists("packages").await?
                || !build_root.try_exists("advisories").await?
            {
                return error::InvalidSetupSnafu {}.fail();
            }
            let packages = build_root.entry("packages").await;
            let advisories = build_root.entry("advisories").await;

            // Run the advisory checker
            validate_advisories(&advisories, &packages).await?;

            // If we get here it passed so we want to record our meta artifact
            let artifact = Artifact::builder()
                .config(Config::builder().id(id).build())
                .media_type(MediaType::Manifest)
                .build();
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, error::Error>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            Err(e) => TransformStatus::Failed(Some(log.path()), e.into()),
        }
    }

    fn can_shell(&self) -> bool {
        false
    }

    fn shell(&self, _env: &Environment) -> TransformResult<()> {
        Ok(())
    }
}
