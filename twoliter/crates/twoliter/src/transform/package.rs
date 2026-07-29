use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use edo::storage::LayerOptions;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Compression, Config, Id, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
    util::Reader,
};
use ocilot::models::Platform;
use sha2::Digest;
use sha2::Sha256;
use snafu::OptionExt;
use tracing::trace;

/// Dummy spec that `Provides: group(builder)`. RPM 6.0 turned the automatic
/// `group()`/`user()` dependencies generated from file-ownership declarations
/// in `%files` (e.g. kernel-devel subpackages that `chgrp builder` some files)
/// into hard `Requires`. Building this tiny package and adding it to the local
/// repo satisfies that requirement without shipping anything.
///
/// Sourced from `br-image` so `crates/br-image/data/builder-group.spec` is
/// the single point of truth.
const BUILDER_GROUP_SPEC: &str = crate::image::BUILDER_GROUP_SPEC;

#[derive(serde::Deserialize, Debug, Clone)]
pub struct PackageBuildOptions {
    package: String,
    arch: Option<String>,
    #[serde(default)]
    depends: Vec<Addr>,
    #[serde(default)]
    vendored: Vec<Addr>,
    #[serde(default)]
    bundled: BTreeMap<Addr, PathBuf>,
}

pub struct PackageBuildTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: PackageBuildOptions,
    pub kits: Vec<Source>,
    pub archives: Vec<Source>,
    pub sources: Vec<Source>,
}

#[async_trait]
impl FromElement for PackageBuildTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut options: PackageBuildOptions = element.get()?;
        options.arch = Some(
            ctx.args()
                .get("arch")
                .cloned()
                .or(options.arch)
                .unwrap_or(std::env::consts::ARCH.to_string()),
        );
        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();
        let mut kits = Vec::new();
        let mut archives = Vec::new();
        let mut sources = Vec::new();
        if let Some(kit_list) = scope_table.get("kit") {
            for element in kit_list {
                kits.push(ctx.add_source(element).await?);
            }
        }
        if let Some(archive_list) = scope_table.get("archives") {
            for element in archive_list {
                archives.push(ctx.add_source(element).await?);
            }
        }
        if let Some(source_list) = scope_table.get("sources") {
            for element in source_list {
                sources.push(ctx.add_source(element).await?);
            }
        }
        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            kits,
            archives,
            sources,
        })
    }
}

#[async_trait]
impl TransformImpl for PackageBuildTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    /// Computes a unique content-address ID for this package build.
    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let mut hash = Sha256::new();

        // (a) Dependency artifact IDs, sorted by addr for determinism
        let mut depends = self.options.depends.clone();
        depends.sort();
        for depend in depends.iter() {
            // We should use the resolved id for the dependency so we flag
            // rebuild if dependencies chang
            let depend_id = ctx
                .get(depend)
                .context(error::DependSnafu {
                    addr: depend.clone(),
                })?
                .get_unique_id(ctx)
                .await?;
            hash.update(depend_id.digest().as_bytes());
        }
        // (b) Bundled artifact IDs, sorted by addr for determinism
        for depend in self.options.bundled.keys() {
            // We should use the resolved id for the dependency so we flag
            // rebuild if dependencies chang
            let depend_id = ctx
                .get(depend)
                .context(error::DependSnafu {
                    addr: depend.clone(),
                })?
                .get_unique_id(ctx)
                .await?;
            hash.update(depend_id.digest().as_bytes());
        }
        // (c) Kit Sources -> Archives -> Sources
        for source in self
            .kits
            .iter()
            .chain(self.archives.iter())
            .chain(self.sources.iter())
        {
            let id = source.get_unique_id().await?;
            hash.update(id.digest().as_bytes());
        }
        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_slice());
        let id = Id::builder()
            .name(self.addr.to_id())
            .digest(digest.to_lowercase())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(component = "transform", type = "rpm", "id is calculated to be {id}");
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self
            .options
            .vendored
            .iter()
            .chain(self.options.depends.iter())
            .chain(self.options.bundled.keys())
            .cloned()
            .collect())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // Fetch all our sources
        for source in self
            .kits
            .iter()
            .chain(self.archives.iter())
            .chain(self.sources.iter())
        {
            source.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        for source in self
            .kits
            .iter()
            .chain(self.archives.iter())
            .chain(self.sources.iter())
        {
            if !ctx
                .storage()
                .has_local(&source.get_unique_id().await?)
                .await?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // First we want to create our build-root and install-root
        let rpm_root = Path::new("deps");
        let build_root = Path::new("src");
        env.create_dir(rpm_root).await?;
        env.create_dir(build_root).await?;
        env.create_dir(Path::new("install-root")).await?;

        // Dependencies will be other rpms that this package build
        // depends on. They will have been built by edo and are inside archives
        for dep in self.options.depends.iter() {
            let t = ctx
                .get(&dep)
                .context(error::DependSnafu { addr: dep.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(component = "transform", type = "rpm", "staging dependency {dep} with id {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(rpm_root)
                    .build(),
            )
            .await?;
        }

        // Stage all kits into the deps rpm root
        for source in self.kits.iter() {
            let id = source.get_unique_id().await?;
            trace!(component = "transform", type = "kit", "staging source {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(rpm_root)
                    .build(),
            )
            .await?;
        }

        // Archives are going to be third party source tarballs that are not extracted
        for source in self.archives.iter() {
            let id = source.get_unique_id().await?;
            trace!(component = "transform", type = "rpm", "staging source {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(build_root)
                    .extract(false)
                    .decompress(false)
                    .build(),
            )
            .await?;
        }

        // Sources are going to be first party sources
        for source in self.sources.iter() {
            let id = source.get_unique_id().await?;
            trace!(component = "transform", type = "rpm", "staging source {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(build_root)
                    .build(),
            )
            .await?;
        }

        // Stage all vendored dependencies in build_root
        for dep in self.options.vendored.iter() {
            let t = ctx
                .get(&dep)
                .context(error::DependSnafu { addr: dep.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(component = "transform", type = "rpm", "staging vendored dependency {dep} with id {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(build_root)
                    .build(),
            )
            .await?;
        }

        // Stage all bundled source depends as archives
        for (addr, target_file) in self.options.bundled.iter() {
            let t = ctx
                .get(addr)
                .context(error::DependSnafu { addr: addr.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(component = "transform", type = "rpm", "staging bundled dependencies {addr} with id {id}");
            let artifact = ctx.storage().safe_open(&id).await?;
            if let Some(layer) = artifact.layers().first() {
                let reader = ctx.storage().safe_read(layer).await?;
                let (_, compression) =
                    Compression::detect(&target_file.to_string_lossy().as_ref())?;
                let layer_compression = layer.media_type().compression();
                let reader = if layer_compression == compression {
                    // If the compression matches we can just use the reader verbatim
                    reader
                } else if layer_compression == Compression::None {
                    // If the layer is not compressed and target_file should be we add compression
                    Reader::with_compression(reader, &compression)
                } else if compression == Compression::None {
                    // If the target file expects no compression and layer is compressed, we decompress
                    Reader::with_decompression(reader, &layer_compression)
                } else {
                    // If the compression mismatches we have to convert compression (which is inefficient but here we are)
                    let decompress = Reader::with_decompression(reader, &layer_compression);
                    Reader::with_compression(decompress, &compression)
                };
                env.write_stream(&build_root.join(target_file), reader)
                    .await?;
            }
        }

        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;

            let vfs = Vfs::new(&id, env, log).await?;

            let src_root = vfs.entry("src").await;
            let install_root = vfs.create_dir("install-root").await?;

            // Step 1: Create rpmbuild directory tree.
            let rpmbuild = vfs.create_dir("rpmbuild").await?;
            let build = rpmbuild.create_dir("BUILD").await?;
            let rpms = rpmbuild.create_dir("RPMS").await?;
            let sources = rpmbuild.create_dir("SOURCES").await?;
            let specs = rpmbuild.create_dir("SPECS").await?;

            // Step 2: Set architecture template variable from context arguments
            let architecture = self
                .options
                .arch
                .clone()
                .unwrap_or(std::env::consts::ARCH.to_string());

            // Step 3: Copy arch-specific RPM macros for cross compilation
            vfs.copy(
                &format!("/usr/lib/rpm/platform/{architecture}-bottlerocket/macros"),
                ".rpmmacros",
            )
            .await?;
            vfs.copy(
                &format!("/usr/lib/rpm/platform/{architecture}-bottlerocket/macros"),
                "/root/.rpmmacros",
            )
            .await?;

            // Step 4: Copy spec file to rpmbuild/SPECS/
            let specfile = specs.entry(format!("{}.spec", self.options.package)).await;
            vfs.copy(&format!("src/{}.spec", self.options.package), &specfile)
                .await?;

            // Step 5: Copy all non-hidden, non-spec source files to rpmbuild/SOURCES/
            vfs.command(
                "copy_sources",
                "find",
                &[
                    "src",
                    "-maxdepth 1",
                    "-not",
                    "-path '*/\\.*'",
                    "-type f",
                    "-exec",
                    "cp",
                    "{}",
                    sources.as_ref(),
                    "\\;",
                ],
            )
            .await?;

            if src_root.try_exists("sources").await? {
                let sources = build.entry("sources").await;
                vfs.copy("src/sources", &sources).await?;
            }
            if src_root.try_exists(".cargo").await? {
                let cargo = src_root.entry(".cargo").await;
                vfs.copy(cargo, rpmbuild.entry(".cargo").await).await?;
            }

            // Step 6: Copy all dependency rpms
            if vfs.try_exists(&format!("deps/Packages")).await? {
                let pkg_dir = vfs.entry("deps/Packages").await;
                vfs.command(
                    "copy-deps",
                    "cp",
                    &[
                        "-r",
                        &format!("{:?}/*/*{architecture}.rpm", pkg_dir.path()),
                        rpms.as_ref(),
                    ],
                )
                .await?;
            }

            // Step 7: Build a dummy package that Provides: group(builder). RPM 6.0 turned the
            // automatic group()/user() Requires generated from %files ownership declarations
            // (used by e.g. kernel-devel subpackages) into hard requirements. Build it in its
            // own topdir and flatten the result into the shared local repo `rpms` dir (not into
            // `rpms/{architecture}`, which is what Step 11 copies into the final artifact) so
            // it's resolvable by builddep but never shipped.
            let bg_root = vfs.create_dir("builder-group").await?;
            bg_root.create_dir("BUILD").await?;
            let bg_rpms = bg_root.create_dir("RPMS").await?;
            bg_root.create_dir("SOURCES").await?;
            let bg_specs = bg_root.create_dir("SPECS").await?;
            let bg_specfile = bg_specs.entry("builder-group.spec").await;
            vfs.write(&bg_specfile, BUILDER_GROUP_SPEC.as_bytes())
                .await?;

            vfs.command(
                "builder-group",
                "rpmbuild",
                &[
                    "-bb",
                    "--clean",
                    "--undefine _auto_set_build_flags",
                    &format!("--define \"_target_cpu {architecture}\""),
                    &format!("--define \"_topdir {:?}\"", bg_root.path()),
                    bg_specfile.as_ref(),
                ],
            )
            .await?;

            vfs.command(
                "stage-builder-group",
                "cp",
                &[
                    "-r",
                    &format!("{:?}/*/*.rpm", bg_rpms.path()),
                    rpms.as_ref(),
                ],
            )
            .await?;

            // Step 8: Create local repo index over the rpm-root dependencies
            vfs.command(
                "local_repo",
                "createrepo_c",
                &[
                    "-o",
                    rpms.as_ref(),
                    "-x '*-debug-info-*.rpm'",
                    "-x '*-debugsource-*.rpm'",
                    "--no-database",
                    rpms.as_ref(),
                ],
            )
            .await?;

            // Step 9: Install spec-declared BuildRequires via dnf builddep
            vfs.command(
                "builddeps",
                "dnf",
                &[
                    "--assumeyes",
                    "--disablerepo '*'",
                    &format!("--repofrompath local,{:?}", rpms.path()),
                    "--enablerepo 'local'",
                    "--nogpgcheck",
                    "--forcearch",
                    &architecture,
                    "builddep",
                    specfile.as_ref(),
                ],
            )
            .await?;

            // Setup environment variables for the build dist
            let dist_tag = if let Some(config) = ctx.config().get("bottlerocket") {
                let table = config.as_object().context(error::FieldSnafu {
                    field: "bottlerocket",
                    type_: "table",
                })?;
                let timestamp = table
                    .get("build-timestamp")
                    .and_then(|x| x.as_str())
                    .unwrap_or("1000000000");
                let version =
                    if let Some(version) = table.get("release-version").and_then(|x| x.as_str()) {
                        version
                    } else {
                        "0.0.0"
                    };
                format!(".{timestamp}.{version}.br1")
            } else {
                ".0000000000.0.0.0.br1".to_string()
            };

            // Step 10: Run rpmbuild with cross-compilation flags.
            // TAR_OPTIONS=--no-same-owner: upstream source tarballs (e.g.
            // aws-efa-installer) carry uids/gids that are unmapped inside our
            // rootless container, which makes %prep's `tar -xf` fail with
            // EINVAL on chown. The original twoliter SDK build avoided this by
            // running rpmbuild as a non-root user; we run as root, so force
            // tar to skip ownership restore.
            vfs.command(
                "build",
                "env",
                &[
                    "TAR_OPTIONS=--no-same-owner",
                    "rpmbuild",
                    "-bb",
                    "--clean",
                    "--undefine _auto_set_build_flags",
                    &format!("--define \"_target_cpu {architecture}\""),
                    &format!("--define \"dist {dist_tag}\""),
                    &format!("--define \"_topdir {:?}\"", rpmbuild.path()),
                    specfile.as_ref(),
                ],
            )
            .await?;

            // Step 11: Copy built RPMS to the install-root
            let target = install_root
                .create_dir(&format!("Packages/{}", self.options.package))
                .await?;
            // Since dependency rpms get copied to the root of rpmbuild/RPMS and not the architecture specific folder
            // we can simply just copy all rpms in the architecture folder
            vfs.command(
                "install",
                "cp",
                &[
                    "-r",
                    &format!("{:?}/{architecture}/*.rpm", rpms.path(),),
                    target.as_ref(),
                ],
            )
            .await?;

            // Collect the artifact from install-root.
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .build();

            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(Path::new("install-root"), writer.clone())
                .await?;

            artifact.layers_mut().push(
                ctx.storage()
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder()
                            .media_type(MediaType::Tar(Compression::None))
                            .platform(
                                Platform::builder()
                                    .os(std::env::consts::OS)
                                    .architecture(architecture)
                                    .build(),
                            )
                            .build(),
                    )
                    .await?,
            );
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact)
        }
        .await
        {
            Ok(artifact) => TransformStatus::Success(artifact.clone()),
            Err(e) => TransformStatus::Retryable(Some(log.path()), e),
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
    use edo::{
        context::{Addr, ContextError},
        transform::TransformError,
    };
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub enum Error {
        #[snafu(transparent)]
        Context { source: edo::context::ContextError },
        #[snafu(display("dependency not found: {addr}"))]
        Depend { addr: Addr },
        #[snafu(display("package transform expects a field '{field}' with type '{type_}'"))]
        Field { field: String, type_: String },
        #[snafu(display("package transform configuration missing required sdk tag"))]
        SdkMissing,
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

#[cfg(test)]
mod tests {
    use super::BUILDER_GROUP_SPEC;

    #[test]
    fn builder_group_spec_provides_group_builder() {
        assert!(BUILDER_GROUP_SPEC.contains("Provides: group(builder)"));
    }
}
