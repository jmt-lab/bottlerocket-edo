//! GRUB installation and grub.cfg emission (non-signing paths). The
//! signing paths (`sign_grub`, `unembed_grub`, `sign_grubcfg`) live in
//! [`crate::secure_boot`].

use std::path::Path;

use edo::environment::Vfs;

use crate::error::Result;
use crate::model::Arch;
use crate::vfs_ext::{shell_quote, shell_quote_path};

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

/// Options for [`render_grub_cfg`].
#[derive(Debug, Clone)]
pub struct GrubCfgOptions<'a> {
    pub pretty_name: &'a str,
    pub version_id: &'a str,
    /// Space-separated kernel parameters.
    pub kernel_parameters: &'a str,
    /// The rendered `DM_VERITY_ROOT` value (see
    /// [`crate::verity::VerityInfo::dm_verity_root_row`]).
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
