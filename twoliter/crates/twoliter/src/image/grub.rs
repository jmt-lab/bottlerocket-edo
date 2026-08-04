//! GRUB installation and grub.cfg emission (non-signing paths). The
//! signing paths (`sign_grub`, `unembed_grub`, `sign_grubcfg`) live in
//! [`crate::image::secure_boot`].

use std::path::Path;

use edo::environment::Vfs;

use snafu::OptionExt;

use crate::image::error::{Error, InvalidLayoutSnafu, Result};
use crate::image::model::Arch;
use crate::image::secure_boot::{
    Sbsetup, provide_certs, sign_grub, sign_mokm, sign_shim, unembed_grub, zero_shim,
};
use crate::image::vfs_ext::{list_files, shell_quote, shell_quote_path};

/// Configuration for [`install_grub_bios`].
#[derive(Debug, Clone)]
pub struct GrubInstall<'a> {
    pub root_mount: &'a Path,
    pub os_image: &'a Path,
    pub arch: Arch,
}

/// Install the GRUB stage-2 into the MBR / BIOS-BOOT partition (x86_64
/// only). Ports the `grub-bios-setup` invocation in `rpm2img`
/// L396-407. Skeleton.
pub async fn install_grub_bios<'a>(vfs: &Vfs, inputs: &GrubInstall<'a>) -> Result<()> {
    if !matches!(inputs.arch, Arch::X86_64) {
        return Ok(());
    }
    // Write the device.map used by grub-bios-setup to identify hd0.
    let device_map = inputs.root_mount.join("boot/grub/device.map");
    let device_map_body = format!("(hd0) {}\n", inputs.os_image.display());
    vfs.write(&device_map, device_map_body.as_bytes()).await?;

    let grub_dir = inputs.root_mount.join("boot/grub");
    // Quote the whole `--flag=value` token so the value survives sh -c.
    let dir_arg = shell_quote(&format!("--directory={}", grub_dir.display()));
    let map_arg = shell_quote(&format!("--device-map={}", device_map.display()));
    let os_image_q = shell_quote_path(inputs.os_image);
    vfs.command(
        "grub_bios_setup",
        "grub-bios-setup",
        &[
            dir_arg.as_str(),
            map_arg.as_str(),
            "--root=hd0",
            "--skip-fs-probe",
            os_image_q.as_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Configuration for [`install_grub_uefi`].
///
/// The UEFI install path is the same on x86_64 and aarch64 in shape —
/// the only difference is which shim/grub/mokm binaries the grub RPM
/// dropped into `${root_mount}/boot/efi/EFI/BOOT/`. Both arches carry
/// this out; only [`install_grub_bios`] is x86_64-only.
///
/// Every path is a `Vfs`-relative or absolute path inside the
/// environment.
#[derive(Debug, Clone)]
pub struct GrubUefiInstall<'a> {
    /// Rootfs mount point. `${root_mount}/boot/efi/EFI/BOOT/*.efi`
    /// were placed there by the shim/grub/mokm RPMs.
    pub root_mount: &'a Path,
    /// The OS disk-image file the ESP will be `dd`-ed into.
    pub os_image: &'a Path,
    /// Working directory the caller has prepared for the ESP staging
    /// (mtools writes into this via mmd/mcopy).
    pub efi_mount: &'a Path,
    /// Scratch file the caller has reserved for the ESP contents.
    /// Sized by `efi_partition_mib` MiB.
    pub efi_scratch: &'a Path,
    /// EFI-A partition size in MiB, from
    /// [`crate::image::partition::compute_partition_layout`].
    pub efi_partition_mib: u32,
    /// EFI-A partition offset in MiB, from
    /// [`crate::image::partition::compute_partition_layout`].
    pub efi_offset_mib: u32,
    /// When `true`, sign shim / mokm / grub via
    /// [`crate::image::secure_boot`]. When `false`, zero the shim's vendor
    /// cert and unembed the grub gpg pubkey (matches
    /// `rpm2img::L426-436`).
    pub uefi_secure_boot: bool,
    /// Secure-boot profile setup. Required when `uefi_secure_boot`
    /// is on; the caller is expected to have already invoked
    /// `.apply(vfs)` before calling this function.
    pub secure_boot_setup: Option<&'a Sbsetup>,
    /// Paths to the DER vendor cert / db cert / vendor obj file that
    /// signing needs. `vendor_cert_der` is required when
    /// `uefi_secure_boot` is on; `vendor_obj` is a scratch path for
    /// the cert-table blob that `zero_shim` / `sign_shim` write
    /// before objcopy folds it into the shim binary. When `None`,
    /// [`install_grub_uefi`] auto-generates it as
    /// `<efi_mount>/vendor.obj`. Callers that supply their own path
    /// must ensure it does not alias the shim binary itself; the
    /// function rejects such aliases with a typed error.
    pub vendor_cert_der: Option<&'a Path>,
    pub vendor_obj: Option<&'a Path>,
    pub db_cer: Option<&'a Path>,
    pub vendor_cer: Option<&'a Path>,
    pub config_sign_pubkey: Option<&'a Path>,
    /// db cert PEM + DER for [`provide_certs`]. Present only when
    /// `uefi_secure_boot` is on.
    pub db_crt: Option<&'a Path>,
    pub db_cer_for_esp: Option<&'a Path>,
    pub arch: Arch,
}

/// Install the UEFI grub payload into the ESP.
///
/// Ports the second half of `rpm2img::L410-450`:
///
/// 1. Move `${root_mount}/boot/efi/EFI` to `${efi_mount}/EFI`.
/// 2. Discover shim/grub/mokm filenames under `${efi_mount}/EFI/BOOT/`.
/// 3. If `uefi_secure_boot`, sign each in turn; else zero shim's
///    vendor cert and unembed grub's GPG pubkey.
/// 4. Allocate the ESP scratch file, format it FAT32, populate
///    `EFI/BOOT/` with every `*.efi` under `${efi_mount}/EFI/BOOT`.
/// 5. When secure boot is on, [`provide_certs`] copies PK/KEK/db onto
///    the ESP so the certs can be imported through the firmware
///    setup UI on bare-metal systems.
/// 6. `dd` the ESP scratch file into `${os_image}` at the EFI-A
///    partition offset.
///
/// Runs on **both x86_64 and aarch64**; the callers using
/// [`install_grub_bios`] and this function together get the same
/// behavior as `rpm2img` (BIOS-BOOT is x86_64-only; ESP is always
/// present).
pub async fn install_grub_uefi<'a>(vfs: &Vfs, inputs: &GrubUefiInstall<'a>) -> Result<()> {
    // Step 1: move the RPM-placed EFI tree out of the rootfs.
    // The shell uses `mv ${ROOT_MOUNT}/boot/efi/*  ${EFI_MOUNT}`, which
    // moves *contents* of `boot/efi`. `Vfs::rename` is a single mv
    // call; use it on the parent EFI directory. We expect exactly one
    // `EFI` directory under `boot/efi` — grub RPMs ship that shape.
    let boot_efi = inputs.root_mount.join("boot/efi");
    let src_efi = boot_efi.join("EFI");
    let dst_efi = inputs.efi_mount.join("EFI");
    vfs.create_dir(inputs.efi_mount).await?;
    if vfs.try_exists(&src_efi).await? {
        vfs.rename(&src_efi, &dst_efi).await?;
    } else {
        return Err(Error::InvalidLayout {
            context: "install_grub_uefi".into(),
            reason: format!(
                "expected {} to exist (grub RPM did not stage its EFI tree)",
                src_efi.display()
            ),
        });
    }

    // Step 2: discover shim / grub / mokm.
    let efi_boot = dst_efi.join("BOOT");
    let shim = pick_first(vfs, &efi_boot, "boot*.efi", "shim").await?;
    let grub = pick_first(vfs, &efi_boot, "grub*.efi", "grub").await?;
    let mokm = pick_first(vfs, &efi_boot, "mm*.efi", "mokm").await?;

    // Resolve the scratch path for the vendor-cert-table blob that
    // `sign_shim` / `zero_shim` write before objcopy folds it into
    // the shim.
    let auto_vendor_obj = inputs.efi_mount.join("vendor.obj");
    let vendor_obj = resolve_vendor_obj(inputs.vendor_obj, &shim, &auto_vendor_obj)?;

    // Step 3: sign or zero.
    if inputs.uefi_secure_boot {
        let vendor_cert_der = inputs.vendor_cert_der.with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: "uefi_secure_boot=true requires vendor_cert_der".to_string(),
        })?;
        let db_cer = inputs.db_cer.with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: "uefi_secure_boot=true requires db_cer".to_string(),
        })?;
        let vendor_cer = inputs.vendor_cer.with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: "uefi_secure_boot=true requires vendor_cer".to_string(),
        })?;
        let config_sign_pubkey = inputs
            .config_sign_pubkey
            .with_context(|| InvalidLayoutSnafu {
                context: "install_grub_uefi".to_string(),
                reason: "uefi_secure_boot=true requires config_sign_pubkey".to_string(),
            })?;
        sign_shim(vfs, &shim, inputs.arch, vendor_cert_der, vendor_obj, db_cer).await?;
        sign_mokm(vfs, &mokm, inputs.arch, vendor_cer).await?;
        sign_grub(vfs, &grub, inputs.arch, config_sign_pubkey, vendor_cer).await?;
    } else {
        // Non-secure-boot: `zero_shim` clears the vendor-cert section
        // of shim, and `unembed_grub` strips the GPG pubkey from grub.
        // `vendor_obj` (resolved above) receives the zeroed
        // cert-table blob before objcopy folds it into the shim; it
        // must be a path distinct from the shim itself.
        zero_shim(vfs, &shim, inputs.arch, vendor_obj).await?;
        unembed_grub(vfs, &grub, inputs.arch).await?;
    }

    // Step 4: allocate + format the ESP scratch file.
    let efi_scratch_q = shell_quote_path(inputs.efi_scratch);
    let scratch_of = shell_quote(&format!("of={}", inputs.efi_scratch.display()));
    let count = format!("count={}", inputs.efi_partition_mib);
    vfs.command(
        "esp_dd_zero",
        "dd",
        &["if=/dev/zero", scratch_of.as_str(), "bs=1M", count.as_str()],
    )
    .await?;

    let size_kib = (inputs.efi_partition_mib as u64) * 1024;
    let size_kib_str = size_kib.to_string();
    vfs.command(
        "esp_mkfs_vfat",
        "mkfs.vfat",
        &[
            "-I",
            "-S",
            "512",
            efi_scratch_q.as_str(),
            size_kib_str.as_str(),
        ],
    )
    .await?;

    vfs.command(
        "esp_mmd_efi",
        "mmd",
        &["-i", efi_scratch_q.as_str(), "::/EFI"],
    )
    .await?;
    vfs.command(
        "esp_mmd_efi_boot",
        "mmd",
        &["-i", efi_scratch_q.as_str(), "::/EFI/BOOT"],
    )
    .await?;

    // Step 4b: copy every `*.efi` from the staged EFI/BOOT dir into
    // the ESP. mtools's `mcopy` accepts multiple files per call, and
    // we don't get shell glob expansion inside `Vfs::command`'s quoted
    // argv, so enumerate the files ourselves.
    let efi_files = list_files(vfs, &efi_boot, "*.efi").await?;
    if efi_files.is_empty() {
        return Err(Error::InvalidLayout {
            context: "install_grub_uefi".into(),
            reason: format!("no *.efi files under {}", efi_boot.display()),
        });
    }
    let file_qs: Vec<String> = efi_files.iter().map(|p| shell_quote_path(p)).collect();
    let mut mcopy_args: Vec<&str> = vec!["-i", efi_scratch_q.as_str()];
    for q in &file_qs {
        mcopy_args.push(q.as_str());
    }
    mcopy_args.push("::/EFI/BOOT");
    vfs.command("esp_mcopy", "mcopy", &mcopy_args).await?;

    // Step 5: seed signing certs onto the ESP for firmware-UI import
    // (secure-boot only).
    if inputs.uefi_secure_boot {
        let db_crt = inputs.db_crt.with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: "uefi_secure_boot=true requires db_crt".to_string(),
        })?;
        let db_cer = inputs.db_cer_for_esp.with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: "uefi_secure_boot=true requires db_cer_for_esp".to_string(),
        })?;
        provide_certs(vfs, inputs.efi_scratch, db_crt, db_cer).await?;
    }

    // Step 6: dd the ESP into the OS disk at the EFI-A offset.
    crate::image::image::dd_partition(
        vfs,
        inputs.efi_scratch,
        inputs.os_image,
        inputs.efi_offset_mib,
    )
    .await
}

/// Resolve the scratch path [`install_grub_uefi`] passes to
/// `sign_shim` / `zero_shim` for the vendor-cert-table blob.
///
/// The blob is written before `objcopy --update-section` folds it
/// into the shim binary. If the caller-supplied path aliases the
/// shim, `zero_shim` clobbers the shim with 4096 bytes of cert-table
/// zeros — the resulting objcopy operates on already-corrupted
/// input, producing an unbootable ESP. Detect that case eagerly and
/// return [`Error::InvalidLayout`] so the caller has a chance to
/// pick a distinct path.
///
/// - `explicit`: the caller's `vendor_obj` field. `Some` means
///   "use this path"; `None` means "auto-generate".
/// - `shim`: the shim binary discovered under `EFI/BOOT/`.
/// - `auto`: the default scratch path (`<efi_mount>/vendor.obj`).
///
/// Returns the resolved path on success. Pure so the alias-guard
/// can be unit-tested without a live [`Vfs`].
fn resolve_vendor_obj<'a>(
    explicit: Option<&'a Path>,
    shim: &Path,
    auto: &'a Path,
) -> Result<&'a Path> {
    match explicit {
        Some(p) if p == shim => Err(Error::InvalidLayout {
            context: "install_grub_uefi".into(),
            reason: format!(
                "vendor_obj path aliases shim binary ({}); pick a distinct scratch path",
                p.display()
            ),
        }),
        Some(p) => Ok(p),
        None => Ok(auto),
    }
}

/// Pick the first file under `dir` matching `glob`. Returns
/// [`Error::InvalidLayout`] when nothing matches so the caller can
/// surface a specific-enough error to explain what was missing.
///
/// Used by [`install_grub_uefi`] to discover shim / grub / mokm
/// filenames the grub RPM shipped.
async fn pick_first(vfs: &Vfs, dir: &Path, glob: &str, role: &str) -> Result<std::path::PathBuf> {
    let matches = list_files(vfs, dir, glob).await?;
    matches
        .into_iter()
        .next()
        .with_context(|| InvalidLayoutSnafu {
            context: "install_grub_uefi".to_string(),
            reason: format!(
                "no {role} binary found under {} matching `{glob}`",
                dir.display()
            ),
        })
}

/// Options for [`render_grub_cfg`].
#[derive(Debug, Clone)]
pub struct GrubCfgOptions<'a> {
    pub pretty_name: &'a str,
    pub version_id: &'a str,
    /// Space-separated kernel parameters.
    pub kernel_parameters: &'a str,
    /// The rendered `DM_VERITY_ROOT` value (see
    /// [`crate::image::verity::VerityInfo::dm_verity_root_row`]).
    pub dm_verity_root: &'a str,
    /// When `true`, disable interactive edits and signature checks
    /// (secure-boot mode).
    pub uefi_secure_boot: bool,
    /// When `true`, first-party-stack is enabled; boot-config parameters
    /// are appended.
    pub first_party_stack: bool,
}

/// Render the grub.cfg text emitted by `rpm2img` L569-591. Pure.
pub fn render_grub_cfg(opts: &GrubCfgOptions<'_>) -> String {
    let mut header = String::new();
    if opts.uefi_secure_boot {
        header.push_str("set superusers=\"\"\n");
        header.push_str("set check_signatures=\"no\"\n");
    }

    let (bootconfig, initrd) = if opts.first_party_stack {
        ("bootconfig", "initrd ($private)/bootconfig.data")
    } else {
        ("", "")
    };
    let fallback = if opts.uefi_secure_boot {
        "   echo \"rebooting in 30 seconds...\"\n   sleep 30\n   reboot\n"
    } else {
        ""
    };

    format!(
        "{header}set default=\"0\"\nset timeout=\"0\"\nset dm_verity_root=\"{dvr}\"\n\n\
         menuentry \"{pretty} {ver}\" --unrestricted {{\n\
         \x20\x20 linux ($root)/vmlinuz \\\n\
         \x20\x20\x20\x20\x20\x20 {kern} \\\n\
         \x20\x20\x20\x20\x20\x20 {bc} \\\n\
         \x20\x20\x20\x20\x20\x20 root=/dev/dm-0 rootwait ro \\\n\
         \x20\x20\x20\x20\x20\x20 raid=noautodetect \\\n\
         \x20\x20\x20\x20\x20\x20 random.trust_cpu=on \\\n\
         \x20\x20\x20\x20\x20\x20 selinux=1 enforcing=1 \\\n\
         \x20\x20\x20\x20\x20\x20 dm-mod.create=\"$dm_verity_root\" \\\n\
         \x20\x20\x20\x20\x20\x20 -- \\\n\
         \x20\x20\x20\x20\x20\x20 systemd.log_target=journal-or-kmsg \\\n\
         \x20\x20\x20\x20\x20\x20 systemd.log_color=0 \\\n\
         \x20\x20\x20\x20\x20\x20 systemd.show_status=true\n\
         \x20\x20 {initrd}\n\
         \x20\x20 boot\n\
         {fallback}}}\n",
        header = header,
        dvr = opts.dm_verity_root,
        pretty = opts.pretty_name,
        ver = opts.version_id,
        kern = opts.kernel_parameters,
        bc = bootconfig,
        initrd = initrd,
        fallback = fallback,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_boot_cfg_has_hardening() {
        let text = render_grub_cfg(&GrubCfgOptions {
            pretty_name: "Bottlerocket",
            version_id: "1.0",
            kernel_parameters: "quiet",
            dm_verity_root: "root,,,ro,0 ...",
            uefi_secure_boot: true,
            first_party_stack: true,
        });
        assert!(text.contains("set superusers=\"\""));
        assert!(text.contains("set check_signatures=\"no\""));
        assert!(text.contains("bootconfig"));
        assert!(text.contains("initrd ($private)/bootconfig.data"));
        assert!(text.contains("rebooting in 30 seconds"));
    }

    #[test]
    fn resolve_vendor_obj_uses_auto_when_none() {
        let shim = Path::new("/efi/EFI/BOOT/bootx64.efi");
        let auto = Path::new("/scratch/efi-mount/vendor.obj");
        let got = resolve_vendor_obj(None, shim, auto).unwrap();
        assert_eq!(got, auto);
    }

    #[test]
    fn resolve_vendor_obj_uses_explicit_when_distinct() {
        let shim = Path::new("/efi/EFI/BOOT/bootx64.efi");
        let auto = Path::new("/scratch/efi-mount/vendor.obj");
        let explicit = Path::new("/other/vendor.obj");
        let got = resolve_vendor_obj(Some(explicit), shim, auto).unwrap();
        assert_eq!(got, explicit);
    }

    #[test]
    fn resolve_vendor_obj_rejects_shim_alias() {
        // A caller mistakenly points vendor_obj at the shim binary
        // itself: `zero_shim` would overwrite the shim before objcopy
        // runs. Reject with a typed error.
        let shim = Path::new("/efi/EFI/BOOT/bootx64.efi");
        let auto = Path::new("/scratch/efi-mount/vendor.obj");
        let err = resolve_vendor_obj(Some(shim), shim, auto).unwrap_err();
        assert!(
            err.to_string().contains("aliases shim binary"),
            "err: {err}"
        );
    }

    #[test]
    fn stripped_no_bootconfig() {
        let text = render_grub_cfg(&GrubCfgOptions {
            pretty_name: "Bottlerocket",
            version_id: "1.0",
            kernel_parameters: "quiet",
            dm_verity_root: "root,,,ro,0 ...",
            uefi_secure_boot: false,
            first_party_stack: false,
        });
        // Stripped variants omit the bootconfig / initrd lines
        // (they contain nothing but whitespace + a backslash).
        assert!(!text.contains("initrd ("));
        assert!(!text.contains("set superusers"));
    }
}
