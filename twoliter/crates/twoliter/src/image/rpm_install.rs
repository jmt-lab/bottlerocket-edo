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
//!   `createrepo_c` index over the staged RPM directory, runs
//!   `dnf install --downloadonly` (with a throwaway `--installroot`
//!   so dnf's rpmdb setup stays out of the real rootfs) to resolve
//!   and download the requested names plus their deps, then invokes
//!   [`install_rpms`] to force-install the resolved set into the
//!   real (empty) rootfs. This is what a `variant-rootfs` transform
//!   should call when it has an `included-packages` name list: it
//!   filters the union of every RPM the kits + packages + metadata
//!   staged down to just the ones actually named (plus their deps),
//!   without letting dnf pre-create `/usr` in the target rootfs.
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
///
/// We pass `--define '_dbpath /var/lib/rpm'` to redirect rpm's DB out
/// of the SDK-default `/usr/lib/sysimage/rpm`. If the DB is created
/// under `/usr/lib/sysimage/rpm/` **before** the `bottlerocket-
/// filesystem` RPM's CPIO payload tries to extract `/usr`, rpm's
/// existing-directory reconciliation falls into the `;<hex>` rename
/// path and fails with `cpio: File from package already exists as a
/// directory`. Old-twoliter avoids this by injecting the same macro
/// in the SDK builder image via `build.Dockerfile` L308
/// (`echo '%_dbpath %{_sharedstatedir}/rpm' >> /etc/rpm/macros`); we
/// re-apply the identical override on every `rpm` invocation instead
/// of mutating SDK state.
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
    let mut argsv: Vec<&str> = vec![
        "-iv",
        "--ignorearch",
        "--define",
        "'_dbpath /var/lib/rpm'",
        "--root",
        install_root_q.as_str(),
    ];
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
    // See `install_rpms` for why `_dbpath` is redirected off
    // `/usr/lib/sysimage/rpm`. Same reasoning applies to the SBOM
    // scratch root: `bottlerocket-*-sbom` RPMs also ship `/usr`.
    let mut argsv: Vec<&str> = vec![
        "-iv",
        "--ignorearch",
        "--define",
        "'_dbpath /var/lib/rpm'",
        "--root",
        scratch_root_q.as_str(),
    ];
    for r in &rpm_qs {
        argsv.push(r.as_str());
    }
    vfs.command("install_sbom_rpms", "rpm", &argsv).await?;
    Ok(())
}

/// Build a local repo index over `package_dir`, resolve the named
/// packages (plus their dependency closure) against it, download the
/// resolved RPM set, then force-install that set into `install_root`.
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
///  2. `dnf --assumeyes --disablerepo='*'
///     --repofrompath=local,<package_dir> --enablerepo=local
///     --nogpgcheck --forcearch=<arch>
///     --setopt=cachedir=<downloads> install --downloadonly
///     <names…>` — resolves the transaction and downloads the RPMs
///     into a scratch cache directory without touching
///     `install_root`. Matches old-twoliter's `build.Dockerfile`
///     `repobuild` stage exactly (no `--installroot`, no
///     `--destdir` — the latter is not accepted by `dnf install` in
///     the SDK's dnf5 build).
///  3. `rpm -iv --ignorearch --root=<install_root>
///     <downloads>/*.rpm` — force-installs the pre-resolved set into
///     the *empty* rootfs. Matches old-twoliter's `rpm2img` line 211,
///     where the filesystem package is the first to create top-level
///     directories like `/usr`, `/bin`, etc.
///
/// The caller is responsible for supplying `arch` because dnf's
/// arch-detection would otherwise use the host arch and refuse to
/// resolve cross-built RPMs.
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

    // Step 2: resolve + download the transitive RPM set into a
    // scratch cache directory. Mirrors old-twoliter's `build.Dockerfile`
    // `repobuild` stage:
    //
    //     dnf -y --disablerepo '*' --repofrompath repo,./rpmbuild/RPMS
    //         --enablerepo 'repo' … --nogpgcheck --forcearch "${ARCH}"
    //         --setopt=cachedir=/local/rpms
    //         install --downloadonly
    //         $(printf "bottlerocket-%s\n" metadata ...)
    //
    // Crucially, `--installroot` is **not** used here: dnf5
    // pre-creates `<installroot>/usr/lib/sysimage/rpm/` (rpmdb) before
    // the transaction runs, and if that installroot is the real
    // rootfs the `bottlerocket-filesystem` RPM's CPIO payload later
    // fails to unpack `/usr` (`cpio: File from package already exists
    // as a directory`). Old-twoliter avoids this by never pointing
    // dnf at the target rootfs at all — the actual install into the
    // rootfs happens via a separate `rpm -iv --root` step
    // (rpm2img L211), which is what [`install_rpms`] does below.
    //
    // Downloaded RPMs land in `<cachedir>/<repo-id-hash>/packages/`
    // (dnf5 layout). We don't flatten — [`install_rpms`] walks the
    // tree recursively via `list_files_recursive`.
    let download_dir = vfs.create_dir("dnf-download").await?;
    let download_dir_path = download_dir.path().to_path_buf();

    let download_dir_q = shell_quote_path(&download_dir_path);
    let repofrompath = format!("--repofrompath=local,{package_dir_q}");
    let cachedir = format!("--setopt=cachedir={download_dir_q}");
    let forcearch = format!("--forcearch={arch}");

    let mut argv: Vec<String> = vec![
        "--assumeyes".into(),
        "--disablerepo=*".into(),
        repofrompath,
        "--enablerepo=local".into(),
        "--nogpgcheck".into(),
        forcearch,
        cachedir,
        "install".into(),
        "--downloadonly".into(),
    ];
    for name in package_names {
        argv.push(crate::image::vfs_ext::shell_quote(name));
    }

    vfs.command("dnf_resolve_rpms", "dnf", &argv).await?;

    // Step 3: force-install the resolved set into the empty rootfs.
    install_rpms(vfs, install_root, &download_dir_path).await?;
    Ok(())
}

/// Query installed RPMs. Returns raw stdout from
/// `rpm -qa --root <root> --queryformat <fmt>` for the caller to parse.
/// Ports the `rpm -qa` call inside `generate_application_inventory`.
///
/// The `_dbpath` override matches [`install_rpms`]: the DB lives at
/// `<root>/var/lib/rpm`, not the SDK-default
/// `<root>/usr/lib/sysimage/rpm`. Without this override `rpm -qa`
/// would look at an empty (or non-existent) DB path and return
/// nothing.
pub async fn queryformat(vfs: &Vfs, install_root: &Path, queryformat: &str) -> Result<Vec<u8>> {
    let install_root_q = shell_quote_path(install_root);
    let queryformat_q = crate::image::vfs_ext::shell_quote(queryformat);
    let out = vfs
        .output(
            "rpm_queryformat",
            "rpm",
            &[
                "-qa",
                "--define",
                "'_dbpath /var/lib/rpm'",
                "--root",
                install_root_q.as_str(),
                "--queryformat",
                queryformat_q.as_str(),
            ],
        )
        .await?;
    Ok(out)
}
