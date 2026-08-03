//! Variant-metadata RPM spec and rpmbuild argument generation, and the
//! variant-manifest emission that `twoliter publish` consumers read.
//!
//! Consumes the bundled `metadata.spec` (see [`crate::image::METADATA_SPEC`])
//! along with the resolved [`ImageFeatures`] to render the `--with` /
//! `--without` argument list rpmbuild expects.
//!
//! [`VariantManifest`] is the JSON blob that ends up as the
//! `manifest.json` output layer of the `variant-image` transform. Its
//! shape is a straight port of the fields
//! `old-twoliter/tools/buildsys/src/manifest.rs::ManifestInfo` writes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use snafu::ResultExt;

use crate::image::error::{Result, SerializationSnafu};
use crate::image::model::{
    Arch, ImageFeature, ImageFeatures, ImageFormat, ImageLayout, PartitionPlan,
};

/// Default variant flavor when the variant name has no fourth segment.
/// Matches `old-twoliter/tools/bottlerocket-variant::DEFAULT_VARIANT_FLAVOR`.
pub const DEFAULT_VARIANT_FLAVOR: &str = "none";

/// Parsed segments of a Bottlerocket variant name (dash-delimited).
///
/// A variant name has the shape `platform-runtime[-version][-flavor]`
/// (plus optional trailing segments that are ignored). The parser is
/// a Rust port of `old-twoliter/tools/bottlerocket-variant::Variant`
/// and only extracts the segments the metadata RPM needs: `platform`,
/// `runtime`, `family` (= `platform-runtime`), and `flavor` (defaulting
/// to `"none"`).
///
/// The metadata RPM's `.spec` (see [`crate::image::METADATA_SPEC`])
/// consumes these as `%_cross_variant_*` macros so it can emit
/// `Provides: bottlerocket-variant-platform(<platform>)` etc., which
/// kmod packages depend on via `Requires:
/// bottlerocket-variant-platform(<platform>)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantSegments<'a> {
    pub name: &'a str,
    pub platform: &'a str,
    pub runtime: &'a str,
    pub family: String,
    pub flavor: &'a str,
}

impl<'a> VariantSegments<'a> {
    /// Parse `name` into its metadata-RPM-relevant segments.
    ///
    /// Errors if the first two segments (`platform`, `runtime`) are
    /// missing or empty. Fourth segment defaults to
    /// [`DEFAULT_VARIANT_FLAVOR`] (`"none"`).
    pub fn parse(name: &'a str) -> std::result::Result<Self, VariantNameError> {
        let mut parts = name.split('-');
        let platform = parts.next().unwrap_or("");
        if platform.is_empty() {
            return Err(VariantNameError {
                name: name.to_string(),
                reason: "platform segment is missing or empty".into(),
            });
        }
        let runtime = parts.next().ok_or_else(|| VariantNameError {
            name: name.to_string(),
            reason: "runtime segment is missing".into(),
        })?;
        if runtime.is_empty() {
            return Err(VariantNameError {
                name: name.to_string(),
                reason: "runtime segment is empty".into(),
            });
        }
        // Skip past the optional version segment (third dash-delimited
        // token, e.g. `1.33` in `aws-k8s-1.33-nvidia`); we don't need
        // to expose it because the metadata spec doesn't reference
        // `%_cross_variant_version`.
        let _version = parts.next();
        let flavor = parts.next().unwrap_or(DEFAULT_VARIANT_FLAVOR);
        Ok(Self {
            name,
            platform,
            runtime,
            family: format!("{platform}-{runtime}"),
            flavor,
        })
    }
}

/// Error returned by [`VariantSegments::parse`] when the variant name
/// fails validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantNameError {
    pub name: String,
    pub reason: String,
}

impl std::fmt::Display for VariantNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid variant name `{}`: {}", self.name, self.reason)
    }
}

impl std::error::Error for VariantNameError {}

/// Inputs to [`build_metadata_rpm`].
///
/// `variant_name` is the raw variant string (e.g. `"aws-k8s-1.33"`);
/// [`VariantSegments::parse`] decomposes it into the `_cross_variant_*`
/// macros the spec needs. `arch` is the target CPU (`"x86_64"` or
/// `"aarch64"`) — used both to load the SDK's arch-specific rpm macros
/// (which define `%{_cross_os}` = `"bottlerocket-"`) and passed to
/// rpmbuild as `--define "_target_cpu"`.
///
/// `output_dir` is where the finished `bottlerocket-metadata-*.rpm`
/// (and its `-sbom-` subpackage) land. The caller places this under
/// the transform's `deps/rpms/` staging tree so `install_rpms` picks
/// them up.
#[derive(Debug, Clone)]
pub struct BuildMetadataRpmInputs<'a> {
    pub variant_name: &'a str,
    pub arch: &'a str,
    pub features: &'a ImageFeatures,
    /// Directory that will hold the built `.rpm` files (and the
    /// scratch rpmbuild tree). Must exist inside the vfs.
    pub workdir: &'a std::path::Path,
    /// Directory where the produced `.rpm` files are copied after the
    /// build. Must exist inside the vfs.
    pub output_dir: &'a std::path::Path,
}

/// Build the per-variant metadata RPM inside the SDK environment.
///
/// Ports the `metadata.spec` build stage of
/// `old-twoliter/twoliter/embedded/build.Dockerfile` (see the
/// `rpm-macros-and-bconds` and `repobuild` stages). The produced RPM
/// provides the virtual capabilities that other RPMs (kmod packages,
/// release-*, etc.) require:
///
/// - `bottlerocket-variant(<name>)`
/// - `bottlerocket-variant-platform(<platform>)`
/// - `bottlerocket-variant-runtime(<runtime>)`
/// - `bottlerocket-variant-family(<family>)`
/// - `bottlerocket-variant-flavor(<flavor>)`
/// - `bottlerocket-image-feature(<feat>)` / `no-<feat>` per feature
///
/// Steps performed inside `workdir`:
///
///  1. Create `rpmbuild/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}`.
///  2. Copy the SDK's arch-specific rpm macros to `.rpmmacros` (this
///     is where `%{_cross_os}` is defined).
///  3. Write the bundled [`crate::image::METADATA_SPEC`] to
///     `rpmbuild/SPECS/metadata.spec`.
///  4. Run `rpmbuild -bb` with `_target_cpu`, `_topdir`, every
///     `_cross_variant_*` macro, and `--with`/`--without` toggles for
///     each feature listed in [`RPMBUILD_TOGGLES`].
///  5. Copy every produced `.rpm` from `rpmbuild/RPMS/**` into
///     `output_dir/`.
///
/// The metadata spec produces two RPMs: the main
/// `bottlerocket-metadata` package and a `bottlerocket-metadata-sbom`
/// subpackage; both are copied.
pub async fn build_metadata_rpm(
    vfs: &edo::environment::Vfs,
    inputs: &BuildMetadataRpmInputs<'_>,
) -> Result<()> {
    use crate::image::error::VariantNameSnafu;
    use snafu::ResultExt;

    let segments = VariantSegments::parse(inputs.variant_name)
        .with_context(|_| VariantNameSnafu)?;

    // Step 1: rpmbuild tree.
    //
    // Every path handed to rpmbuild must be **absolute**. rpmbuild
    // spawns its own `sh -e` subshells for each phase, and `%mkbuilddir`
    // `cd`s into `%{_topdir}/BUILD/<name>-<ver>-build` before running
    // `%prep`; a relative `_topdir` gets resolved from the phase's
    // cwd rather than from the vfs cwd, and the injected
    // `%__spec_prep_pre` (see SDK macros' `ln -srnf %{_topdir}/BUILD/sources
    // %{builddir}/sources`) then fails ENOENT.
    //
    // `Vfs::create_dir` returns a child Vfs whose `.path()` is
    // absolute (post-canonicalize), so we capture the handles rather
    // than composing PathBufs by hand.
    let workdir_vfs = vfs.entry(inputs.workdir).await;
    let rpmbuild_vfs = workdir_vfs.create_dir("rpmbuild").await?;
    let mut sub_paths = std::collections::HashMap::new();
    for sub in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"] {
        let child = rpmbuild_vfs.create_dir(sub).await?;
        sub_paths.insert(sub, child.path().to_path_buf());
    }
    // `BUILD/sources` must exist even for a source-less spec: the
    // SDK's arch-specific rpm macros inject a `%__spec_prep_pre` that
    // runs `ln -srnf %{_topdir}/BUILD/sources %{builddir}/sources` at
    // the top of every `%prep`. Without the dir, ln fails with
    // ENOENT and %prep exits non-zero. Packages get this dir for
    // free by copying `src/sources` in; metadata.spec has no sources
    // so we create the dir empty.
    rpmbuild_vfs.create_dir("BUILD/sources").await?;

    let rpmbuild_path = rpmbuild_vfs.path().to_path_buf();
    let specs_path = sub_paths["SPECS"].clone();
    let rpms_path = sub_paths["RPMS"].clone();

    // Step 2: SDK arch-specific rpm macros. Defines `%{_cross_os}`,
    // among other cross-compile macros. Copied to both the workdir's
    // `.rpmmacros` (which rpmbuild picks up when cwd points at it)
    // and `/root/.rpmmacros` (which it picks up unconditionally when
    // running as root) — same belt-and-braces pattern as
    // `PackageTransform`.
    let rpmmacros_src = format!(
        "/usr/lib/rpm/platform/{}-bottlerocket/macros",
        inputs.arch
    );
    vfs.copy(&rpmmacros_src, &workdir_vfs.path().join(".rpmmacros"))
        .await?;
    vfs.copy(&rpmmacros_src, "/root/.rpmmacros").await?;

    // Step 3: write spec.
    let spec_path = specs_path.join("metadata.spec");
    vfs.write(&spec_path, crate::image::METADATA_SPEC.as_bytes())
        .await?;

    // Step 4: rpmbuild. Args flow through the vfs `execute` as a
    // shell-joined string, so `--define` values that carry spaces
    // must include their own literal double quotes; the same pattern
    // `PackageTransform` uses. See `edo::environment::Vfs::command`.
    let topdir_arg = format!("--define \"_topdir {:?}\"", rpmbuild_path);
    let target_cpu_arg = format!("--define \"_target_cpu {}\"", inputs.arch);
    let variant_arg = format!("--define \"_cross_variant {}\"", segments.name);
    let platform_arg = format!(
        "--define \"_cross_variant_platform {}\"",
        segments.platform
    );
    let runtime_arg = format!(
        "--define \"_cross_variant_runtime {}\"",
        segments.runtime
    );
    let family_arg = format!(
        "--define \"_cross_variant_family {}\"",
        segments.family
    );
    let flavor_arg = format!(
        "--define \"_cross_variant_flavor {}\"",
        segments.flavor
    );

    let mut argv: Vec<String> = vec![
        "-bb".into(),
        "--clean".into(),
        "--undefine _auto_set_build_flags".into(),
        topdir_arg,
        target_cpu_arg,
        variant_arg,
        platform_arg,
        runtime_arg,
        family_arg,
        flavor_arg,
    ];

    for tok in render_metadata_rpmbuild_args(inputs.features) {
        argv.push(tok);
    }
    argv.push(format!("{:?}", spec_path));

    vfs.command("build_metadata_rpm", "rpmbuild", &argv).await?;

    // Step 5: copy the built RPMs to `output_dir`. rpmbuild drops
    // them under `rpmbuild/RPMS/<arch>/`, so we glob the arch
    // subdir the same way `PackageTransform` does its final copy.
    let copy_glob = format!("{:?}/{}/*.rpm", rpms_path, inputs.arch);
    // `output_dir` may be relative; resolve it against the vfs cwd
    // by walking through `Vfs::entry` (its `.path()` is absolute).
    let output_dir_abs = vfs.entry(inputs.output_dir).await.path().to_path_buf();
    vfs.command(
        "copy_metadata_rpm",
        "cp",
        &[
            "-r".to_string(),
            copy_glob,
            format!("{:?}", output_dir_abs),
        ],
    )
    .await?;
    Ok(())
}

/// Convert an [`ImageFeatures`] snapshot into the ordered
/// `--with FLAG` / `--without FLAG` argument list for `rpmbuild`. Every
/// feature listed in [`RPMBUILD_TOGGLES`] appears exactly once.
///
/// Matches the shell's convention: features named in `metadata.spec`
/// under `%if %{with feature_name}` translate to `--with feature_name`
/// / `--without feature_name`.
pub fn render_metadata_rpmbuild_args(features: &ImageFeatures) -> Vec<String> {
    let mut out = Vec::with_capacity(RPMBUILD_TOGGLES.len() * 2);
    for (flag, on) in metadata_toggles(features) {
        out.push(if on {
            "--with".into()
        } else {
            "--without".into()
        });
        out.push(flag.into());
    }
    out
}

/// Features that appear as `%if %{with ...}` toggles in `metadata.spec`.
///
/// The `rpmbuild_name` column is the exact identifier the spec expects;
/// the accessor produces the value from an [`ImageFeatures`] snapshot.
pub const RPMBUILD_TOGGLES: &[(ImageFeature, &str)] = &[
    (ImageFeature::InPlaceUpdates, "in_place_updates"),
    (ImageFeature::HostContainers, "host_containers"),
    (ImageFeature::UefiSecureBoot, "uefi_secure_boot"),
    (ImageFeature::XfsDataPartition, "xfs_data_partition"),
    (ImageFeature::ErofsRootPartition, "erofs_root_partition"),
    (ImageFeature::Fips, "fips"),
    (
        ImageFeature::ExternalKmodDevelopment,
        "external_kmod_development",
    ),
    (ImageFeature::EncryptedStorage, "encrypted_storage"),
    (ImageFeature::FirstPartyStack, "first_party_stack"),
];

fn metadata_toggles(features: &ImageFeatures) -> Vec<(&'static str, bool)> {
    RPMBUILD_TOGGLES
        .iter()
        .map(|(feature, name)| (*name, is_on(features, *feature)))
        .collect()
}

fn is_on(features: &ImageFeatures, feature: ImageFeature) -> bool {
    match feature {
        ImageFeature::GrubSetPrivateVar => features.grub_set_private_var,
        ImageFeature::SystemdNetworkd => features.systemd_networkd,
        ImageFeature::XfsDataPartition => features.xfs_data_partition,
        ImageFeature::ErofsRootPartition => features.erofs_root_partition,
        ImageFeature::UefiSecureBoot => features.uefi_secure_boot,
        ImageFeature::Fips => features.fips,
        ImageFeature::InPlaceUpdates => features.in_place_updates,
        ImageFeature::HostContainers => features.host_containers,
        ImageFeature::ExternalKmodDevelopment => features.external_kmod_development,
        ImageFeature::EncryptedStorage => features.encrypted_storage,
        ImageFeature::FirstPartyStack => features.first_party_stack,
    }
}

/// Variant `manifest.json` — the descriptor
/// `twoliter publish ami` / `twoliter publish repo` consume when they
/// pick up an already-built variant image.
///
/// Every field maps 1:1 to an emitter in
/// `old-twoliter/tools/buildsys/src/manifest.rs::ManifestInfo`. Kept
/// kebab-case on the wire so the file remains grep-compatible with the
/// old manifests.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct VariantManifest {
    pub name: String,
    pub version: semver::Version,
    pub arch: Arch,
    pub image_format: ImageFormat,
    pub partition_plan: PartitionPlan,
    /// Full resolved layout (post-[`crate::image::layout::resolved_image_layout`]).
    pub image_layout: ImageLayout,
    /// The set of enabled features. Serialized as a sorted array of
    /// kebab-case identifiers (matches [`ImageFeature`]'s wire
    /// format), which round-trips cleanly through the same set-of-
    /// features shape old buildsys used.
    pub image_features: BTreeSet<ImageFeature>,
    /// Publish-time OS image size in GiB, or `None` when the caller
    /// left it to publish tooling to derive.
    #[serde(default)]
    pub os_image_publish_size_gib: Option<i32>,
    /// Publish-time data image size in GiB. `-1` means "no separate
    /// data image" (the caller is on a `Unified` plan); `None` when
    /// unset.
    #[serde(default)]
    pub data_image_publish_size_gib: Option<i32>,
    /// Kernel-parameters in their declaration order (order-sensitive:
    /// grub concatenates them verbatim).
    #[serde(default)]
    pub kernel_parameters: Vec<String>,
}

impl VariantManifest {
    /// Serialize to pretty-printed JSON bytes suitable for writing to
    /// the `manifest.json` output layer.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(self).context(SerializationSnafu)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Metadata layer emitted by the `variant-rootfs` transform.
///
/// Carries information the disk stage needs but can't rederive from
/// the rootfs tar alone: the SELinux `file_contexts` path discovered
/// after `stage_root` installed the policy RPM, and the sorted list
/// of RPM names installed into the rootfs (used by the finalizer's
/// SBOM step to attribute packages without re-scanning `deps/rpms/`).
///
/// Serialized as JSON with kebab-case keys so a human can `jq` a
/// broken cache entry when debugging.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct RootfsMetadata {
    /// Absolute path (inside the rootfs) to the `file_contexts` file
    /// picked out by `resolve_file_contexts`. `None` when no SELinux
    /// policy RPM landed — the image will boot unlabeled and the
    /// downstream stage falls back to an empty contexts file.
    pub file_contexts_path: Option<std::path::PathBuf>,
    /// Sorted RPM basenames installed into the rootfs (with the
    /// trailing `.rpm` extension). Duplicates removed, order stable.
    pub packaged_rpms: Vec<String>,
}

impl RootfsMetadata {
    /// Serialize to pretty-printed JSON bytes with a trailing newline
    /// so the layer file is diff-friendly.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(self).context(SerializationSnafu)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_segments_parses_four_segment_name() {
        let s = VariantSegments::parse("aws-k8s-1.29-nvidia").unwrap();
        assert_eq!(s.platform, "aws");
        assert_eq!(s.runtime, "k8s");
        assert_eq!(s.family, "aws-k8s");
        assert_eq!(s.flavor, "nvidia");
    }

    #[test]
    fn variant_segments_defaults_flavor_to_none() {
        let s = VariantSegments::parse("aws-k8s-1.33").unwrap();
        assert_eq!(s.platform, "aws");
        assert_eq!(s.runtime, "k8s");
        assert_eq!(s.family, "aws-k8s");
        assert_eq!(s.flavor, "none");
    }

    #[test]
    fn variant_segments_two_segments_ok() {
        // e.g. `aws-dev` — no version, no flavor.
        let s = VariantSegments::parse("aws-dev").unwrap();
        assert_eq!(s.platform, "aws");
        assert_eq!(s.runtime, "dev");
        assert_eq!(s.family, "aws-dev");
        assert_eq!(s.flavor, "none");
    }

    #[test]
    fn variant_segments_rejects_single_segment() {
        assert!(VariantSegments::parse("aws").is_err());
    }

    #[test]
    fn variant_segments_rejects_empty_platform() {
        assert!(VariantSegments::parse("-k8s").is_err());
    }

    #[test]
    fn variant_segments_rejects_empty_runtime() {
        assert!(VariantSegments::parse("aws-").is_err());
    }

    #[test]
    fn all_off_yields_all_without() {
        let f = ImageFeatures::default();
        let args = render_metadata_rpmbuild_args(&f);
        assert_eq!(args.len(), RPMBUILD_TOGGLES.len() * 2);
        for pair in args.chunks(2) {
            assert_eq!(pair[0], "--without");
        }
    }

    #[test]
    fn selected_features_produce_with() {
        let f = ImageFeatures {
            uefi_secure_boot: true,
            first_party_stack: true,
            in_place_updates: true,
            ..ImageFeatures::default()
        };
        let args = render_metadata_rpmbuild_args(&f);
        // Format is (--with|--without) NAME pairs; scan for our flags.
        let mut with = Vec::new();
        let mut without = Vec::new();
        for pair in args.chunks(2) {
            if pair[0] == "--with" {
                with.push(pair[1].clone());
            } else {
                without.push(pair[1].clone());
            }
        }
        assert!(with.contains(&"uefi_secure_boot".to_string()));
        assert!(with.contains(&"first_party_stack".to_string()));
        assert!(with.contains(&"in_place_updates".to_string()));
        assert!(without.contains(&"fips".to_string()));
        assert!(without.contains(&"encrypted_storage".to_string()));
    }

    #[test]
    fn embedded_metadata_spec_is_non_empty() {
        assert!(crate::image::METADATA_SPEC.contains("%{_cross_os}metadata"));
    }

    #[test]
    fn variant_manifest_round_trips_through_json() {
        let mut features = BTreeSet::new();
        features.insert(ImageFeature::FirstPartyStack);
        features.insert(ImageFeature::UefiSecureBoot);
        let manifest = VariantManifest {
            name: "aws-k8s-1.29".into(),
            version: semver::Version::new(0, 1, 0),
            arch: Arch::X86_64,
            image_format: ImageFormat::Raw,
            partition_plan: PartitionPlan::Split,
            image_layout: ImageLayout::new(Some(2), 1, 22, PartitionPlan::Split),
            image_features: features,
            os_image_publish_size_gib: Some(2),
            data_image_publish_size_gib: Some(20),
            kernel_parameters: vec!["quiet".into(), "console=ttyS0".into()],
        };
        let bytes = manifest.to_json_bytes().expect("serialize");
        let parsed: VariantManifest = serde_json::from_slice(&bytes).expect("round-trip");
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn rootfs_metadata_round_trips_through_json() {
        let metadata = RootfsMetadata {
            file_contexts_path: Some(std::path::PathBuf::from(
                "/usr/lib/selinux/targeted/contexts/files/file_contexts",
            )),
            packaged_rpms: vec!["a.rpm".into(), "b.rpm".into()],
        };
        let bytes = metadata.to_json_bytes().expect("serialize");
        let parsed: RootfsMetadata = serde_json::from_slice(&bytes).expect("round-trip");
        assert_eq!(parsed, metadata);
    }

    #[test]
    fn rootfs_metadata_absent_file_contexts_serializes_as_null() {
        // Variants without an SELinux policy RPM emit
        // `file_contexts_path: None`; the JSON representation must
        // survive a round-trip.
        let metadata = RootfsMetadata {
            file_contexts_path: None,
            packaged_rpms: vec![],
        };
        let bytes = metadata.to_json_bytes().expect("serialize");
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("null"), "expected null contexts, got {text}");
        let parsed: RootfsMetadata = serde_json::from_slice(&bytes).expect("round-trip");
        assert_eq!(parsed, metadata);
    }

    #[test]
    fn rootfs_metadata_wire_shape_is_kebab_case() {
        let metadata = RootfsMetadata {
            file_contexts_path: Some(std::path::PathBuf::from("/tmp/fc")),
            packaged_rpms: vec![],
        };
        let bytes = metadata.to_json_bytes().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("\"file-contexts-path\""),
            "text={text}"
        );
        assert!(text.contains("\"packaged-rpms\""), "text={text}");
    }

    #[test]
    fn variant_manifest_wire_shape_is_kebab_case() {
        let manifest = VariantManifest {
            name: "x".into(),
            version: semver::Version::new(0, 0, 1),
            arch: Arch::X86_64,
            image_format: ImageFormat::Raw,
            partition_plan: PartitionPlan::Split,
            image_layout: ImageLayout::default(),
            image_features: BTreeSet::new(),
            os_image_publish_size_gib: None,
            data_image_publish_size_gib: None,
            kernel_parameters: vec![],
        };
        let bytes = manifest.to_json_bytes().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        // Every field name is kebab-case; assert on a handful of the
        // hyphen-containing ones to catch a rename regression.
        assert!(text.contains("\"image-format\""), "text={text}");
        assert!(text.contains("\"partition-plan\""), "text={text}");
        assert!(text.contains("\"image-layout\""), "text={text}");
        assert!(text.contains("\"image-features\""), "text={text}");
        assert!(text.contains("\"kernel-parameters\""), "text={text}");
    }
}
