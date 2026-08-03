//! DNF/RPM-based rootfs staging. Ports the `rpm -iv --root ...` and
//! `dnf install --installroot` invocations that `rpm2img` / `rpm2eif`
//! use to populate the rootfs.
//!
//! Two install primitives live here:
//!
//! - [`install_rpms`] — force-install every `*.rpm` under a directory.
//!   Used only when the caller has *already* narrowed the RPM set (via
//!   dnf resolution), matching old-twoliter's `rpm2img` step where
//!   `PACKAGE_DIR=/local/rpms` contains only the packages
//!   `dnf install --downloadonly` selected.
//!
//! - [`install_rpms_by_name`] — the resolving primitive. Builds a
//!   `createrepo_c` index over the staged RPM directory and runs
//!   `dnf install --installroot=<root>` with a list of package short
//!   names. This is what a `variant-rootfs` transform should call
//!   when it has an `included-packages` name list: it filters the
//!   union of every RPM the kits + packages + metadata staged down
//!   to just the ones actually named (plus their deps).
//!
//! Skeleton: subprocess sequences are wired but not exercised in-crate
//! (require a real dnf/rpm plus a package cache).

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::image::error::{Error, Result};
use crate::image::vfs_ext::{list_files_recursive, shell_quote_path};

/// Enumerate every `*.rpm` under `dir` recursively inside the
/// environment, sorted. Returns an empty vec when nothing matches;
/// errors if the directory itself is missing or unreadable.
///
/// The recursive walk is deliberate: package and kit-image sources
/// extract RPMs into a nested layout (`Packages/<pkg>/[<arch>/]…`)
/// rather than dropping them flat under the staging root, so a
/// non-recursive `-maxdepth 1` walk would miss every RPM the transform
/// staged.
async fn collect_rpms(vfs: &Vfs, dir: &Path) -> Result<Vec<PathBuf>> {
    list_files_recursive(vfs, dir, "*.rpm").await
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

/// Build a local repo index over `package_dir` and dnf-install the
/// named packages into `install_root`.
///
/// `package_names` are dnf-resolvable package selectors — typically
/// short names like `"kernel-6.12"` that dnf will match against
/// `%{name}` in the local repo's metadata. Callers add the
/// `bottlerocket-` prefix if the SDK convention requires it (the old
/// Dockerfile did this at the shell level:
/// `printf "bottlerocket-%s\n" metadata ...`).
///
/// This is the fix for the "`rpm -iv <every-rpm-in-dir>`" fallback:
/// when a kit ships mutually-exclusive alternatives (e.g.
/// `iptables-legacy` and `iptables-nft`, or `docker-engine-25` and
/// `docker-engine-29`), force-installing every RPM triggers RPM
/// conflicts. Selecting by name lets dnf pick exactly the variant
/// the caller asked for and resolve its deps.
///
/// Steps performed:
///
///  1. `createrepo_c` against `package_dir` (idempotent; adds a
///     `repodata/` subdir to the staged tree).
///  2. `dnf install --installroot=<root> --disablerepo='*'
///     --repofrompath=local,<package_dir> --enablerepo=local
///     --nogpgcheck --forcearch=<arch> --assumeyes <names…>`.
///
/// The caller is responsible for supplying `arch` because dnf's
/// arch-detection would otherwise use the host arch and refuse to
/// install cross-built RPMs.
pub async fn install_rpms_by_name(
    vfs: &Vfs,
    install_root: &Path,
    package_dir: &Path,
    package_names: &[String],
    arch: &str,
) -> Result<()> {
    if package_names.is_empty() {
        return Err(Error::InvalidLayout {
            context: "install_rpms_by_name".into(),
            reason: "no package names supplied".into(),
        });
    }

    // Step 1: index the local repo.
    let package_dir_q = shell_quote_path(package_dir);
    vfs.command(
        "createrepo_local",
        "createrepo_c",
        &[
            "-o".to_string(),
            package_dir_q.clone(),
            "-x".to_string(),
            "'*-debuginfo-*.rpm'".to_string(),
            "-x".to_string(),
            "'*-debugsource-*.rpm'".to_string(),
            "--no-database".to_string(),
            package_dir_q.clone(),
        ],
    )
    .await?;

    // Step 2: dnf install into the rootfs.
    let install_root_q = shell_quote_path(install_root);
    let repofrompath = format!("--repofrompath=local,{package_dir_q}");
    let installroot = format!("--installroot={install_root_q}");
    let forcearch = format!("--forcearch={arch}");

    let mut argv: Vec<String> = vec![
        "--assumeyes".into(),
        "--disablerepo=*".into(),
        repofrompath,
        "--enablerepo=local".into(),
        "--nogpgcheck".into(),
        forcearch,
        installroot,
        "install".into(),
    ];
    for name in package_names {
        argv.push(crate::image::vfs_ext::shell_quote(name));
    }

    vfs.command("dnf_install_rootfs", "dnf", &argv).await?;
    Ok(())
}

/// Query installed RPMs. Returns raw stdout from
/// `rpm -qa --root <root> --queryformat <fmt>` for the caller to parse.
/// Ports the `rpm -qa` call inside `generate_application_inventory`.
pub async fn queryformat(vfs: &Vfs, install_root: &Path, queryformat: &str) -> Result<Vec<u8>> {
    let install_root_q = shell_quote_path(install_root);
    let queryformat_q = crate::image::vfs_ext::shell_quote(queryformat);
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
