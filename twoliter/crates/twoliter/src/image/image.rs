//! Filesystem staging and image assembly. Ports the non-verity /
//! non-secure-boot pieces of `imghelper`.
//!
//! - [`validate_build_inputs`] ports `sanity_checks`. Its OVF-template
//!   text-shape rules are factored into the pure
//!   [`validate_ovf_template_text`] helper (fully unit-tested); the
//!   async wrapper reads the template through `Vfs` and delegates.
//! - The `mkfs_*` and disk-staging functions are typed skeletons whose
//!   subprocess bodies are wired up but not exercised in this crate's
//!   test suite (they need real `mkfs.*` binaries).

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use snafu::OptionExt;

use crate::image::error::{Error, InvalidLayoutSnafu, Result};
use crate::image::model::{ImageFormat, PartitionPlan};

/// Pre-flight checks for a variant-image build. Rejects impossible
/// combinations before we start shelling out.
///
/// Ports `imghelper::sanity_checks`.
#[derive(Debug, Clone)]
pub struct BuildInputs<'a> {
    pub output_fmt: ImageFormat,
    pub partition_plan: PartitionPlan,
    /// OVF template path (inside the environment). Required for
    /// `Vmdk` output; unused otherwise.
    pub ovf_template: Option<&'a Path>,
    pub uefi_secure_boot: bool,
    /// SBOM package directory (inside the environment). If provided,
    /// must exist.
    pub sbom_package_dir: Option<&'a Path>,
}

/// Validate caller-supplied build inputs against a live `Vfs`. Errors
/// surface as [`Error::InvalidLayout`] with a descriptive `reason`.
///
/// Composes three checks:
/// 1. [`validate_pre_template`] — the pure rules that don't need I/O
///    (EIF rejection, VMDK-requires-template).
/// 2. `sbom_package_dir` existence, dispatched via `vfs.try_exists`.
/// 3. For VMDK builds, [`validate_ovf_template_text`] against the
///    OVF-template bytes read through `vfs`.
///
/// Splitting the pure checks out lets us unit-test each branch without
/// spinning up a mock `Vfs`.
pub async fn validate_build_inputs(vfs: &Vfs, inputs: &BuildInputs<'_>) -> Result<()> {
    validate_pre_template(inputs)?;

    if let Some(dir) = inputs.sbom_package_dir
        && !vfs.try_exists(dir).await?
    {
        return Err(Error::InvalidLayout {
            context: "sbom-package-dir".into(),
            reason: format!("does not exist: {}", dir.display()),
        });
    }

    if matches!(inputs.output_fmt, ImageFormat::Vmdk) {
        // `validate_pre_template` already enforced that ovf_template is Some.
        let template = inputs
            .ovf_template
            .expect("checked by validate_pre_template");
        let bytes = vfs.read(template).await?;
        let text = String::from_utf8_lossy(&bytes);
        validate_ovf_template_text(&text, inputs.partition_plan, inputs.uefi_secure_boot)?;
    }

    Ok(())
}

/// Pure validator for the non-I/O branches of [`validate_build_inputs`].
///
/// Rejects `ImageFormat::Eif` (built by a separate transform) and
/// requires an OVF template when `output_fmt` is VMDK. The template's
/// *contents* are validated by [`validate_ovf_template_text`] which
/// [`validate_build_inputs`] calls once the bytes have been read.
pub fn validate_pre_template(inputs: &BuildInputs<'_>) -> Result<()> {
    // rpm2img's sanity_checks doesn't accept EIF (that lives in
    // rpm2eif); reject here too.
    if matches!(inputs.output_fmt, ImageFormat::Eif) {
        return Err(Error::InvalidLayout {
            context: "output_fmt".into(),
            reason: "eif is built by the eif transform, not the variant-image path".into(),
        });
    }
    if matches!(inputs.output_fmt, ImageFormat::Vmdk) && inputs.ovf_template.is_none() {
        return Err(Error::InvalidLayout {
            context: "output_fmt=vmdk".into(),
            reason: "OVF template is required".into(),
        });
    }
    Ok(())
}

/// Pure validator for OVF-template text. Encodes the placeholder-shape
/// rules independently of any I/O so they can be tested without a live
/// `Vfs`.
pub fn validate_ovf_template_text(
    text: &str,
    partition_plan: PartitionPlan,
    uefi_secure_boot: bool,
) -> Result<()> {
    match partition_plan {
        PartitionPlan::Split => {
            if !text.contains("{{DATA_DISK}}") {
                return Err(Error::InvalidLayout {
                    context: "ovf-template".into(),
                    reason: "'split' layout requires `{{DATA_DISK}}` in the OVF template".into(),
                });
            }
        }
        PartitionPlan::Unified => {
            if text.contains("{{DATA_DISK}}") {
                return Err(Error::InvalidLayout {
                    context: "ovf-template".into(),
                    reason: "'unified' layout must NOT include `{{DATA_DISK}}` in the OVF template"
                        .into(),
                });
            }
        }
    }
    if uefi_secure_boot && !text.contains("{{DB_CERT_DER_HEX}}") {
        return Err(Error::InvalidLayout {
            context: "ovf-template".into(),
            reason: "secure-boot builds require `{{DB_CERT_DER_HEX}}` in the OVF template".into(),
        });
    }
    Ok(())
}

/// `mkfs.ext4` a data image, then `dd` it onto the target disk.
///
/// Ports `imghelper::mkfs_data_ext4`. Full subprocess sequence.
///
/// `unlabeled_debugfs_script` is a `debugfs` script that applies
/// SELinux labels to files created without a policy context (the shell
/// version pipes it into `debugfs -w -f - <scratch_image>` after the
/// `mkfs.ext4` call). Piping stdin into a child process is not yet
/// available in edo's [`Vfs`], so this function only accepts the empty
/// string. Any non-empty script fails with [`Error::Unsupported`]
/// rather than being silently dropped: a data partition that skips the
/// labeling step boots with mislabelled inodes and SELinux denies at
/// runtime, which is exactly the shape of silent regression the goal
/// doc calls out.
///
/// [`Vfs`]: edo::environment::Vfs
#[allow(clippy::too_many_arguments)]
pub async fn mkfs_data_ext4(
    vfs: &Vfs,
    target: &Path,
    size_mib: u32,
    offset_mib: u32,
    scratch_image: &Path,
    data_mount: &Path,
    unlabeled_debugfs_script: &str,
) -> Result<()> {
    reject_nonempty_debugfs_script(unlabeled_debugfs_script)?;

    let size_arg = format!("{size_mib}M");
    let data_mount_q = crate::image::vfs_ext::shell_quote_path(data_mount);
    let scratch_q = crate::image::vfs_ext::shell_quote_path(scratch_image);
    vfs.command(
        "mkfs_data_ext4",
        "mkfs.ext4",
        &[
            "-m",
            "0",
            "-d",
            data_mount_q.as_str(),
            scratch_q.as_str(),
            size_arg.as_str(),
        ],
    )
    .await?;

    dd_partition(vfs, scratch_image, target, offset_mib).await
}

/// Pure guard for [`mkfs_data_ext4`]'s SELinux-labeling step.
///
/// The shell version pipes a debugfs `ea_set` script into
/// `debugfs -w -f - <scratch_image>` after `mkfs.ext4` so that files
/// created by mkfs without a policy context receive the correct
/// `security.selinux` xattr. Skipping this step silently produces a
/// data partition whose inodes are mislabelled — the boot never fails
/// loudly, but SELinux denies at runtime and services fail to start.
///
/// edo's `Vfs` has no stdin-piping primitive today, so `br-image`
/// cannot perform this step itself. Rather than silently drop the
/// script, this guard rejects any non-empty input with a typed
/// [`Error::Unsupported`] so the caller (transform driver) is forced
/// to notice the gap and route the script through its own facility.
///
/// An empty script means "no labels to apply" and is a valid no-op.
pub fn reject_nonempty_debugfs_script(script: &str) -> Result<()> {
    if !script.is_empty() {
        return Err(Error::Unsupported {
            what: "mkfs_data_ext4: applying unlabeled_debugfs_script (SELinux label fix-ups \
                   for files created without a policy context)"
                .into(),
            blocker: "edo::environment::Vfs has no stdin-piping primitive; pipe the script \
                      into `debugfs -w -f - <scratch_image>` from the transform driver, \
                      or wait for a Vfs stdin API"
                .into(),
        });
    }
    Ok(())
}

/// `mkfs.xfs` on a data image by first writing zeros; the actual XFS
/// filesystem is created at first boot. Ports `mkfs_data_xfs`.
pub async fn mkfs_data_xfs(
    vfs: &Vfs,
    target: &Path,
    size_mib: u32,
    offset_mib: u32,
    scratch_image: &Path,
) -> Result<()> {
    // dd parses `key=value` args itself, so quote the entire
    // key=value token to protect the value's metacharacters from the
    // enclosing shell.
    let of = crate::image::vfs_ext::shell_quote(&format!("of={}", scratch_image.display()));
    let count = format!("count={size_mib}");
    vfs.command(
        "mkfs_data_xfs_zero",
        "dd",
        &["if=/dev/zero", of.as_str(), "bs=1M", count.as_str()],
    )
    .await?;
    dd_partition(vfs, scratch_image, target, offset_mib).await
}

/// Build the erofs root filesystem from `root_mount` and write it to
/// `root_image`. Ports `mkfs_root_erofs`.
pub async fn mkfs_root_erofs(
    vfs: &Vfs,
    root_mount: &Path,
    root_image: &Path,
    selinux_file_contexts: &Path,
) -> Result<()> {
    let fc = crate::image::vfs_ext::shell_quote(&format!(
        "--file-contexts={}",
        selinux_file_contexts.display()
    ));
    let root_image_q = crate::image::vfs_ext::shell_quote_path(root_image);
    let root_mount_q = crate::image::vfs_ext::shell_quote_path(root_mount);
    // The shell also passes `-T $(stat -c %Y "$root_mount/root")` — the
    // transform driver is expected to compute the stat and pass it
    // through in a follow-up wrapper.
    vfs.command(
        "mkfs_root_erofs",
        "mkfs.erofs",
        &[
            fc.as_str(),
            "--all-root",
            "-z",
            "lz4hc,12",
            "-C",
            "262144",
            root_image_q.as_str(),
            root_mount_q.as_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Verify that `image` fits in the partition budget. Ports
/// `check_image_size`.
pub async fn check_image_size(vfs: &Vfs, image: &Path, partition_mib: u32) -> Result<()> {
    let bytes = crate::image::vfs_ext::size_bytes(vfs, image).await?;
    let budget = (partition_mib as u64) * 1024 * 1024;
    if bytes > budget {
        return Err(Error::Partition {
            plan: "check_image_size".into(),
            feature: image.display().to_string(),
            reason: format!(
                "image content is {bytes} bytes; partition budget is {budget} bytes ({partition_mib} MiB)"
            ),
        });
    }
    Ok(())
}

/// `dd` a scratch image onto the target disk at the given offset.
/// Skeleton wrapper: no partition math, just the shell command.
pub async fn dd_partition(vfs: &Vfs, scratch: &Path, target: &Path, offset_mib: u32) -> Result<()> {
    // Quote the whole `key=value` token so the value survives sh -c.
    let ifof = crate::image::vfs_ext::shell_quote(&format!("if={}", scratch.display()));
    let ofof = crate::image::vfs_ext::shell_quote(&format!("of={}", target.display()));
    let seek = format!("seek={offset_mib}");
    vfs.command(
        "dd_partition",
        "dd",
        &[
            ifof.as_str(),
            ofof.as_str(),
            "conv=notrunc",
            "bs=1M",
            seek.as_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Install a CA-bundle into the rootfs. Ports `install_ca_certs`.
/// Skeleton.
pub async fn install_ca_certs(vfs: &Vfs, root: &Path, source_bundle: &Path) -> Result<PathBuf> {
    let install_dir = root.join("usr/share/factory/etc/pki/tls/certs");
    vfs.create_dir(&install_dir).await?;
    let dst = install_dir.join("ca-bundle.crt");
    vfs.copy(source_bundle, &dst).await?;
    Ok(dst)
}

/// Install a TUF root JSON file into the rootfs. Ports
/// `install_root_json`.
pub async fn install_tuf_root(vfs: &Vfs, root: &Path, source_json: &Path) -> Result<PathBuf> {
    let install_dir = root.join("usr/share/updog");
    vfs.create_dir(&install_dir).await?;
    let dst = install_dir.join("root.json");
    vfs.copy(source_json, &dst).await?;
    Ok(dst)
}

/// Inputs for [`stage_root`]. Every optional field defaults to `None`,
/// which the coordinator treats as "skip this step".
///
/// Fields that are `Some(_)` must reference paths that already exist
/// inside the [`Vfs`]; the underlying primitives will surface the
/// error otherwise.
#[derive(Debug, Clone)]
pub struct StageRootInputs<'a> {
    /// The rootfs mount point that RPMs will install into.
    pub root_mount: &'a Path,
    /// Directory containing every `*.rpm` to install.
    pub package_dir: &'a Path,
    /// Package short names to hand to `dnf install` when
    /// [`install_by_name`](Self::install_by_name) is `true`. Ignored
    /// otherwise. The list is passed to dnf as-is, so callers add
    /// any convention-required prefix (e.g. `bottlerocket-`)
    /// themselves.
    pub package_names: &'a [String],
    /// When true, [`crate::image::rpm_install::install_rpms_by_name`]
    /// runs (createrepo + `dnf install <names>`), letting dnf resolve
    /// deps against the local repo and picking exactly the packages
    /// named. When false, every RPM under `package_dir` is
    /// force-installed via `rpm -iv`.
    ///
    /// Old-twoliter's `rpm2img` used the force-install path
    /// exclusively because its caller had already filtered
    /// `PACKAGE_DIR` down to the resolved subset via a preceding
    /// `dnf install --downloadonly` step. In this crate the same
    /// filtering happens inline via
    /// [`crate::image::rpm_install::install_rpms_by_name`], so callers
    /// with an `included-packages` list should flip this on.
    pub install_by_name: bool,
    /// Target CPU (e.g. `"x86_64"`, `"aarch64"`) passed to dnf as
    /// `--forcearch`. Ignored when
    /// [`install_by_name`](Self::install_by_name) is `false`. Absent
    /// when the caller doesn't know the arch and is using
    /// force-install anyway.
    pub arch: Option<&'a str>,
    /// Optional CA-bundle PEM to install as
    /// `usr/share/factory/etc/pki/tls/certs/ca-bundle.crt`.
    pub ca_bundle: Option<&'a Path>,
    /// Optional TUF root JSON to install as `usr/share/updog/root.json`.
    pub tuf_root: Option<&'a Path>,
    /// Optional `file_contexts` file for SELinux labeling. When
    /// provided, [`crate::image::selinux::compute_labels`] runs and the
    /// resulting labels are computed but not applied — see
    /// [`crate::image::selinux::build_ea_set_script`] for why applying the
    /// labels requires a stdin-piping primitive `Vfs` does not yet
    /// expose. This function still runs the compute step so a real
    /// `setfiles` error surfaces at build time instead of at first
    /// boot.
    pub selinux_file_contexts: Option<&'a Path>,
}

/// Coordinator for the "install the rootfs, then decorate it" phase
/// of `rpm2img::stage_images`.
///
/// Runs, in order:
///   1. [`crate::image::rpm_install::install_rpms`] against `root_mount`.
///   2. [`install_ca_certs`] when `ca_bundle` is `Some`.
///   3. [`install_tuf_root`] when `tuf_root` is `Some`.
///   4. [`crate::image::selinux::compute_labels`] when
///      `selinux_file_contexts` is `Some`.
///
/// Each step is a call into an already-tested primitive; this function
/// is orchestration only. The chosen order matches the sequence
/// documented in `doc/transforms/variant-image.md::Algorithm` (step
/// `d`): install → decorate → relabel.
pub async fn stage_root(vfs: &Vfs, inputs: &StageRootInputs<'_>) -> Result<()> {
    if inputs.install_by_name {
        let arch = inputs.arch.with_context(|| InvalidLayoutSnafu {
            context: "stage_root".to_string(),
            reason: "install_by_name=true requires arch to be set".to_string(),
        })?;
        crate::image::rpm_install::install_rpms_by_name(
            vfs,
            inputs.root_mount,
            inputs.package_dir,
            inputs.package_names,
            arch,
        )
        .await?;
    } else {
        crate::image::rpm_install::install_rpms(vfs, inputs.root_mount, inputs.package_dir).await?;
    }
    if let Some(bundle) = inputs.ca_bundle {
        install_ca_certs(vfs, inputs.root_mount, bundle).await?;
    }
    if let Some(root_json) = inputs.tuf_root {
        install_tuf_root(vfs, inputs.root_mount, root_json).await?;
    }
    if let Some(contexts) = inputs.selinux_file_contexts {
        // The bytes are computed but not applied — see doc-comment on
        // StageRootInputs.selinux_file_contexts for the reason.
        let _ = crate::image::selinux::compute_labels(vfs, inputs.root_mount, contexts).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(fmt: ImageFormat, plan: PartitionPlan) -> BuildInputs<'static> {
        BuildInputs {
            output_fmt: fmt,
            partition_plan: plan,
            ovf_template: None,
            uefi_secure_boot: false,
            sbom_package_dir: None,
        }
    }

    #[test]
    fn eif_output_fmt_rejected() {
        let err =
            validate_pre_template(&inputs(ImageFormat::Eif, PartitionPlan::Unified)).unwrap_err();
        assert!(err.to_string().contains("eif"));
    }

    #[test]
    fn raw_output_needs_no_template() {
        // No template supplied; validate_pre_template accepts because
        // Raw doesn't require one.
        validate_pre_template(&inputs(ImageFormat::Raw, PartitionPlan::Split)).unwrap();
    }

    #[test]
    fn vmdk_requires_template() {
        let err =
            validate_pre_template(&inputs(ImageFormat::Vmdk, PartitionPlan::Unified)).unwrap_err();
        assert!(err.to_string().contains("OVF template"));
    }

    #[test]
    fn vmdk_with_template_passes_pre_template() {
        let template = std::path::Path::new("/dev/null");
        let inputs = BuildInputs {
            output_fmt: ImageFormat::Vmdk,
            partition_plan: PartitionPlan::Unified,
            ovf_template: Some(template),
            uefi_secure_boot: false,
            sbom_package_dir: None,
        };
        // Content validation lives in validate_ovf_template_text; the
        // pre-template pass only checks that a path was supplied.
        validate_pre_template(&inputs).unwrap();
    }

    #[test]
    fn vmdk_split_requires_data_disk_placeholder() {
        let text = "<Envelope>{{OS_DISK}}</Envelope>\n";
        let err = validate_ovf_template_text(text, PartitionPlan::Split, false).unwrap_err();
        assert!(err.to_string().contains("DATA_DISK"));
    }

    #[test]
    fn vmdk_unified_rejects_data_disk_placeholder() {
        let text = "{{OS_DISK}} {{DATA_DISK}}";
        let err = validate_ovf_template_text(text, PartitionPlan::Unified, false).unwrap_err();
        assert!(err.to_string().contains("unified"));
    }

    #[test]
    fn vmdk_secure_boot_requires_db_cert_placeholder() {
        let text = "{{OS_DISK}} {{DATA_DISK}}";
        let err = validate_ovf_template_text(text, PartitionPlan::Split, true).unwrap_err();
        assert!(err.to_string().contains("DB_CERT_DER_HEX"));
    }

    #[test]
    fn valid_split_template_accepted() {
        let text = "{{OS_DISK}} {{DATA_DISK}} {{DB_CERT_DER_HEX}}";
        validate_ovf_template_text(text, PartitionPlan::Split, true).unwrap();
    }

    #[test]
    fn empty_debugfs_script_is_accepted() {
        // No labels to apply is a valid no-op.
        reject_nonempty_debugfs_script("").unwrap();
    }

    #[test]
    fn nonempty_debugfs_script_is_rejected_loudly() {
        // A silently-dropped script produces a data partition with
        // mislabelled inodes; ensure the caller is forced to notice.
        let err = reject_nonempty_debugfs_script("ea_set /foo security.selinux system_u:...\n")
            .unwrap_err();
        assert!(
            matches!(err, Error::Unsupported { .. }),
            "expected Unsupported, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("stdin"),
            "err message should name the blocker: {msg}"
        );
        assert!(
            msg.contains("debugfs"),
            "err message should name the tool: {msg}"
        );
    }

    #[test]
    fn valid_unified_template_accepted() {
        let text = "{{OS_DISK}}";
        validate_ovf_template_text(text, PartitionPlan::Unified, false).unwrap();
    }
}
