mod constants;

use std::path::Path;

use crate::{
    kit::apply_target_arch,
    model::Arch, variant::{
        ImageFormat, ImageOptions, OsOptions, PartitionPlan, Variant, VariantOptions, rpm2img::constants::{GUEST_IMAGES_HELPER, IMGHELPER, PARTYPLANNER, RPM2IMG},
    },
};
use async_trait::async_trait;
use edo::{
    context::{Addr, Context, Element, FromElement, Handle, Log},
    environment::{Environment, Vfs},
    source::Source,
    storage::{Artifact, ArtifactStageOptions, Compression, Config, Id, LayerOptions, MediaType},
    transform::{TransformError, TransformImpl, TransformResult, TransformStatus},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use snafu::{OptionExt, ensure};
use tracing::trace;

/// One entry in the resolved `"kits"` source scope.
///
/// We keep the source's `Addr` alongside the `Source` handle because the
/// `Source` trait itself carries no address (see `edo::source::Source`),
/// and the vendor + kit name are encoded structurally in the address —
/// `//kits/<vendor>/<name>` (convention documented in
/// `config/variant.rs::Variant::to_edo` and enforced by the project
/// resolver). We parse those two fields off the addr at stage time to
/// build the on-disk layout `rpm2img.sh` and `imghelper.sh` expect:
/// `<external_kits_path>/<vendor>/<name>/<arch>/`.
pub struct KitEntry {
    pub addr: Addr,
    pub source: Source,
}

impl KitEntry {
    /// Extract `(vendor, name)` from a `//kits/<vendor>/<name>` addr.
    ///
    /// Delegates to [`parse_kit_addr`] on `self.addr` — the split
    /// happens on the addr's component list (via `Addr::to_id`), and
    /// path-unsafe segments (`.`, `..`, empty, embedded `/` or NUL)
    /// are rejected so a malformed but plausible addr surfaces as
    /// `MalformedKitAddr` at stage time rather than silently placing
    /// the kit at an unintended path under `kits/`.
    fn parsed_vendor_name(&self) -> Option<(String, String)> {
        parse_kit_addr(&self.addr)
    }
}

/// Parse a `//kits/<vendor>/<name>` addr into its `(vendor, name)`
/// components. Returns `None` for any shape or content that would
/// either mis-locate the kit on disk or escape the `kits/` root when
/// joined into a filesystem path.
///
/// **Parser shape.** `Addr::parse` (`edo/context/address.rs:87-97`)
/// strips at most one leading `//` and splits the rest on `/` into a
/// component vec; `Addr::to_id()` joins with `/` again with no prefix.
/// So `//kits/bottlerocket/core-kit` round-trips through `to_id()` as
/// `"kits/bottlerocket/core-kit"` (three components). We split that
/// back on `/` and require exactly three segments starting with `kits`.
///
/// **Path-safety.** Both segments feed straight into
/// `Path::join(vendor).join(name)` under `kits/`. We reject:
/// - empty segments (`""`) — from trailing slash / double slash;
/// - `.` and `..` — parent/self path components that would escape
///   the `kits/` root when joined;
/// - segments containing any `/` — defense-in-depth: `Addr::parse`
///   already splits on `/` so a component cannot legitimately contain
///   one, but the guard keeps the invariant local to this function
///   and defends against a hand-built `Addr` that skipped the parser
///   or a future change to the split rule;
/// - segments containing NUL — filesystem APIs reject these anyway,
///   but rejecting here yields a clearer error site.
///
/// Percent-encoding, unicode, and other "unusual but legal" characters
/// pass through verbatim — the addr is a build-graph identifier, not
/// a URL, and edo does not decode it. `//kits/foo%2Fbar/name` becomes
/// a literal vendor directory `foo%2Fbar/`: surprising but not unsafe.
fn parse_kit_addr(addr: &Addr) -> Option<(String, String)> {
    fn segment_is_safe(s: &str) -> bool {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains('/')
            && !s.contains('\0')
    }

    let id = addr.to_id();
    let mut parts = id.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("kits"), Some(vendor), Some(name), None)
            if segment_is_safe(vendor) && segment_is_safe(name) =>
        {
            Some((vendor.to_string(), name.to_string()))
        }
        _ => None,
    }
}

pub struct RPM2ImageTransform {
    pub addr: Addr,
    pub sdk: Addr,
    /// External kits resolved off the element's `"kits"` source scope.
    /// Empty when the project has no `[[kit]]` list. Populated by
    /// [`FromElement::new`] mirroring `RepoTransform`'s wiring.
    pub kits: Vec<KitEntry>,
    pub options: VariantOptions,
}

#[async_trait]
impl FromElement for RPM2ImageTransform {
    type Error = error::Error;

    async fn new(element: &Element, ctx: &Context) -> Result<Self, error::Error> {
        let options: VariantOptions = element.get()?;

        // Resolve the `"kits"` source scope exactly the way
        // `RepoTransform::new` does: each kit element is scoped to
        // the target arch (so the kit-image source picks the right
        // per-platform manifest out of the multi-arch index) and then
        // registered as an edo `Source`.
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
                let source = ctx.add_source(&scoped).await?;
                kits.push(KitEntry {
                    addr: kit_element.addr.clone(),
                    source,
                });
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

        // (1b) All external kits — their content flows into the image's
        // application-inventory rewrite, so a new kit release must
        // invalidate the image cache.
        for kit in self.kits.iter() {
            let id = kit.source.get_unique_id().await?;
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

    async fn prepare(&self, log: &Log, ctx: &Handle) -> TransformResult<()> {
        // Ensure every external kit's artifact is present in the local
        // cache before `stage()` tries to extract them. Mirrors
        // `RepoTransform::prepare` — kits are the only network-fetched
        // inputs to the image stage, so nothing else needs to run here.
        for kit in self.kits.iter() {
            kit.source.cache(log, ctx.storage()).await?;
        }
        Ok(())
    }

    async fn needs_prepare(&self, ctx: &Handle) -> TransformResult<bool> {
        // Any un-cached kit forces the fetch phase. When every kit is
        // already local (or the project has no kits), the scheduler
        // can skip spawning `prepare` altogether.
        for kit in self.kits.iter() {
            if !kit.source.is_cached(ctx.storage()).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn stage(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformResult<()> {
        let id = self.get_unique_id(ctx).await?;
        let rpm_root = Path::new("rpms");
        let bin_root = Path::new("bin");
        let images_root = Path::new("images");
        let kits_root = Path::new("kits");
        env.create_dir(rpm_root).await?;
        env.create_dir(bin_root).await?;
        env.create_dir(images_root).await?;
        env.create_dir(kits_root).await?;

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

        // Stage each external kit into `kits/<vendor>/<name>/<arch>/`
        // and emit `kits/external-kit-metadata.json`, matching the
        // layout expected by `imghelper.sh::generate_application_inventory`
        // (see `imghelper.sh:879-902`). The kit tarball layers place
        // `Packages/<pkg>/*.rpm` and `repodata/*` at their root
        // (`kit/transform.rs:290-340`), so staging into the per-kit
        // subdirectory produces exactly the tree the shell then feeds
        // to `dnf --repofrompath kit,file://<kit_path>`.
        //
        // We track the parsed `(vendor, name, version)` triples as we
        // go so a single pass builds both the on-disk layout and the
        // metadata document — no separate scan of the artifact tree.
        let arch_str = self.options.arch.to_string();
        let mut kit_metadata_entries: Vec<Value> = Vec::with_capacity(self.kits.len());
        for kit in self.kits.iter() {
            let (vendor, name) = kit.parsed_vendor_name().context(error::MalformedKitAddrSnafu {
                addr: kit.addr.clone(),
            })?;
            let kit_id = kit.source.get_unique_id().await?;
            // Read the source artifact's config metadata for the version
            // string. `kit-image` sources flatten the decoded
            // `KitMetadata` label onto `Config::metadata` under `"version"`
            // (see `kit/sources/kit_image.rs::flatten_kit_metadata`); the
            // locally-built `KitTransform` writes the same key
            // (`kit/transform.rs::build_kit_metadata`). Fall back to the
            // artifact `Id`'s version so a kit whose metadata was
            // pre-stripped still lands with something reasonable in the
            // JSON — the shell only reads `.kit[].version` for logging /
            // debugging, not for path resolution.
            let kit_artifact = ctx.storage().safe_open(&kit_id).await?;
            let version = kit_artifact
                .config()
                .metadata()
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| kit_id.version().map(|v| v.to_string()))
                .unwrap_or_default();

            let dest = kits_root.join(&vendor).join(&name).join(&arch_str);
            env.create_dir(&dest).await?;
            let kit_addr = &kit.addr;
            let dest_display = dest.display();
            trace!(
                component = "transform",
                type = "rpm2img",
                "staging kit {kit_addr} with id {kit_id} into {dest_display}",
            );
            env.stage(
                ctx,
                ArtifactStageOptions::builder()
                    .id(kit_id)
                    .path(&dest)
                    .build(),
            )
            .await?;

            kit_metadata_entries.push(json!({
                "name": name,
                "vendor": vendor,
                "version": version,
            }));
        }

        // Emit the `external-kit-metadata.json` sidecar the shell
        // reads via `jq -r ".kit[].name"` to enumerate kits (see
        // `imghelper.sh:880`). Only the `kit[]` array is consumed
        // downstream — old-twoliter also wrote `sdk` and
        // `project_vendor` fields (`twoliter/src/project/lock/mod.rs:41-48`)
        // but nothing in `imghelper.sh` reads them, so we omit
        // those here rather than fabricate values the source
        // artifact does not carry. A future consumer that needs them
        // should thread the SDK source through the image transform
        // the way `RepoTransform` already does.
        let metadata_doc = json!({ "kit": kit_metadata_entries });
        let metadata_bytes = serde_json::to_vec(&metadata_doc).map_err(|e| error::Error::Json {
            source: e,
        })?;
        let metadata_path = kits_root.join("external-kit-metadata.json");
        env.write_bytes(&metadata_path, &metadata_bytes).await?;

        // Now Stage and make executable our scripts that drive the transform
        for (script, contents) in [
            ("partyplanner", PARTYPLANNER),
            ("imghelper", IMGHELPER),
            ("rpm2img", RPM2IMG),
            ("guest-images-helper", GUEST_IMAGES_HELPER),
        ] {
            let script_path = bin_root.join(script);
            env.write_bytes(&script_path, contents.as_bytes()).await?;
            ensure!(
                env.execute(log, &id, &bin_root, &format!("chmod +x {script}"))
                    .await?,
                error::ExecutableSnafu { script }
            );
        }

        // Stage the OVF template if the variant has one on disk.
        //
        // Old-twoliter always passed `--ovf-template=/bypass/variants/<name>/template.ovf`
        // via a live bypass mount (`build.Dockerfile:442`), whether or not
        // the file existed. Non-VMDK variants got a path that pointed at
        // nothing; `imghelper.sh::sanity_checks` only `-s`-checks the file
        // when `output_fmt == "vmdk"` (imghelper.sh:95), so nothing broke.
        //
        // The new transform has no bypass mount, so we must materialize the
        // file into the container ourselves. When the source path exists on
        // host, copy its bytes into `bin/template.ovf`; otherwise skip. The
        // transform step below unconditionally passes the container path
        // `bin/template.ovf` regardless -- non-VMDK builds still get a
        // non-empty `--ovf-template=` argument (satisfying imghelper's
        // `${3:?}` positional guard), and VMDK builds that lack a template
        // fail loudly at the `-s` check with a proper error message.
        if let Some(host_path) = self.options.image.ovf_template.as_ref() {
            match tokio::fs::read(host_path).await {
                Ok(bytes) => {
                    let container_path = bin_root.join("template.ovf");
                    env.write_bytes(&container_path, &bytes).await?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    trace!(
                        component = "transform",
                        type = "rpm2img",
                        "no template.ovf at {}: non-VMDK variants tolerate this",
                        host_path.display()
                    );
                }
                Err(e) => {
                    return Err(error::Error::Io {
                        path: host_path.clone(),
                        source: e,
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
        match async move {
            let id = self.get_unique_id(ctx).await?;
            let vfs = Vfs::new(&id, env, log).await?;

            // Staged assets
            let rpm_root = vfs.entry("rpms").await;
            // `kits/` holds `<vendor>/<name>/<arch>/` subdirectories for
            // every external kit plus `external-kit-metadata.json` at the
            // root (staged in `stage()` above). `rpm2img.sh` accepts an
            // empty `EXTERNAL_KITS_PATH` — imghelper's inventory rewrite
            // just short-circuits when the JSON has an empty `kit[]`
            // array (which is what `stage()` emits for kit-less projects).
            let kits_root = vfs.entry("kits").await;
            // Staged but not passed as an arg until the guest-image wire
            // shape lands; see `GuestImagesNotWired` check below.
            let _images_root = vfs.entry("images").await;
            let bin_root = vfs.entry("bin").await;

            // Working area
            let build_root = vfs.create_dir("build-root").await?;

            // `--guest-images` wants a newline-separated list of
            // `<guest>:<install_path>:<host_dir>` triples (see
            // `rpm2img.sh` line ~241 and old-twoliter
            // `builder.rs:581-592`). The script's `if [ -n … ]` gate
            // skips the loop when the value is empty, which is the
            // correct behavior for variants without guests.
            //
            // Populated case is **not yet wired**: `VariantOptions`
            // currently carries only `Vec<Addr>` for `guest_images`
            // (no `install_path`, no per-guest sub-directory in
            // `images_root`), so we cannot build valid triples yet.
            // Passing the bare `images_root` path here — as an earlier
            // version of this file did — causes the shell to parse it
            // as a single malformed triple (`guest=<path>`,
            // `install_path=""`, `host_dir=""`) and fail with a
            // confusing "source directory '' for guest '<path>'"
            // error. Fail loudly instead until the wire shape is
            // extended with per-guest name + install_path + staged
            // sub-path; see `crates/twoliter/src/variant/options.rs`.
            ensure!(
                self.options.guest_images.is_empty(),
                error::GuestImagesNotWiredSnafu {
                    count: self.options.guest_images.len(),
                }
            );

            let mut args = vec![
                format!("--package-dir={:?}", rpm_root.path()),
                format!("--output-dir={:?}", build_root.path()),
                format!("--output-fmt={}", self.options.image.format),
                format!("--external-kits-path={:?}", kits_root.path()),
                format!(
                    "--os-image-size-gib={}",
                    self.options.image.os_size.unwrap_or(2)
                ),
                format!(
                    "--data-image-size-gib={}",
                    self.options.image.data_size.unwrap_or(1)
                ),
                format!("--partition-plan={}", self.options.image.partitions),
                // Old-twoliter passes `--ovf-template=` unconditionally
                // (build.Dockerfile:442). We do the same, pointing at a
                // fixed container-side path staged during `stage()` above.
                // For non-VMDK variants the file may not exist on disk;
                // that's tolerated by `imghelper.sh::sanity_checks`, which
                // only reads the file when `output_fmt == "vmdk"`.
                format!("--ovf-template={:?}", bin_root.path().join("template.ovf")),
                // Empty value: gated by `if [ -n … ]` in rpm2img.sh so
                // the guest-embed loop is skipped. Non-empty case is
                // rejected above with `GuestImagesNotWired`.
                "--guest-images=".to_string(),
                // The RepoTransform tarball nests SBOM RPMs at
                // `sbom-rpms/` inside the same layer it stages as `rpms/`
                // (see `variant/repo/transform.rs`). Point rpm2img at that
                // nested path so it matches old-twoliter's
                // `--sbom-package-dir=/local/sbom-rpms` contract.
                format!("--sbom-package-dir={:?}", rpm_root.path().join("sbom-rpms")),
            ];
            args.append(&mut self.options.image.features.to_args());

            vfs.set_env("IMAGE_NAME", "bottlerocket").await?;
            vfs.set_env("VARIANT", &self.options.name.to_string()).await?;
            vfs.set_env("VARIANT_PLATFORM", &self.options.name.platform()).await?;
            vfs.set_env("ARCH", &self.options.arch.to_string()).await?;
            vfs.set_env("VERSION_ID", &self.options.version.to_string()).await?;
            // Old-twoliter derived BUILD_ID from `git describe --abbrev=8`
            // (see `Makefile.toml:27`) so that a given source tree always
            // produced the same value. That contract is what makes
            // `${VERSION_ID}-${BUILD_ID}` (rpm2img.sh:83) a reproducible
            // output-directory name.
            //
            // The transform's `Id::digest()` already provides a stronger
            // version of the same property: it's a SHA-256 over every input
            // that could change the image (dependent artifacts, kits,
            // features, sizes, kernel params — see `get_unique_id`). We
            // take the first 8 hex chars to match old-twoliter's 8-hex-digit
            // filename ergonomics; the risk of a 32-bit collision within a
            // single build tree is negligible.
            //
            // Not using `jiff::Timestamp::now()` (which is what this line
            // used to be): a wall-clock value churns on every rebuild, which
            // (a) breaks reproducibility and (b) invalidates every cached
            // downstream artifact that keys off `${VERSION_ID}-${BUILD_ID}`.
            let build_id: String = id.digest().chars().take(8).collect();
            vfs.set_env("BUILD_ID", &build_id).await?;
            vfs.set_env("PRETTY_NAME", "Bottlerocket OS").await?;
            // TODO: add config option for this
            vfs.set_env("PROJECT_VENDOR", "bottlerocket").await?;
            // Old-twoliter's `build.Dockerfile:363,385` exposes this as
            // `KERNEL_PARAMETERS` (matching the manifest field name), and
            // `rpm2img.sh:585` reads that exact variable when it stitches
            // together `grub.cfg`. Setting it under any other name (e.g.
            // `KERNEL_OPTIONS`) trips `set -u` in the script with
            // `KERNEL_PARAMETERS: unbound variable`.
            vfs.set_env("KERNEL_PARAMETERS", &self.options.os.kernel_parameters.join(" ")).await?;

            // Now we run rpm2img :D
            vfs.command("rpm2img", "bin/rpm2img", args).await?;

            // We then just archive the output in build_root!
            let writer = ctx.storage().safe_start_layer().await?;
            env.read_stream(build_root.path(), writer.clone()).await?;
            let layer = ctx
                .storage()
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(Compression::None))
                        .build(),
                )
                .await?;
            let mut artifact = Artifact::builder()
                .config(Config::builder().id(id.clone()).build())
                .media_type(MediaType::Manifest)
                .layers(vec![layer])
                .build();
            ctx.storage().safe_save(&artifact).await?;
            Ok::<Artifact, TransformError>(artifact.clone())
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
        #[snafu(display(
            "variant declares {count} `[[variant.guest-image]]` entr{} but \
             embedding guest images is not yet supported by this build of \
             twoliter (VariantOptions carries only guest addresses, not the \
             install-path + staged-directory info the rpm2img script needs \
             to build `<guest>:<install_path>:<host_dir>` triples). Remove \
             the guest-image entries from variant.toml to build without \
             them, or wire the populated path in \
             `crates/twoliter/src/variant/rpm2img/mod.rs`",
            if *count == 1 { "y" } else { "ies" },
        ))]
        GuestImagesNotWired { count: usize },
        #[snafu(display("failed to read {}: {source}", path.display()))]
        Io {
            path: std::path::PathBuf,
            source: std::io::Error,
        },
        #[snafu(display(
            "kit source address {addr} does not follow the `//kits/<vendor>/<name>` \
             convention; cannot derive the on-disk `<vendor>/<name>/<arch>/` \
             layout that `rpm2img.sh --external-kits-path=` expects. Rename the \
             kit's addr or route it through the project resolver."
        ))]
        MalformedKitAddr { addr: Addr },
        #[snafu(display("failed to serialize external-kit-metadata.json: {source}"))]
        Json { source: serde_json::Error },
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

#[cfg(test)]
mod parse_kit_addr_tests {
    //! Exhaustive coverage of [`parse_kit_addr`] because it sits
    //! directly on the path-traversal boundary — a bad segment
    //! becomes a filesystem path fragment under `kits/`.
    use super::parse_kit_addr;
    use edo::context::Addr;

    fn parse(addr: &str) -> Option<(String, String)> {
        parse_kit_addr(&Addr::parse(addr).expect("addr parse"))
    }

    #[test]
    fn canonical_shape_parses() {
        assert_eq!(
            parse("//kits/bottlerocket/core-kit"),
            Some(("bottlerocket".to_string(), "core-kit".to_string()))
        );
    }

    #[test]
    fn relative_form_also_parses() {
        // `Addr::parse` treats absolute and relative the same at the
        // component level. Accept both — the layout is derived from
        // components, not from the `//` prefix.
        assert_eq!(
            parse("kits/vendor/name"),
            Some(("vendor".to_string(), "name".to_string()))
        );
    }

    #[test]
    fn wrong_top_level_prefix_rejected() {
        assert_eq!(parse("//packages/foo/bar"), None);
        assert_eq!(parse("//kit/foo/bar"), None); // singular
        assert_eq!(parse("//variants/foo/bar"), None);
    }

    #[test]
    fn too_few_segments_rejected() {
        assert_eq!(parse("//kits"), None);
        assert_eq!(parse("//kits/vendor"), None);
    }

    #[test]
    fn too_many_segments_rejected() {
        // A 4th segment (even the empty one produced by a trailing
        // slash) must fail, or we'd silently place the kit at
        // `kits/vendor/name/` and ignore the tail.
        assert_eq!(parse("//kits/vendor/name/extra"), None);
        assert_eq!(parse("//kits/vendor/name/"), None);
    }

    #[test]
    fn empty_segments_rejected() {
        assert_eq!(parse("//kits//name"), None);
        assert_eq!(parse("//kits/vendor/"), None);
        assert_eq!(parse("//kits//"), None);
    }

    #[test]
    fn dot_and_dotdot_rejected_as_path_traversal_defense() {
        // These would resolve to `kits/../<name>/<arch>` on the
        // filesystem — outside the kits root. Refuse rather than
        // trust the addr.
        assert_eq!(parse("//kits/../etc"), None);
        assert_eq!(parse("//kits/vendor/.."), None);
        assert_eq!(parse("//kits/./name"), None);
        assert_eq!(parse("//kits/vendor/."), None);
    }

    #[test]
    fn unusual_but_legal_characters_pass_through() {
        // Percent-encoded, unicode, dashes, underscores — the addr is
        // an identifier, not a URL. These just become path segments.
        assert_eq!(
            parse("//kits/foo%2Fbar/kit-name_1"),
            Some(("foo%2Fbar".to_string(), "kit-name_1".to_string()))
        );
        assert_eq!(
            parse("//kits/vendor/kit.name"),
            Some(("vendor".to_string(), "kit.name".to_string()))
        );
    }
}
