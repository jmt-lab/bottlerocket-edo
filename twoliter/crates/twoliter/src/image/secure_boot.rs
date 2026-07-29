//! Secure Boot key material, cert-table encoding, and shim/grub
//! signing.
//!
//! Ports `cert_table`, `sbsetup_*`, `sign_shim`, `zero_shim`,
//! `sign_mokm`, `sign_grub`, `unembed_grub`, `provide_certs`,
//! `sign_vmlinuz`, `sign_grubcfg`, and `generate_hmac` from the shell's
//! `imghelper`. The pure part — [`cert_table`] — is fully implemented
//! and unit-tested here. The signing entry points are typed function
//! signatures with the correct subprocess sequences; running them
//! requires an SDK image with `openssl`, `gpg`, `pesign`, `certutil`,
//! `mcopy`, `pk12util`, and an `${ARCH}-bottlerocket-linux-gnu-objcopy`
//! toolchain on `PATH` inside the transform environment.

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::image::error::{Error, Result};
use crate::image::model::Arch;
use crate::image::vfs_ext::{remove_if_present, shell_quote, shell_quote_path};

/// Fixed size of the cert-table blob, per shim's expectations.
pub const CERT_TABLE_SIZE: usize = 4096;

/// Build the shim `.vendor_cert` cert-table blob.
///
/// The shim binary expects a header followed by a payload:
/// ```text
/// struct {
///   uint32_t vendor_authorized_size;      // = payload.len()
///   uint32_t vendor_deauthorized_size;    // = 0
///   uint32_t vendor_authorized_offset;    // = 16 (sizeof header)
///   uint32_t vendor_deauthorized_offset;  // = 16 + payload.len()
/// } cert_table;
/// ```
///
/// The output is zero-padded to exactly [`CERT_TABLE_SIZE`] bytes so
/// subsequent `objcopy --update-section` calls always fit. Passing an
/// empty payload produces the "no vendor cert" blob that
/// `imghelper::zero_shim` writes.
///
/// Returns [`Error::InvalidLayout`] if `payload.len() + 16 > CERT_TABLE_SIZE`.
pub fn cert_table(payload: &[u8]) -> Result<Vec<u8>> {
    let max_payload = CERT_TABLE_SIZE - 16;
    if payload.len() > max_payload {
        return Err(Error::InvalidLayout {
            context: "cert_table".into(),
            reason: format!(
                "vendor cert too large: {} bytes (max {})",
                payload.len(),
                max_payload
            ),
        });
    }

    let size = payload.len() as u32;
    let offset: u32 = 16;
    let mut out = Vec::with_capacity(CERT_TABLE_SIZE);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&(size + offset).to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(CERT_TABLE_SIZE, 0);
    Ok(out)
}

/// Which signing profile the build is running under.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SbsetupProfile {
    /// AWS KMS PKCS#11 signing.
    Aws,
    /// Local pesign / certutil signing.
    Local,
}

/// Reserved key material paths ("$HOME/sbkeys"). Callers pass this in
/// so `br-image` doesn't need to consult the environment.
#[derive(Debug, Clone)]
pub struct Sbsetup {
    pub profile: SbsetupProfile,
    /// Directory containing PK/KEK/db/vendor .crt files and signing keys.
    pub sbkeys_dir: PathBuf,
    /// AWS CA-bundle override, if the caller has one.
    pub aws_ca_bundle: Option<PathBuf>,
}

impl Sbsetup {
    /// Apply the profile: convert PEM certs to DER, import signing keys
    /// into pesign, and (for AWS) verify STS access. Runs many
    /// subprocesses; ports `sbsetup_wrapup` + `sbsetup_aws_profile` /
    /// `sbsetup_local_profile`.
    pub async fn apply(&self, vfs: &Vfs) -> Result<()> {
        // Convert PEM certs to DER; done for every profile.
        for cert in ["PK", "KEK", "db", "vendor"] {
            let pem = self.sbkeys_dir.join(format!("{cert}.crt"));
            let der = self.sbkeys_dir.join(format!("{cert}.cer"));
            let pem_q = shell_quote_path(&pem);
            let der_q = shell_quote_path(&der);
            vfs.command(
                "sbsetup_openssl",
                "openssl",
                &[
                    "x509",
                    "-inform",
                    "PEM",
                    "-in",
                    pem_q.as_str(),
                    "-outform",
                    "DER",
                    "-out",
                    der_q.as_str(),
                ],
            )
            .await?;
        }

        // Import gpg key used to sign grub.cfg (config-sign.key).
        let key = self.sbkeys_dir.join("config-sign.key");
        let key_q = shell_quote_path(&key);
        vfs.command(
            "sbsetup_gpg_import",
            "gpg",
            &["--batch", "--no-tty", "--import", key_q.as_str()],
        )
        .await?;

        // TODO: profile-specific certutil/pk12util or AWS credential
        // wiring. This is a defer-till-transform-bringup skeleton;
        // wiring the full local profile requires binding to file paths
        // outside `${HOME}/.aws`.
        Ok(())
    }
}

/// Sign the shim binary with the `db` key, first replacing its embedded
/// vendor cert with `cert_table` output. Skeleton: dispatches to
/// `objcopy` + `pesign`.
pub async fn sign_shim(
    vfs: &Vfs,
    shim: &Path,
    arch: Arch,
    vendor_cert_der: &Path,
    vendor_obj: &Path,
    db_cer: &Path,
) -> Result<()> {
    // Build the cert-table blob into `vendor_obj`.
    let payload = vfs.read(vendor_cert_der).await?;
    let blob = cert_table(&payload)?;
    vfs.write(vendor_obj, &blob).await?;

    undo_sign(vfs, shim, arch).await?;
    objdumpcopy(
        vfs,
        arch,
        shim,
        &[
            "--update-section".to_string(),
            shell_quote(&format!(".vendor_cert={}", vendor_obj.display())),
        ],
    )
    .await?;
    // Note: the shim-sign-key argument list is profile-specific and
    // provided by the caller once `Sbsetup` is applied. Skeleton here
    // stops before the `pesign -s` call.
    do_sign(vfs, shim, db_cer, &["shim-sign-key".to_string()]).await
}

/// Wipe the shim's vendor cert with a zero-sized blob (used when
/// UEFI_SECURE_BOOT=no). Skeleton.
pub async fn zero_shim(vfs: &Vfs, shim: &Path, arch: Arch, vendor_obj: &Path) -> Result<()> {
    let blob = cert_table(&[])?;
    vfs.write(vendor_obj, &blob).await?;
    objdumpcopy(
        vfs,
        arch,
        shim,
        &[
            "--update-section".to_string(),
            shell_quote(&format!(".vendor_cert={}", vendor_obj.display())),
        ],
    )
    .await
}

/// Sign the MokManager binary with the `vendor` key. Skeleton.
pub async fn sign_mokm(vfs: &Vfs, mokm: &Path, arch: Arch, vendor_cer: &Path) -> Result<()> {
    undo_sign(vfs, mokm, arch).await?;
    do_sign(vfs, mokm, vendor_cer, &["code-sign-key".to_string()]).await
}

/// Replace grub's embedded GPG public key with the code-signing pubkey
/// and re-sign with `vendor`. Skeleton.
pub async fn sign_grub(
    vfs: &Vfs,
    grub: &Path,
    arch: Arch,
    config_sign_pubkey: &Path,
    vendor_cer: &Path,
) -> Result<()> {
    undo_sign(vfs, grub, arch).await?;
    objdumpcopy(
        vfs,
        arch,
        grub,
        &[
            "--file-alignment".to_string(),
            "4096".to_string(),
            "--update-section".to_string(),
            shell_quote(&format!(".pubkey={}", config_sign_pubkey.display())),
        ],
    )
    .await?;
    do_sign(vfs, grub, vendor_cer, &["code-sign-key".to_string()]).await
}

/// Remove the embedded GPG public key from grub (used when
/// UEFI_SECURE_BOOT=no). Skeleton.
pub async fn unembed_grub(vfs: &Vfs, grub: &Path, arch: Arch) -> Result<()> {
    objdumpcopy(
        vfs,
        arch,
        grub,
        &[
            "--file-alignment".to_string(),
            "4096".to_string(),
            "--remove-section".to_string(),
            ".pubkey".to_string(),
        ],
    )
    .await
}

/// Copy the db PEM+DER onto the EFI system partition (`::/EFI/BOOT`) so
/// operators can import them via firmware setup on bare metal.
/// Skeleton.
pub async fn provide_certs(
    vfs: &Vfs,
    efi_image: &Path,
    db_crt: &Path,
    db_cer: &Path,
) -> Result<()> {
    let efi_image_q = shell_quote_path(efi_image);
    let db_crt_q = shell_quote_path(db_crt);
    let db_cer_q = shell_quote_path(db_cer);
    vfs.command(
        "provide_certs_mcopy",
        "mcopy",
        &[
            "-i",
            efi_image_q.as_str(),
            "-ov",
            db_crt_q.as_str(),
            db_cer_q.as_str(),
            "::/EFI/BOOT",
        ],
    )
    .await?;
    Ok(())
}

/// Sign `vmlinuz` with the `vendor` key. Skeleton.
pub async fn sign_vmlinuz(vfs: &Vfs, vmlinuz: &Path, arch: Arch, vendor_cer: &Path) -> Result<()> {
    undo_sign(vfs, vmlinuz, arch).await?;
    do_sign(vfs, vmlinuz, vendor_cer, &["code-sign-key".to_string()]).await
}

/// Compute the FIPS HMAC file that ships next to `vmlinuz`. Returns the
/// raw stdout of `openssl sha512 -hmac ... -hex ...` for the caller to
/// parse (the shell version pipes into awk to rewrite the label; the
/// transform driver captures our stdout and does the rewrite in Rust
/// before writing the file).
pub async fn generate_hmac(vfs: &Vfs, vmlinuz: &Path) -> Result<Vec<u8>> {
    let vmlinuz_q = shell_quote_path(vmlinuz);
    let out = vfs
        .output(
            "generate_hmac",
            "openssl",
            &[
                "sha512",
                "-hmac",
                "FIPS-FTW-RHT2009",
                "-hex",
                vmlinuz_q.as_str(),
            ],
        )
        .await?;
    Ok(out)
}

/// Detached-sign `grub.cfg` with the config-signing GPG key. Skeleton.
pub async fn sign_grubcfg(vfs: &Vfs, grub_cfg: &Path) -> Result<()> {
    // Remove any stale detached sig (`<basename>.sig`); ignore missing.
    // `gpg --detach-sign FOO.cfg` writes to `FOO.cfg.sig`, appending the
    // literal `.sig` — it does NOT replace the existing extension.
    let sig = companion(grub_cfg, "sig");
    remove_if_present(vfs, &sig).await?;

    let grub_cfg_q = shell_quote_path(grub_cfg);
    vfs.command(
        "sign_grubcfg",
        "gpg",
        &["--batch", "--no-tty", "--detach-sign", grub_cfg_q.as_str()],
    )
    .await?;
    Ok(())
}

/// A companion path used to stage a rewritten PE image next to the
/// original. `what` and the returned path always share a parent
/// directory so the final `rename` is atomic on the same filesystem.
fn companion(what: &Path, suffix: &str) -> PathBuf {
    let name = what
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    let mut buf = name;
    buf.push(".");
    buf.push(suffix);
    let mut path = what.to_path_buf();
    path.set_file_name(buf);
    path
}

/// Best-effort restore of `original` from `backup`, invoked on the
/// error path so callers see a consistent on-disk state and can retry
/// the transform.
async fn restore_from_backup(vfs: &Vfs, original: &Path, backup: &Path) {
    // If `original` already exists (mid-operation crash), leave it in
    // place and drop the backup. Otherwise move the backup back.
    match vfs.try_exists(original).await {
        Ok(true) => {
            let _ = remove_if_present(vfs, backup).await;
        }
        Ok(false) => {
            let _ = vfs.rename(backup, original).await;
        }
        Err(_) => {
            // Environment error while checking — leave the tree alone;
            // the environment's log has already recorded the failure.
        }
    }
}

/// Strip any existing PE signature from `what`.
///
/// The shell version moves `what` to `what.orig`, runs
/// `pesign -r -i what.orig -o what -u 0`, then unlinks `what.orig`. The
/// Rust port preserves that shape but always restores `what` on any
/// failure so callers can retry.
async fn undo_sign(vfs: &Vfs, what: &Path, _arch: Arch) -> Result<()> {
    let backup = companion(what, "orig");
    vfs.rename(what, &backup).await?;

    let backup_q = shell_quote_path(&backup);
    let what_q = shell_quote_path(what);
    if let Err(e) = vfs
        .command(
            "undo_sign",
            "pesign",
            &[
                "-i",
                backup_q.as_str(),
                "-o",
                what_q.as_str(),
                "-u",
                "0",
                "-r",
            ],
        )
        .await
    {
        // Leave the tree in the state it started in.
        restore_from_backup(vfs, what, &backup).await;
        return Err(e.into());
    }
    // pesign succeeded; drop the backup. Ignore failures to unlink.
    let _ = remove_if_present(vfs, &backup).await;
    Ok(())
}

/// PE-sign `what` with the specified key, then verify.
///
/// Uses a tempfile-based staging pattern: `pesign` writes to
/// `what.signed`; on success we `rename` it over `what`. On any failure
/// after the `pesign` call, `what.signed` is removed so a retry starts
/// from a clean state.
async fn do_sign(vfs: &Vfs, what: &Path, cert: &Path, sign_key_args: &[String]) -> Result<()> {
    let signed_path = companion(what, "signed");
    let what_q = shell_quote_path(what);
    let signed_q = shell_quote_path(&signed_path);
    let cert_q = shell_quote_path(cert);
    // Sign-key args are caller-supplied strings; quote each so pesign
    // sees them as one token apiece even if they contain spaces.
    let key_qs: Vec<String> = sign_key_args.iter().map(|s| shell_quote(s)).collect();

    let mut argsv: Vec<&str> = vec!["-i", what_q.as_str(), "-o", signed_q.as_str(), "-s"];
    for k in &key_qs {
        argsv.push(k.as_str());
    }
    if let Err(e) = vfs.command("do_sign_pesign", "pesign", &argsv).await {
        // `pesign` may have left a partial `what.signed` around; unlink it.
        let _ = remove_if_present(vfs, &signed_path).await;
        return Err(e.into());
    }
    // Rename `signed_path` over `what`.
    if let Err(e) = vfs.rename(&signed_path, what).await {
        let _ = remove_if_present(vfs, &signed_path).await;
        return Err(e.into());
    }
    vfs.command(
        "do_sign_pesigcheck",
        "pesigcheck",
        &["-i", what_q.as_str(), "-n", "0", "-c", cert_q.as_str()],
    )
    .await?;
    Ok(())
}

/// Wrap `objcopy` in the log-before / log-after `objdump -h` pattern
/// used by `imghelper::${arch}-objdumpcopy`. Skeleton.
async fn objdumpcopy(vfs: &Vfs, arch: Arch, obj: &Path, extra_args: &[String]) -> Result<()> {
    // `extra_args` values are caller-supplied strings; some of them
    // are pre-quoted `--update-section=...` blobs built by sign_shim /
    // sign_grub above (where the value's path already went through
    // `shell_quote`). Others are fixed tokens like `--file-alignment`
    // or `4096`. Both survive `sh -c` intact because our quoter is a
    // no-op change for tokens that contain no metacharacters and
    // idempotent for ones already quoted.
    let objcopy = format!("{arch}-bottlerocket-linux-gnu-objcopy");
    let objdump = format!("{arch}-bottlerocket-linux-gnu-objdump");
    let obj_q = shell_quote_path(obj);

    // objdump -h <obj>
    vfs.command("objdumpcopy_before", &objdump, &["-h", obj_q.as_str()])
        .await?;

    // objcopy <extra> <obj>
    let mut argsv: Vec<&str> = extra_args.iter().map(|s| s.as_str()).collect();
    argsv.push(obj_q.as_str());
    vfs.command("objdumpcopy_objcopy", &objcopy, &argsv).await?;

    // objdump -h <obj>
    vfs.command("objdumpcopy_after", &objdump, &["-h", obj_q.as_str()])
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_table_zero_payload_is_all_zero_after_header() {
        let out = cert_table(&[]).unwrap();
        assert_eq!(out.len(), CERT_TABLE_SIZE);
        // Header: size=0, dsize=0, offset=16, deoffset=16.
        assert_eq!(&out[0..4], &0u32.to_le_bytes());
        assert_eq!(&out[4..8], &0u32.to_le_bytes());
        assert_eq!(&out[8..12], &16u32.to_le_bytes());
        assert_eq!(&out[12..16], &16u32.to_le_bytes());
        // Padding is all zero.
        assert!(out[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn cert_table_small_payload_layout() {
        let payload = b"\x11\x22\x33\x44\x55";
        let out = cert_table(payload).unwrap();
        assert_eq!(out.len(), CERT_TABLE_SIZE);
        // size = 5, dsize = 0, offset = 16, deoffset = 16 + 5 = 21.
        assert_eq!(&out[0..4], &5u32.to_le_bytes());
        assert_eq!(&out[4..8], &0u32.to_le_bytes());
        assert_eq!(&out[8..12], &16u32.to_le_bytes());
        assert_eq!(&out[12..16], &21u32.to_le_bytes());
        assert_eq!(&out[16..21], payload);
        assert!(out[21..].iter().all(|&b| b == 0));
    }

    #[test]
    fn cert_table_realistic_payload() {
        // 1024-byte payload. Fits inside 4096 - 16 header.
        let payload = vec![0xab; 1024];
        let out = cert_table(&payload).unwrap();
        assert_eq!(out.len(), CERT_TABLE_SIZE);
        assert_eq!(&out[0..4], &1024u32.to_le_bytes());
        assert_eq!(&out[12..16], &(1024u32 + 16).to_le_bytes());
        assert_eq!(&out[16..16 + 1024], payload.as_slice());
        assert!(out[16 + 1024..].iter().all(|&b| b == 0));
    }

    #[test]
    fn cert_table_rejects_oversized_payload() {
        let err = cert_table(&vec![0u8; CERT_TABLE_SIZE - 15]).unwrap_err();
        assert!(matches!(err, Error::InvalidLayout { .. }));
        assert!(err.to_string().contains("vendor cert too large"));
    }

    #[test]
    fn companion_appends_suffix_to_filename() {
        // The companion sits next to the original so a rename is atomic.
        assert_eq!(
            companion(Path::new("/tmp/shim.efi"), "orig"),
            Path::new("/tmp/shim.efi.orig")
        );
        assert_eq!(
            companion(Path::new("grub.cfg"), "sig"),
            Path::new("grub.cfg.sig")
        );
        // No extension is fine.
        assert_eq!(
            companion(Path::new("/tmp/vmlinuz"), "signed"),
            Path::new("/tmp/vmlinuz.signed")
        );
    }
}
