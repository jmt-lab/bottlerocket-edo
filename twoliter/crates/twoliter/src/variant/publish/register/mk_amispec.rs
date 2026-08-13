//! Assemble the amispec that drives the `RegisterImage` call from a
//! `VariantOptions`, freshly-created snapshot IDs, and an optional
//! user-supplied TOML overlay.
//!
//! Adapted from
//! `old-twoliter/tools/pubsys/src/aws/ami/register/mk_amispec.rs`.
//! The old flow built a `TemplatedAmiSpec` via the `serde-templated`
//! proc-macro, then rendered it with a handlebars context, then
//! converted that to a plain `AmiSpec`. We collapse that indirection:
//! run handlebars on the *raw TOML text* first, then deserialize the
//! rendered text directly into `amispec::AmiSpec`. Same user contract
//! (`{{ ami.unique_name }}` etc.), one dependency instead of three,
//! ~1000 lines of proc-macro machinery avoided.

use crate::variant::features::ImageFeature;
use crate::variant::image::PartitionPlan;
use crate::variant::options::VariantOptions;
use amispec::{AmiSpec, EbsBlockDeviceKind, InvalidValueError};
use serde::Serialize;
use snafu::{OptionExt, ResultExt, Snafu, ensure};
use std::collections::HashMap;
use tokio::fs;

use super::merge_toml::Merge;

/// The virtualization type for every AMI we register. Also matches
/// the `parse_ok` filter in `get_ami_id` — see
/// `old-twoliter/tools/pubsys/src/aws/ami/register/mod.rs:257`.
pub(crate) const VIRT_TYPE: &str = "hvm";

/// Root block-device name. Fixed by convention with old-twoliter.
pub(crate) const ROOT_DEVICE_NAME: &str = "/dev/xvda";
/// Data block-device name. Present only for `partition-plan = "split"`.
pub(crate) const DATA_DEVICE_NAME: &str = "/dev/xvdb";

/// Default amispec template as raw TOML text. Handlebars variables
/// are interpolated by [`render_amispec_toml`] before deserialization.
///
/// Written as `raw + block-device fragments`. The block-device
/// mappings are joined in below based on which snapshots the register
/// pipeline actually created (root always, data only for split-layout
/// variants), so we don't emit a data mapping with a placeholder
/// snapshot ID for unified variants.
const DEFAULT_AMISPEC_HEAD: &str = r#"
name = "{{ ami.unique_name }}"
{{#if ami.description}}description = "{{ ami.description }}"
{{/if}}architecture = "{{ ami.arch }}"
root-device-name = "/dev/xvda"
sriov-net-support = "simple"
virtualization-type = "hvm"
ena-support = true
"#;

const ROOT_DEVICE_TEMPLATE: &str = r#"
[block-device-mappings."/dev/xvda".ebs]
volume-type = "gp2"
volume-size = {{ block_devices.root.volume_size }}
snapshot-id = "{{ block_devices.root.snapshot_id }}"
delete-on-termination = true
"#;

const DATA_DEVICE_TEMPLATE: &str = r#"
[block-device-mappings."/dev/xvdb".ebs]
volume-type = "gp2"
volume-size = {{ block_devices.data.volume_size }}
snapshot-id = "{{ block_devices.data.snapshot_id }}"
delete-on-termination = true
"#;

/// Inputs supplied by the caller (register orchestrator) that drive
/// the amispec assembly. Extracted from `VariantOptions` + the
/// snapshot IDs coldsnap just produced + CLI overrides.
#[derive(Debug, Clone)]
pub(crate) struct AmispecInputs<'a> {
    /// AMI name. Derived from `bottlerocket-<variant>-<arch>-<version>-<build_id>`
    /// unless overridden by `--name`.
    pub name: String,
    /// AMI description. Derived similarly to `name` unless overridden.
    pub description: Option<String>,
    /// AMI architecture as EC2 spells it: `x86_64` or `arm64`.
    pub arch: &'a str,
    /// Snapshot IDs. `data` is only present for split-layout variants.
    pub snapshots: BottlerocketSnapshots,
    /// The variant's `[variant.image]` block, source of truth for
    /// volume sizes and feature flags.
    pub variant: &'a VariantOptions,
    /// Optional user overlay TOML text (contents of `--amispec-file`).
    pub user_overlay: Option<String>,
}

/// Snapshot IDs produced by the register orchestrator. `data` is
/// only present when the variant's partition plan is `split`.
#[derive(Debug, Clone)]
pub(crate) struct BottlerocketSnapshots {
    pub os_snapshot: String,
    pub data_snapshot: Option<String>,
}

impl BottlerocketSnapshots {
    /// Return every snapshot ID this bundle carries. Used by the
    /// register cleanup path and by `get_snapshots` callers that
    /// want to name existing snapshots.
    pub(crate) fn snapshot_ids(&self) -> Vec<String> {
        let mut ids = vec![self.os_snapshot.clone()];
        if let Some(data) = &self.data_snapshot {
            ids.push(data.clone());
        }
        ids
    }
}



/// Build the full amispec (with block-device mappings) from a set of
/// concrete snapshot IDs. Called after `coldsnap` finishes.
pub(crate) fn create_amispec(inputs: &AmispecInputs<'_>) -> Result<AmiSpec, AmispecError> {
    // Guard: uefi-secure-boot needs a UEFI-data blob we don't yet
    // have a source for. Fail loudly rather than register an AMI that
    // silently ignores the feature flag.
    ensure!(
        !inputs
            .variant
            .image
            .features
            .is_enabled(ImageFeature::UefiSecureBoot),
        SecureBootNotWiredSnafu {
            feature: "uefi-secure-boot"
        }
    );

    let template = assemble_template(inputs)?;
    let context = build_render_context(inputs)?;
    let mut spec = render_and_parse(&template, &context, inputs.user_overlay.as_deref())?;
    ensure_amispec_volume_sizes(&mut spec, &context.block_devices)?;

    // Feature-driven overrides that don't fit cleanly in the template
    // (they replace fields the template already set):
    //
    // - encrypted-storage: boot-mode=uefi + tpm-support=v2.0
    //   (old-twoliter mk_amispec.rs:271-279).
    if inputs
        .variant
        .image
        .features
        .is_enabled(ImageFeature::EncryptedStorage)
    {
        spec.boot_mode = Some(amispec::BootMode::Uefi);
        spec.tpm_support = Some(amispec::TpmSupport::V2_0);
    }

    Ok(spec)
}

/// Load `--amispec-file` contents (if any) so the caller can hand
/// them into `AmispecInputs`. This is a helper rather than an inline
/// call so tests can pass raw text without touching the disk.
pub(crate) async fn load_amispec_file(
    path: Option<&std::path::Path>,
) -> Result<Option<String>, AmispecError> {
    match path {
        Some(p) => Ok(Some(
            fs::read_to_string(p)
                .await
                .context(ReadOverlaySnafu { path: p.to_owned() })?,
        )),
        None => Ok(None),
    }
}

/// Build the default template text (head + root device + optional
/// data device). No handlebars processing yet — that happens in
/// [`render_and_parse`] after we've decided whether to merge in a
/// user overlay.
fn assemble_template(inputs: &AmispecInputs<'_>) -> Result<String, AmispecError> {
    let mut default = String::from(DEFAULT_AMISPEC_HEAD);
    default.push_str(ROOT_DEVICE_TEMPLATE);
    if inputs.snapshots.data_snapshot.is_some() {
        default.push_str(DATA_DEVICE_TEMPLATE);
    }
    Ok(default)
}

/// Handlebars-render both the default template and the optional user
/// overlay against `context`, then merge the resulting `toml::Table`s
/// (user wins per-key), then deserialize the merged table into an
/// `AmiSpec`.
///
/// The render-then-merge order matters: our default template embeds
/// handlebars tokens in non-string TOML positions (e.g.
/// `volume-size = {{ … }}`), so parsing the template as TOML before
/// rendering would fail. Rendering first turns those into concrete
/// integers/strings before any TOML parser sees them.
fn render_and_parse(
    template_text: &str,
    context: &AmispecRenderContext,
    user_overlay: Option<&str>,
) -> Result<AmiSpec, AmispecError> {
    let mut hb = handlebars::Handlebars::new();
    hb.set_strict_mode(true);
    let rendered = hb
        .render_template(template_text, context)
        .context(RenderTemplateSnafu)?;
    tracing::trace!("Rendered default amispec:\n{rendered}");
    let mut merged: toml::Table =
        toml::from_str(&rendered).context(ParseRenderedSnafu {
            rendered: rendered.clone(),
        })?;

    if let Some(user_text) = user_overlay {
        let rendered_user = hb
            .render_template(user_text, context)
            .context(RenderTemplateSnafu)?;
        tracing::trace!("Rendered user amispec overlay:\n{rendered_user}");
        let user_table: toml::Table =
            toml::from_str(&rendered_user).context(ParseUserOverlaySnafu)?;
        merged.merge(user_table);
    }

    toml::Value::Table(merged)
        .try_into()
        .map_err(|source| AmispecError::ParseRendered {
            rendered: "<merged-toml>".to_string(),
            source,
        })
}

/// Handlebars render context — `{{ ami.foo }}`, `{{ block_devices.root.bar }}`.
#[derive(Debug, Clone, Serialize)]
struct AmispecRenderContext {
    ami: AmiRenderContext,
    block_devices: HashMap<String, BlockDeviceRenderContext>,
}

#[derive(Debug, Clone, Serialize)]
struct AmiRenderContext {
    unique_name: String,
    arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct BlockDeviceRenderContext {
    device_name: String,
    snapshot_id: String,
    volume_size: u64,
}

/// Build the handlebars context from the amispec inputs. Validates
/// that the variant's partition plan agrees with which snapshots
/// were created.
fn build_render_context(inputs: &AmispecInputs<'_>) -> Result<AmispecRenderContext, AmispecError> {
    let (expects_data, err_msg) = match inputs.variant.image.partitions {
        PartitionPlan::Split => (true, "expects data volume, but none provided"),
        PartitionPlan::Unified => (false, "does not expect data volume, but one provided"),
    };
    ensure!(
        expects_data == inputs.snapshots.data_snapshot.is_some(),
        ImageLayoutSnafu {
            message: err_msg.to_string(),
        }
    );

    // Volume sizes: fall back to the same defaults old-twoliter used
    // (see `manifest.rs::publish_image_sizes_gib`). Root defaults to
    // 2 GiB, data to 20 GiB.
    let root_size = inputs.variant.image.os_size.unwrap_or(2) as u64;
    let data_size = inputs.variant.image.data_size.unwrap_or(20) as u64;

    let mut block_devices = HashMap::new();
    block_devices.insert(
        "root".to_string(),
        BlockDeviceRenderContext {
            device_name: ROOT_DEVICE_NAME.to_string(),
            snapshot_id: inputs.snapshots.os_snapshot.clone(),
            volume_size: root_size,
        },
    );
    if let Some(data_snapshot) = &inputs.snapshots.data_snapshot {
        block_devices.insert(
            "data".to_string(),
            BlockDeviceRenderContext {
                device_name: DATA_DEVICE_NAME.to_string(),
                snapshot_id: data_snapshot.clone(),
                volume_size: data_size,
            },
        );
    }

    Ok(AmispecRenderContext {
        ami: AmiRenderContext {
            unique_name: inputs.name.clone(),
            arch: inputs.arch.to_string(),
            description: inputs.description.clone(),
        },
        block_devices,
    })
}

/// If a user overlay set a `volume-size` smaller than the variant's
/// declared size, override it to the declared size (matches
/// old-twoliter `ensure_amispec_volume_sizes` at
/// `mk_amispec.rs:165-219`).
fn ensure_amispec_volume_sizes(
    amispec: &mut AmiSpec,
    block_devices: &HashMap<String, BlockDeviceRenderContext>,
) -> Result<(), AmispecError> {
    let Some(bdms) = amispec.block_device_mappings.as_mut() else {
        return Ok(());
    };

    for bdm in bdms.values_mut() {
        let Some(ebs) = bdm.ebs.as_mut() else {
            continue;
        };
        // Look up which context device (root/data) this EBS is
        // associated with via its snapshot ID. If the user's overlay
        // replaced the snapshot ID with something we don't recognize
        // we leave the size untouched — the user knows what they're
        // doing.
        let snapshot_id = match ebs.snapshot_id() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let Some(device_settings) = block_devices
            .values()
            .find(|d| d.snapshot_id == snapshot_id)
        else {
            continue;
        };
        let size_hint = device_settings.volume_size;
        match ebs.volume_size() {
            Some(curr) if curr < size_hint => {
                tracing::warn!(
                    "EBS volume for '{}' was configured with size {curr}, but size hints \
                     dictate at least {size_hint}. Using {size_hint}.",
                    device_settings.device_name
                );
                ebs.set_volume_size(Some(size_hint))
                    .context(InvalidEbsVolumeSizeSnafu)?;
            }
            Some(_) => {}
            None => ebs
                .set_volume_size(Some(size_hint))
                .context(InvalidEbsVolumeSizeSnafu)?,
        }
    }
    Ok(())
}

pub(crate) type Result<T, E = AmispecError> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
#[snafu(module(amispec_error), visibility(pub(crate)))]
pub(crate) enum AmispecError {
    #[snafu(display("Failed to parse user --amispec-file overlay as TOML: {source}"))]
    ParseUserOverlay { source: toml::de::Error },

    #[snafu(display("Failed to read user --amispec-file at '{}': {source}", path.display()))]
    ReadOverlay {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("Failed to render amispec template: {source}"))]
    RenderTemplate {
        source: handlebars::RenderError,
    },

    #[snafu(display(
        "Failed to parse rendered amispec as AmiSpec: {source}\n--- rendered text ---\n{rendered}"
    ))]
    ParseRendered {
        rendered: String,
        source: toml::de::Error,
    },

    #[snafu(display("Image-layout mismatch: {message}"))]
    ImageLayout { message: String },

    #[snafu(display("Invalid EBS volume size: {source}"))]
    InvalidEbsVolumeSize { source: InvalidValueError },

    #[snafu(display(
        "Variant enables feature '{feature}' but the on-disk UEFI-data source is not yet \
         wired into the new twoliter publish flow. Disable the feature or wait for \
         secure-boot support to land."
    ))]
    SecureBootNotWired { feature: String },
}

// Re-export for the orchestrator error module.
pub(crate) use amispec_error::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Arch;
    use crate::variant::{
        features::ImageFeatures,
        image::{ImageFormat, PartitionPlan},
        options::{ImageOptions, OsOptions, VariantOptions},
        variant::Variant,
    };
    use semver::Version;

    /// Build a minimal `VariantOptions` for tests without going
    /// through the full config loader.
    fn test_variant(partitions: PartitionPlan, features: ImageFeatures) -> VariantOptions {
        VariantOptions {
            name: Variant::new("aws-ecs-4").unwrap(),
            version: Version::new(1, 0, 0),
            arch: Arch::X86_64,
            depends: Vec::new(),
            guest_images: Vec::new(),
            image: ImageOptions {
                format: ImageFormat::Raw,
                partitions,
                os_size: Some(2),
                data_size: Some(20),
                features,
                ovf_template: None,
            },
            os: OsOptions {
                kernel_parameters: Vec::new(),
                packages: Vec::new(),
            },
        }
    }

    /// A unified-layout variant with only an OS snapshot renders a
    /// spec that carries exactly one block-device mapping
    /// (`/dev/xvda`) with the right snapshot ID.
    #[test]
    fn unified_layout_renders_root_only() {
        let variant = test_variant(PartitionPlan::Unified, ImageFeatures::default());
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: Some("test description".to_string()),
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: None,
            },
            variant: &variant,
            user_overlay: None,
        };
        let spec = create_amispec(&inputs).unwrap();
        assert_eq!(spec.name, "test-ami");
        assert_eq!(spec.description.as_deref(), Some("test description"));
        let bdms = spec.block_device_mappings.unwrap();
        assert!(bdms.contains_key("/dev/xvda"));
        assert!(!bdms.contains_key("/dev/xvdb"));
        let root_ebs = bdms["/dev/xvda"].ebs.as_ref().unwrap();
        assert_eq!(root_ebs.snapshot_id(), Some("snap-os"));
    }

    /// A split-layout variant with both OS and data snapshots renders
    /// two block-device mappings.
    #[test]
    fn split_layout_renders_root_and_data() {
        let variant = test_variant(PartitionPlan::Split, ImageFeatures::default());
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: None,
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: Some("snap-data".to_string()),
            },
            variant: &variant,
            user_overlay: None,
        };
        let spec = create_amispec(&inputs).unwrap();
        let bdms = spec.block_device_mappings.unwrap();
        assert_eq!(
            bdms["/dev/xvda"].ebs.as_ref().unwrap().snapshot_id(),
            Some("snap-os")
        );
        assert_eq!(
            bdms["/dev/xvdb"].ebs.as_ref().unwrap().snapshot_id(),
            Some("snap-data")
        );
    }

    /// `partition-plan = "split"` without a data snapshot is caught
    /// by `build_render_context` before we try to render anything.
    #[test]
    fn split_layout_without_data_snapshot_errors() {
        let variant = test_variant(PartitionPlan::Split, ImageFeatures::default());
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: None,
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: None,
            },
            variant: &variant,
            user_overlay: None,
        };
        assert!(matches!(
            create_amispec(&inputs).unwrap_err(),
            AmispecError::ImageLayout { .. }
        ));
    }

    /// A variant with `uefi-secure-boot = true` fails loudly with
    /// `SecureBootNotWired`, as designed for MVP.
    #[test]
    fn uefi_secure_boot_fails_loudly() {
        let features: ImageFeatures = toml::from_str(r#""uefi-secure-boot" = true"#).unwrap();
        let variant = test_variant(PartitionPlan::Unified, features);
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: None,
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: None,
            },
            variant: &variant,
            user_overlay: None,
        };
        assert!(matches!(
            create_amispec(&inputs).unwrap_err(),
            AmispecError::SecureBootNotWired { .. }
        ));
    }

    /// `encrypted-storage = true` forces `boot-mode = uefi` and
    /// `tpm-support = v2.0` regardless of what the template said.
    #[test]
    fn encrypted_storage_sets_boot_and_tpm() {
        let features: ImageFeatures = toml::from_str(r#""encrypted-storage" = true"#).unwrap();
        let variant = test_variant(PartitionPlan::Unified, features);
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: None,
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: None,
            },
            variant: &variant,
            user_overlay: None,
        };
        let spec = create_amispec(&inputs).unwrap();
        assert_eq!(spec.boot_mode, Some(amispec::BootMode::Uefi));
        assert_eq!(spec.tpm_support, Some(amispec::TpmSupport::V2_0));
    }

    /// A user overlay merges onto the default template — same key
    /// wins; unrelated keys survive.
    #[test]
    fn user_overlay_merges_at_toml_level() {
        let variant = test_variant(PartitionPlan::Unified, ImageFeatures::default());
        let inputs = AmispecInputs {
            name: "test-ami".to_string(),
            description: None,
            arch: "x86_64",
            snapshots: BottlerocketSnapshots {
                os_snapshot: "snap-os".to_string(),
                data_snapshot: None,
            },
            variant: &variant,
            user_overlay: Some(
                r#"
[block-device-mappings."/dev/xvda".ebs]
volume-type = "gp3"
"#
                .to_string(),
            ),
        };
        let spec = create_amispec(&inputs).unwrap();
        let bdms = spec.block_device_mappings.unwrap();
        let root = bdms["/dev/xvda"].ebs.as_ref().unwrap();
        assert_eq!(root.volume_type(), "gp3");
        // snapshot-id came from default → survived merge
        assert_eq!(root.snapshot_id(), Some("snap-os"));
    }
}
