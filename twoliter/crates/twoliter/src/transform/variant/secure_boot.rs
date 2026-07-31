//! UEFI grub-install + secure-boot signing sequence, shared by the
//! `variant-grub` transform.
//!
//! Extracted from `grub.rs` because the "when uefi_secure_boot=true,
//! locate sbkeys/, run Sbsetup, then feed every signed cert path into
//! `install_grub_uefi`" sequence is dense enough that inlining it in
//! the transform body pushed the file over the 500-line gate. The
//! logic is a straight port of the pre-decomposition monolith at
//! `variant.rs::1176-1240` in the pre-refactor tree.

use crate::image::{
    Arch, ImageFeatures,
    grub::{GrubUefiInstall, install_grub_uefi},
    secure_boot::{Sbsetup, SbsetupProfile},
};
use edo::environment::Vfs;
use snafu::ResultExt;
use std::path::{Path, PathBuf};

use super::error;

/// Run the UEFI grub install (with optional secure-boot signing) on
/// the given `os_disk` + `root_mount`.
///
/// When `features.uefi_secure_boot` is on, the caller must have
/// staged `sbkeys` into `deps/sbkeys/` first — we surface a typed
/// [`error::Error::BuildStep`] if it's missing.
pub async fn install_uefi_with_secure_boot(
    vfs: &Vfs,
    features: &ImageFeatures,
    sbkeys_source_present: bool,
    root_mount_path: &Path,
    os_disk: &Path,
    efi_mount: &Path,
    efi_scratch: &Path,
    efi_partition_mib: u32,
    efi_offset_mib: u32,
    arch: Arch,
) -> Result<(), error::Error> {
    let sbkeys_dir = if features.uefi_secure_boot {
        if !sbkeys_source_present {
            return Err(error::Error::BuildStep {
                step: "uefi-secure-boot".into(),
                reason: "uefi_secure_boot=true requires an `sbkeys` source scope on the \
                         transform element; declare `source = { sbkeys = [\"//addr\"] }` \
                         alongside the SDK addr."
                    .into(),
            });
        }
        Some(PathBuf::from("deps/sbkeys"))
    } else {
        None
    };

    let sb_vendor_cert_der = sbkeys_dir.as_ref().map(|d| d.join("vendor.cer"));
    let sb_vendor_cer = sb_vendor_cert_der.clone();
    let sb_db_cer = sbkeys_dir.as_ref().map(|d| d.join("db.cer"));
    let sb_config_sign_pubkey = sbkeys_dir.as_ref().map(|d| d.join("config-sign.pub"));
    let sb_db_crt = sbkeys_dir.as_ref().map(|d| d.join("db.crt"));

    if features.uefi_secure_boot {
        let dir = sbkeys_dir
            .clone()
            .expect("sbkeys_dir Some when uefi_secure_boot=true");
        let sbsetup = Sbsetup {
            profile: SbsetupProfile::Local,
            sbkeys_dir: dir,
            aws_ca_bundle: None,
        };
        sbsetup.apply(vfs).await.context(error::BrImageSnafu)?;
    }

    install_grub_uefi(
        vfs,
        &GrubUefiInstall {
            root_mount: root_mount_path,
            os_image: os_disk,
            efi_mount,
            efi_scratch,
            efi_partition_mib,
            efi_offset_mib,
            uefi_secure_boot: features.uefi_secure_boot,
            secure_boot_setup: None,
            vendor_cert_der: sb_vendor_cert_der.as_deref(),
            vendor_obj: None,
            db_cer: sb_db_cer.as_deref(),
            vendor_cer: sb_vendor_cer.as_deref(),
            config_sign_pubkey: sb_config_sign_pubkey.as_deref(),
            db_crt: sb_db_crt.as_deref(),
            db_cer_for_esp: sb_db_cer.as_deref(),
            arch,
        },
    )
    .await
    .context(error::BrImageSnafu)?;
    Ok(())
}
