//! DNF/RPM-based rootfs staging. Ports the `rpm -iv --root ...` and
//! `dnf install --installroot` invocations that `rpm2img` / `rpm2eif`
//! use to populate the rootfs.
//!
//! Skeleton: subprocess sequences are wired but not exercised in-crate
//! (require a real dnf/rpm plus a package cache).

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::error::{Error, Result};
use crate::vfs_ext::{list_files, shell_quote_path};

/// Enumerate every `*.rpm` under `dir` (non-recursive) inside the
/// environment, sorted. Returns an empty vec when the directory is
/// empty; errors if the directory itself is missing or unreadable.
async fn collect_rpms(vfs: &Vfs, dir: &Path) -> Result<Vec<PathBuf>> {
    list_files(vfs, dir, "*.rpm").await
}

/// Install every `*.rpm` under `package_dir` into `install_root` via
/// `rpm -iv --ignorearch --root`. Ports the head of `rpm2img` (L211).
///
/// The shell version relied on the caller's `*.rpm` glob expansion; we
/// enumerate the directory in Rust because `vfs.command` does not
/// invoke a shell (only `sh -c` at the wrapper level), so a literal
/// `*.rpm` would either be expanded against the wrong cwd or passed
/// verbatim to `rpm` — neither of which is what we want.
pub async fn install_rpms(vfs: &Vfs, install_root: &Path, package_dir: &Path) -> Result<()> {
    let rpms = collect_rpms(vfs, package_dir).await?;
    if rpms.is_empty() {
        return Err(Error::InvalidLayout {
            context: "install_rpms".into(),
            reason: format!("no `*.rpm` files found under {}", package_dir.display()),
        });
    }
    let install_root_q = shell_quote_path(install_root);
    let rpm_qs: Vec<String> = rpms.iter().map(|p| shell_quote_path(p)).collect();
    let mut argsv: Vec<&str> = vec!["-iv", "--ignorearch", "--root", install_root_q.as_str()];
    for r in &rpm_qs {
        argsv.push(r.as_str());
    }
    vfs.command("install_rpms", "rpm", &argsv).await?;
    Ok(())
}

/// Install SBOM RPMs into a scratch mount so their contents can be
/// merged into the image SBOM.  Ports the SBOM-install steps in
/// `rpm2img` (L292-304).
///
/// Same shell-glob caveat as [`install_rpms`]: the directory is
/// enumerated in Rust and passed as separate positional arguments.
pub async fn install_sbom_rpms(
    vfs: &Vfs,
    scratch_root: &Path,
    sbom_package_dir: &Path,
) -> Result<()> {
    vfs.create_dir(&scratch_root.join("var/lib/rpm")).await?;
    let rpms = collect_rpms(vfs, sbom_package_dir).await?;
    if rpms.is_empty() {
        return Err(Error::InvalidLayout {
            context: "install_sbom_rpms".into(),
            reason: format!(
                "no `*.rpm` files found under {}",
                sbom_package_dir.display()
            ),
        });
    }
    let scratch_root_q = shell_quote_path(scratch_root);
    let rpm_qs: Vec<String> = rpms.iter().map(|p| shell_quote_path(p)).collect();
    let mut argsv: Vec<&str> = vec!["-iv", "--ignorearch", "--root", scratch_root_q.as_str()];
    for r in &rpm_qs {
        argsv.push(r.as_str());
    }
    vfs.command("install_sbom_rpms", "rpm", &argsv).await?;
    Ok(())
}

/// Query installed RPMs. Returns raw stdout from
/// `rpm -qa --root <root> --queryformat <fmt>` for the caller to parse.
/// Ports the `rpm -qa` call inside `generate_application_inventory`.
pub async fn queryformat(vfs: &Vfs, install_root: &Path, queryformat: &str) -> Result<Vec<u8>> {
    let install_root_q = shell_quote_path(install_root);
    let queryformat_q = crate::vfs_ext::shell_quote(queryformat);
    let out = vfs
        .output(
            "rpm_queryformat",
            "rpm",
            &[
                "-qa",
                "--root",
                install_root_q.as_str(),
                "--queryformat",
                queryformat_q.as_str(),
            ],
        )
        .await?;
    Ok(out)
}
