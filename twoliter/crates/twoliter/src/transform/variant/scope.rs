//! Source-scope helpers shared by every `variant-*` transform.
//!
//! Each transform reads its `element.source` as a keyed
//! [`SourceMap`](edo::context::element::SourceMap) with named scopes
//! (`ca-bundle`, `tuf-root`, `sbkeys`, `ovf-template`, …). This module
//! centralises the two operations every transform needs:
//! - [`resolve_optional`]: parse a single-address optional scope,
//!   returning `None` when absent, a resolved [`Source`] when
//!   declared once, and a typed error when declared multiple times.
//! - [`pick_first_in`]: pick a single file out of a `deps/<scope>/`
//!   directory that a source unpacked into. Optional-source scopes
//!   may name their file anything, so we don't assume a fixed leaf.
//!
//! Also lives here:
//! - [`try_exists`]: `Vfs::try_exists` wrapped to return the crate's
//!   error type.
//! - [`resolve_file_contexts`]: post-`stage_root` discovery of the
//!   SELinux `file_contexts` path (needed by both the rootfs stage
//!   for labeling and the disk stage for `mkfs.erofs`).
//! - [`copy_partition`]: thin wrapper around
//!   [`crate::image::image::dd_partition`] returning our error type.

use edo::{
    context::{Context, Element},
    environment::Vfs,
    source::Source,
};
use snafu::ResultExt;
use std::path::Path;

use super::error;

/// Optional keyed scopes. Each accepts at most one source addr;
/// multiple entries are a config error surfacing as
/// [`error::Error::TooManyScopeSources`].
pub async fn resolve_optional(
    scope_table: &std::collections::BTreeMap<String, Vec<Element>>,
    scope: &str,
    ctx: &Context,
) -> Result<Option<Source>, error::Error> {
    let entries = match scope_table.get(scope) {
        Some(v) => v,
        None => return Ok(None),
    };
    match entries.as_slice() {
        [single] => Ok(Some(ctx.add_source(single).await?)),
        [] => Ok(None),
        _ => Err(error::Error::TooManyScopeSources {
            scope: scope.to_string(),
            count: entries.len(),
        }),
    }
}

/// Return the first regular file directly under `dir` inside `vfs`,
/// or `None` when the directory is absent or empty.
///
/// Used to pick the single file each optional single-source scope
/// (`ca-bundle`, `tuf-root`, `ovf-template`) stages into its
/// scope-specific `deps/<scope>/` directory. Sources may name their
/// file anything (`ca-bundle.crt`, `bundle.pem`, `certs.crt`, …) so
/// we don't assume a fixed leaf name.
pub async fn pick_first_in(
    vfs: &Vfs,
    dir: &str,
) -> Result<Option<std::path::PathBuf>, error::Error> {
    let dir_path = Path::new(dir);
    if !try_exists(vfs, dir_path).await? {
        return Ok(None);
    }
    let files = crate::image::vfs_ext::list_files(vfs, dir_path, "*")
        .await
        .context(error::BrImageSnafu)?;
    Ok(files.into_iter().next())
}

/// Wrap `Vfs::try_exists` in the transform's `error::Error` type. The
/// raw `EnvironmentError` doesn't have a direct `From` into
/// `error::Error`, so route through `crate::image::Error` (whose `Env`
/// variant is `#[snafu(transparent)]` and provides the conversion).
pub async fn try_exists(vfs: &Vfs, path: &Path) -> Result<bool, error::Error> {
    vfs.try_exists(path)
        .await
        .map_err(crate::image::Error::from)
        .context(error::BrImageSnafu)
}

/// Locate the SELinux `file_contexts` path inside an already-installed
/// rootfs. First tries the canonical `targeted` policy path; falls
/// back to a glob across every policy directory. Returns `None` when
/// no policy is present (the variant did not install an SELinux
/// policy RPM), which is a valid — if unusual — configuration.
pub async fn resolve_file_contexts(
    vfs: &Vfs,
    root_mount: &Path,
) -> Result<Option<std::path::PathBuf>, error::Error> {
    let targeted = root_mount.join("usr/lib/selinux/targeted/contexts/files/file_contexts");
    if try_exists(vfs, &targeted).await? {
        return Ok(Some(targeted));
    }
    // Fall back: walk `usr/lib/selinux/*/contexts/files/file_contexts`.
    // `list_files` is non-recursive, so we enumerate policy dirs
    // ourselves.
    let selinux_root = root_mount.join("usr/lib/selinux");
    if !try_exists(vfs, &selinux_root).await? {
        return Ok(None);
    }
    let policies = crate::image::vfs_ext::list_dir_all(vfs, &selinux_root)
        .await
        .context(error::BrImageSnafu)?;
    for policy in policies {
        let candidate = policy.join("contexts/files/file_contexts");
        if try_exists(vfs, &candidate).await? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// `dd` a scratch image into `disk` at the given MiB offset, going
/// through [`crate::image::image::dd_partition`] so the "one subprocess
/// per step" rule is preserved.
pub async fn copy_partition(
    vfs: &Vfs,
    src: &Path,
    disk: &Path,
    offset_mib: u32,
) -> Result<(), error::Error> {
    crate::image::image::dd_partition(vfs, src, disk, offset_mib)
        .await
        .context(error::BrImageSnafu)
}
