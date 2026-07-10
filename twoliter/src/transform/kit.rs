use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use ocilot::models::Platform;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use snafu::OptionExt;
use std::path::Path;
use tracing::trace;

#[derive(serde::Deserialize, Debug, Clone)]
pub struct KitOptions {
    name: String,
    version: semver::Version,
    /// Package transforms to pull RPMs from.
    packages: Vec<Addr>,
    /// Other kit transforms in the same project that this kit composes on
    /// top of. Mirrors `LOCAL_KIT_DEPENDENCIES` in the bash script.
    #[serde(default)]
    kit_deps: Vec<Addr>,
    /// Vendor string used when synthesising kit metadata for local kit deps.
    #[serde(default = "default_vendor")]
    vendor: String,
    arch: Option<String>,
}

fn default_vendor() -> String {
    "bottlerocket".to_string()
}

pub struct KitTransform {
    pub addr: Addr,
    pub sdk: Addr,
    pub options: KitOptions,
    /// Resolved SDK image source. Required: a kit transform always declares
    /// exactly one source pointing at the same image addr the SDK
    /// environments consume (e.g. `//bottlerocket-sdk`). We read this
    /// source's artifact to populate the `sdk` field of the kit metadata.
    pub sdk_source: Source,
}

#[async_trait]
impl FromElement for KitTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let mut options: KitOptions = element.get()?;
        options.arch = Some(
            ctx.args()
                .get("arch")
                .cloned()
                .or(options.arch)
                .unwrap_or(std::env::consts::ARCH.to_string()),
        );

        // Extract the lone SDK source from the element's `source` field.
        // We accept either the default scope or any single-scope mapping
        // so config can write `source = "//bottlerocket-sdk"`.
        let scope_table = element
            .source
            .as_ref()
            .and_then(|x| x.get_resolved())
            .cloned()
            .unwrap_or_default();
        let sdk_elements: Vec<_> = scope_table.into_values().flatten().collect();
        let sdk_element = match sdk_elements.as_slice() {
            [single] => single,
            [] => return error::MissingSdkSourceSnafu.fail(),
            _ => return error::TooManySdkSourcesSnafu.fail(),
        };
        let sdk_source = ctx.add_source(sdk_element).await?;

        Ok(Self {
            addr: element.addr.clone(),
            sdk: element
                .environment
                .clone()
                .unwrap_or(Addr::parse("//default")?),
            options,
            sdk_source,
        })
    }
}

#[async_trait]
impl TransformImpl for KitTransform {
    async fn environment(&self) -> TransformResult<Addr> {
        Ok(self.sdk.clone())
    }

    /// Computes a unique content-address ID for the kit using
    /// the packages and kit deps being built.
    async fn get_unique_id(&self, ctx: &Handle) -> TransformResult<Id> {
        let mut hash = Sha256::new();

        // Hash package deps and kit deps deterministically by address.
        let mut deps = self.options.packages.clone();
        deps.extend(self.options.kit_deps.iter().cloned());
        deps.sort();
        for dep in deps.iter() {
            let dep_id = ctx
                .get(dep)
                .context(error::DependSnafu { addr: dep.clone() })?
                .get_unique_id(ctx)
                .await?;
            hash.update(dep_id.digest().as_bytes());
        }
        // Fold in the SDK source identity so a new SDK image triggers a
        // rebuild of the kit (its embedded metadata depends on it).
        let sdk_id = self.sdk_source.get_unique_id().await?;
        hash.update(sdk_id.digest().as_bytes());

        // Fold in the kit's own identity so two distinct kits don't alias
        // when they consume the same set of packages.
        hash.update(self.options.name.as_bytes());
        hash.update(self.options.version.to_string().as_bytes());

        let hash_bytes = hash.finalize();
        let digest = base16::encode_lower(hash_bytes.as_slice());
        let id = Id::builder()
            .name(self.options.name.clone())
            .digest(digest.to_lowercase())
            .version(self.options.version.clone())
            .maybe_arch(self.options.arch.clone())
            .build();
        trace!(component = "transform", type = "kit", "id is calculated to be {id}");
        Ok(id.clone())
    }

    async fn depends(&self) -> TransformResult<Vec<Addr>> {
        Ok(self
            .options
            .packages
            .iter()
            .chain(self.options.kit_deps.iter())
            .cloned()
            .collect())
    }

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // Ensure the SDK source artifact is in the local cache so
        // `transform()` can open it to read `provides`.
        self.sdk_source.cache(log, ctx.storage()).await?;
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        Ok(!self.sdk_source.is_cached(ctx.storage()).await?)
    }

    async fn stage(&self, _log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        // Each package transform produces an artifact whose top-level
        // layout is `Packages/<pkg>/<arch>/...rpm` (see `package.rs`).
        // Stage them all into a shared build-root.
        let build_root = Path::new("build-root");
        env.create_dir(build_root).await?;

        for pkg in self.options.packages.iter() {
            let t = ctx
                .get(pkg)
                .context(error::DependSnafu { addr: pkg.clone() })?;
            let id = t.get_unique_id(ctx).await?;
            trace!(component = "transform", type = "kit", "staging dependency {pkg} with id {id}");
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(id)
                    .path(build_root)
                    .build(),
            )
            .await?;
        }

        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let architecture = self
                .options
                .arch
                .clone()
                .unwrap_or(std::env::consts::ARCH.to_string());

            let vfs = Vfs::new(&id, env, log).await?;
            let build_root = vfs.entry("build-root").await;
            let install_root = vfs.create_dir("install-root").await?;
            let install_pkgs = install_root.create_dir("Packages").await?;

            // Step 1: For each package, copy non-debug arch-matching RPMs into
            // install-root/Packages/<pkg>/. The package transform already
            // emits a Packages/<pkg>/ tree, but it may include debuginfo/
            // debugsource RPMs and other arches — filter explicitly here so
            // the kit only ships what we intend to publish.
            let package_names = self.discover_package_names(&build_root).await?;
            for pkg_name in package_names.iter() {
                let src_dir = build_root.entry(format!("Packages/{pkg_name}")).await;
                if !build_root
                    .try_exists(format!("Packages/{pkg_name}"))
                    .await?
                {
                    // Package staged nothing for this arch; skip.
                    continue;
                }
                let dst_dir = install_pkgs.create_dir(pkg_name).await?;
                vfs.command(
                    &format!("collect-{pkg_name}"),
                    "find",
                    &[
                        src_dir.as_ref(),
                        "-mindepth 1",
                        "-name",
                        &format!("\"*.{architecture}.rpm\""),
                        "!",
                        "-name",
                        "'*-debuginfo-*'",
                        "!",
                        "-name",
                        "'*-debugsource-*'",
                        "-size",
                        "+0c",
                        "-exec",
                        "install",
                        "-p",
                        "-m",
                        "0644",
                        "-t",
                        dst_dir.as_ref(),
                        "{}",
                        "+",
                    ],
                )
                .await?;
            }

            // Step 2: Generate repodata over the assembled package set.
            let install_root_path: &str = install_root.as_ref();
            vfs.command("createrepo", "createrepo_c", &[install_root_path])
                .await?;

            // Step 3: Sanity-check the repository with dnf.
            vfs.command(
                "repoquery",
                "dnf",
                &[
                    "--disablerepo",
                    "'*'",
                    &format!(
                        "--repofrompath \"kit,file://{}\"",
                        install_root.path().display()
                    ),
                    "repoquery",
                ],
            )
            .await?;

            // Step 4: Build the kit metadata document. We read the SDK's
            // descriptor from the SDK image artifact's `provides` (the
            // image URI, set by the OCI image source) and merge in
            // entries synthesised from this kit's local kit deps.
            let kit_metadata = self.build_kit_metadata(ctx, &package_names).await?;

            // Step 5: Materialise edo layers — one per package directory plus
            // one for the shared repodata directory. Each layer carries a
            // path_hint so consumers stage it back to its original location.
            let mut artifact = Artifact::builder()
                .config(
                    Config::builder()
                        .id(id.clone())
                        .metadata(kit_metadata)
                        .build(),
                )
                .media_type(MediaType::Manifest)
                .build();

            for pkg_name in package_names.iter() {
                if !install_root
                    .try_exists(format!("Packages/{pkg_name}"))
                    .await?
                {
                    continue;
                }
                // We create the layer archive manually to control the path
                let layer_aname = format!("layer-{pkg_name}");
                let layer_archive = install_root.entry(&format!("{layer_aname}.tar")).await;
                install_root
                    .command(
                        &layer_aname,
                        "tar",
                        &[
                            "-cvf",
                            layer_archive.as_ref(),
                            "--sort=name",
                            "-C",
                            install_root.as_ref(),
                            &format!("Packages/{pkg_name}"),
                        ],
                    )
                    .await?;
                // Now start the artifact layer
                let writer = ctx.storage().safe_start_layer().await?;
                env.read_stream(layer_archive.path(), writer.clone())
                    .await?;
                // Now finish the layer and ensure the mediatype is the oci mediatype
                let layer = ctx
                    .storage()
                    .safe_finish_layer(
                        &writer,
                        &LayerOptions::builder()
                            .media_type(MediaType::Tar(Compression::None))
                            .build(),
                    )
                    .await?;
                artifact.layers_mut().push(layer);
            }

            // Now we need to store the repodata folder
            let repo_name = "repodata.tar";
            let repo_path = install_root.entry(repo_name).await;
            install_root
                .command(
                    repo_name,
                    "tar",
                    &[
                        "-cvf",
                        repo_path.as_ref(),
                        "--sort=name",
                        "-C",
                        install_root.as_ref(),
                        "repodata",
                    ],
                )
                .await?;
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(repo_path.path(), writer.clone()).await?;
            let layer = ctx
                .storage()
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(Compression::None))
                        .build(),
                )
                .await?;
            artifact.layers_mut().push(layer);

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

impl KitTransform {
    /// Enumerate package directories under `build-root/Packages/` in the
    /// staged build environment. Falls back to the addresses configured on
    /// the transform if the staged layout is empty for some reason.
    async fn discover_package_names(&self, build_root: &Vfs) -> TransformResult<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        if build_root.try_exists("Packages").await? {
            // `ls -1` is portable enough for our SDK container. We feed the
            // output to `cat` so `vfs.output` doesn't see ls's color/quote
            // escapes when run from a TTY-detecting shell.
            let raw = build_root
                .output("list-packages", "ls", &["-1", "Packages"])
                .await?;
            let text = String::from_utf8_lossy(&raw);
            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    names.push(trimmed.to_string());
                }
            }
        }
        if names.is_empty() {
            // Fallback: derive package names from the configured addresses,
            // using the address's terminal id.
            for addr in self.options.packages.iter() {
                names.push(addr.to_id());
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Assemble the `dev.bottlerocket.kit.v3` JSON document.
    ///
    /// The SDK descriptor is built from the SDK image source's stored
    /// artifact: the OCI image source records the image URI under
    /// `Config.provides`, and the URI's tag is the SDK version. The kit
    /// array mirrors the bash projection: local kit deps first, then any
    /// inherited SDK chain (currently empty — the bash inherited from
    /// EXTERNAL_KIT_METADATA, which no longer exists in this flow).
    async fn build_kit_metadata(
        &self,
        ctx: &Handle,
        package_names: &[String],
    ) -> TransformResult<Value> {
        let sdk_id = self.sdk_source.get_unique_id().await?;
        let sdk_artifact = ctx.storage().safe_open(&sdk_id).await?;
        let sdk_uri = sdk_artifact
            .config()
            .provides()
            .iter()
            .next()
            .cloned()
            .context(error::MissingSdkProvidesSnafu)?;
        let (sdk_name, sdk_version) = split_image_uri(&sdk_uri);
        let sdk_descriptor = json!({
            "digest": sdk_id.digest(),
            "name": &sdk_name.split("/").last().unwrap(),
            "source": sdk_uri,
            "vendor": "bottlerocket",
            "version": &sdk_version.strip_prefix("v").unwrap_or(&sdk_version),
        });

        // Synthesize entries for in-project kit dependencies. The kit
        // transform's address-tail is the kit name by convention
        // (see `config/kit.rs`), so we use that directly rather than
        // re-resolving an artifact.
        let mut local_entries = Vec::with_capacity(self.options.kit_deps.len());
        for dep in self.options.kit_deps.iter() {
            // Confirm the kit dep is wired up; surface a clear error if not.
            ctx.get(dep)
                .context(error::DependSnafu { addr: dep.clone() })?;
            local_entries.push(json!({
                "name": dep.to_id(),
                "version": self.options.version.to_string(),
                "vendor": self.options.vendor,
            }));
        }

        Ok(json!({
            "name": self.options.name,
            "version": self.options.version.to_string(),
            "sdk": sdk_descriptor,
            "kit": local_entries,
            // Not in the original bash template, but useful for downstream
            // tooling that wants to enumerate kit contents without parsing
            // layers.
            "packages": package_names,
        }))
    }
}

/// Split an OCI image URI into `(name, version)`.
///
/// Accepts both tag-form (`registry/repo:tag`) and digest-form
/// (`registry/repo@sha256:...`). The "name" returned is the repository
/// path (everything before the tag/digest separator) — the registry host
/// is left intact because downstream consumers may need it to re-pull.
/// If no separator is present the entire URI is returned as the name with
/// an empty version.
fn split_image_uri(uri: &str) -> (String, String) {
    // Prefer digest separator; fall back to the last `:` after the last
    // `/` to avoid mis-splitting `host:port/repo`.
    if let Some((name, digest)) = uri.split_once('@') {
        return (name.to_string(), digest.to_string());
    }
    if let Some(slash_idx) = uri.rfind('/') {
        if let Some(rel_colon) = uri[slash_idx..].rfind(':') {
            let split_at = slash_idx + rel_colon;
            return (uri[..split_at].to_string(), uri[split_at + 1..].to_string());
        }
    } else if let Some((name, tag)) = uri.split_once(':') {
        return (name.to_string(), tag.to_string());
    }
    (uri.to_string(), String::new())
}

/// Helper: tar `path` (a directory inside `env`) into a fresh edo layer and
/// return the finalised [`Layer`]. The layer is stored uncompressed; consumers
/// that want compressed transport should re-encode at publish time.
async fn write_dir_layer(
    ctx: &Handle,
    env: &Environment,
    path: &Path,
    platform: &Platform,
) -> TransformResult<edo::storage::Layer> {
    let writer = ctx.storage().safe_start_layer().await?;
    env.read_stream(path, writer.clone()).await?;
    let layer = ctx
        .storage()
        .safe_finish_layer(
            &writer,
            &LayerOptions::builder()
                .media_type(MediaType::Tar(Compression::None))
                .platform(platform.clone())
                .build(),
        )
        .await?;
    Ok(layer)
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
        Context { source: ContextError },
        #[snafu(display("dependent package not found: {addr}"))]
        Depend { addr: Addr },
        #[snafu(display(
            "kit transform requires exactly one source pointing at the SDK image (the same source the SDK environments consume); none was declared"
        ))]
        MissingSdkSource,
        #[snafu(display(
            "kit transform requires exactly one source pointing at the SDK image; multiple sources were declared"
        ))]
        TooManySdkSources,
        #[snafu(display(
            "SDK image source artifact has no `provides` URI from which to extract the SDK version"
        ))]
        MissingSdkProvides,
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
