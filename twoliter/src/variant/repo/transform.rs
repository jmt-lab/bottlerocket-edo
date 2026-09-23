use crate::kit::apply_target_arch;
use crate::variant::VariantOptions;
use crate::variant::repo::metadata::BuildMetadata;
use async_trait::async_trait;
use edo::storage::{Artifact, Compression, LayerOptions, MediaType};
use edo::transform::TransformError;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs, shell_quote, shell_quote_path},
    source::Source,
    storage::{ArtifactStageOptions, Digest, Id},
    transform::{TransformImpl, TransformResult, TransformStatus},
};
use ocilot::models::Platform;
use snafu::OptionExt;
use std::path::Path;
use tracing::trace;

pub struct RepoTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub kits: Vec<Source>,
    pub options: VariantOptions,
}

#[async_trait]
impl FromElement for RepoTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let options: VariantOptions = element.get()?;
        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();

        let mut kits = Vec::new();
        if let Some(kit_list) = scope_table.get("kits") {
            for kit_element in kit_list {
                let scoped = apply_target_arch(kit_element, &options.arch);
                kits.push(ctx.add_source(&scoped).await?);
            }
        }

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            kits,
            options,
        })
    }
}

#[async_trait]
impl TransformImpl for RepoTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        // The rootfs id is calculated using anything and everything that could mutate the image
        let mut digest = Digest::builder();

        // (1) All packages served by the external kits
        for kit in self.kits.iter() {
            let id = kit.get_unique_id().await?;
            digest.update(id.digest().hash());
        }
        // (2) Packages served locally
        for package in self.options.depends.iter() {
            let t = ctx.get(package).context(error::NotFoundSnafu {
                addr: package.clone(),
            })?;
            let id = t.get_unique_id(ctx).await?;
            digest.update(id.digest().hash());
        }

        // (2) Configuration options in VariantOptions that affect the RootFs
        for name in self.options.os.packages.iter() {
            digest.update(name.as_bytes());
        }
        let bitset = &self.options.image.features.as_bytes();
        digest.update(bitset);

        let digest = digest.build();
        let id = Id::builder()
            .name(format!("{}-repo", self.options.name))
            .digest(digest)
            .version(self.options.version.clone())
            .arch(self.options.arch.to_string())
            .build();
        trace!(
            component = "transform",
            type = "repo",
            "id is calculated to be {id}"
        );
        Ok(id)
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

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        for kit in self.kits.iter() {
            kit.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        for kit in self.kits.iter() {
            if !kit.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let rpm_root = Path::new("deps/rpms");
        // Stage each kit's packages into our rpm root
        env.create_dir(rpm_root).await?;
        for kit in self.kits.iter() {
            let id = kit.get_unique_id().await?;
            trace!(
                component = "transform",
                type = "repo",
                "staging kit packages from kit with id {id}"
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

        // Stage any packages coming from transforms into the rpm root
        for pkg in self.options.depends.iter() {
            let t = ctx
                .get(pkg)
                .context(error::NotFoundSnafu { addr: pkg.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(
                component = "transform",
                type = "repo",
                "staging local package {pkg} with id {id}"
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

        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            let rpm_root = vfs.entry("deps/rpms").await;
            let install_root = vfs.create_dir("install-root").await?;
            // SBOM RPMs live in a `sbom-rpms/` subdirectory of the layer we
            // ship, so the rpm2img transform can stage the whole layer as
            // `rpms/` and address the SBOM set as `rpms/sbom-rpms/`. Old
            // twoliter kept them in a sibling `/local/sbom-rpms/` directory
            // (`build.Dockerfile:320-337`); we nest instead of siblings so
            // a single tarball artifact carries both sets.
            let sbom_rpms_root = install_root.create_dir("sbom-rpms").await?;
            let build_root = vfs.create_dir("build-root").await?;

            // (1) Build the per-variant metadata RPM.
            let metadata_workdir = build_root.create_dir("metadata").await?;
            let metadata_builder = BuildMetadata::builder()
                .variant(self.options.name.clone())
                .arch(self.options.arch.clone())
                .features(self.options.image.features.clone())
                .workdir(metadata_workdir)
                .output(rpm_root.clone())
                .build();
            metadata_builder.build().await?;

            // (2) Install the rpm packages to build the rootfs

            // (2a) Index the rpms being used
            vfs.command(
                "createrepo_rpms",
                "createrepo_c",
                &[
                    "-o",
                    rpm_root.as_ref(),
                    "-x",
                    "'*-debuginfo-*.rpm'",
                    "-x",
                    "'*-debugsource-*.rpm'",
                    "--no-database",
                    rpm_root.as_ref(),
                ],
            )
            .await?;

            // (2b) Use dnf install downloadonly to create a filtered
            // repository for installation, resolving conflicts.
            //
            // The install list mirrors `build.Dockerfile:334`:
            // `bottlerocket-metadata` + `bottlerocket-metadata-sbom` +
            // every requested variant package. `-metadata-sbom` is the
            // SBOM subpackage emitted by our own `metadata.spec`; kits
            // ship their own `-sbom` subpackages which dnf pulls in
            // transitively through `Requires:` on the main packages.
            let package_dir = shell_quote_path(rpm_root.path());
            let repofrompath = format!("--repofrompath=local,{package_dir}");
            let forcearch = format!("--forcearch={}", self.options.arch);
            let setopt = format!(
                "--setopt=cachedir={}",
                shell_quote_path(install_root.path())
            );
            let mut argv: Vec<String> = vec![
                "--assumeyes".into(),
                "--disablerepo=*".into(),
                repofrompath,
                "--enablerepo=local".into(),
                "--nogpgcheck".into(),
                forcearch,
                setopt,
                "install".into(),
                "--downloadonly".into(),
                "bottlerocket-metadata".into(),
                "bottlerocket-metadata-sbom".into(),
            ];
            for name in self.options.os.packages.iter() {
                let prefixed = format!("bottlerocket-{name}");
                argv.push(prefixed);
            }
            vfs.command("dnf_resolve", "dnf", argv).await?;

            // (2c) Partition the download by filename convention. Mirrors
            // `build.Dockerfile:335-337` but with `sbom-rpms/` nested inside
            // `install_root` rather than a sibling:
            //
            //   - every SBOM subpackage moves to `install_root/sbom-rpms/`
            //   - every other `*.rpm` is flattened into `install_root/`
            //   - non-rpm files (dnf's repo cache junk) are removed
            //
            // `-mindepth 2` skips files already at the top level (which is
            // also how we avoid re-hitting the `sbom-rpms/` subdirectory:
            // once SBOM rpms land there they are exactly at depth 2, so the
            // subsequent `-name '*.rpm'` flatten pass would sweep them back
            // into `install_root/`). Prune that subdirectory explicitly on
            // both the flatten and cleanup passes.
            //
            // SBOM-subpackage glob: `<name>-sbom-<version>-<release>.<arch>.rpm`.
            // Old-twoliter used `*-sbom-*.rpm` (`build.Dockerfile:335`), which
            // would false-match a hypothetical future package literally named
            // e.g. `foo-sbom-bar`. Tighten to `*-sbom-[0-9]*.rpm` — the char
            // after the `sbom-` separator is the first digit of the RPM
            // Version field, which is a hard rpmbuild constraint (Version
            // must start with a digit). No legitimate sbom subpackage is
            // excluded, and no legitimate main package with a `-sbom-`
            // literal in its stem is included.
            //
            // Naming these three under separate `vfs.command` actions keeps
            // each dnf-cache-partition step legible in the log and lets a
            // shell error point at a specific stage.
            let install_root_str: &str = install_root.as_ref();
            let sbom_rpms_str: &str = sbom_rpms_root.as_ref();
            vfs.command(
                "sort_sbom_rpms",
                "find",
                &[
                    install_root_str,
                    "-mindepth",
                    "2",
                    "-type",
                    "f",
                    "-name",
                    "'*-sbom-[0-9]*.rpm'",
                    "-not",
                    "-path",
                    &format!("'{sbom_rpms_str}/*'"),
                    "-exec",
                    "mv",
                    "-t",
                    sbom_rpms_str,
                    "{}",
                    "+",
                ],
            )
            .await?;
            vfs.command(
                "flatten_rpms",
                "find",
                &[
                    install_root_str,
                    "-mindepth",
                    "2",
                    "-type",
                    "f",
                    "-name",
                    "'*.rpm'",
                    "-not",
                    "-path",
                    &format!("'{sbom_rpms_str}/*'"),
                    "-exec",
                    "mv",
                    "-t",
                    install_root_str,
                    "{}",
                    "+",
                ],
            )
            .await?;
            // Cleanup: after `sort_sbom_rpms` and `flatten_rpms` every RPM
            // we care about now lives either at `install_root/*.rpm` (main
            // packages) or `install_root/sbom-rpms/*.rpm` (SBOM subpackages).
            // Everything else is dnf's per-repo cache scaffolding (metadata
            // XML, sqlite dbs, empty `packages/` subdirs) which we drop by
            // removing every top-level entry that is neither `sbom-rpms`
            // nor a `.rpm` file. Reproduces the intent of
            // `build.Dockerfile:337`'s `find ... ! -name '*.rpm' -delete`
            // without accidentally recursing into `sbom-rpms/`.
            vfs.command(
                "clean_cache_junk",
                "find",
                &[
                    install_root_str,
                    "-mindepth",
                    "1",
                    "-maxdepth",
                    "1",
                    "!",
                    "-name",
                    "'*.rpm'",
                    "!",
                    "-name",
                    "'sbom-rpms'",
                    "-exec",
                    "rm",
                    "-rf",
                    "{}",
                    "+",
                ],
            )
            .await?;

            // Now we store the result as a single tarball containing the filtered packages for this variant
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(install_root.path(), writer.clone()).await?;

            let layer = ctx
                .storage()
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(Compression::None))
                        .platform(
                            Platform::builder()
                                .os("linux")
                                .architecture(self.options.arch.to_string())
                                .build(),
                        )
                        .build(),
                )
                .await?;

            let artifact = Artifact::builder()
                .config(edo::storage::Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .layers(vec![layer])
                .build();
            ctx.storage().safe_save(&artifact).await?;

            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e.into()),
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
    use edo::transform::TransformError;
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(transparent)]
        Environment {
            source: edo::environment::EnvironmentError,
        },
        #[snafu(display("transform dependency not found with addr: {addr}"))]
        NotFound { addr: Addr },
        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },
        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },
        #[snafu(display("stub"))]
        Stub,
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
