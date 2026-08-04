//! SBOM helper: generate an SPDX SBOM over a directory of RPMs and
//! resolve it to a single output path.
//!
//! Wraps `crate::image::sbom::generate_sbom` with the "pick the produced
//! `.spdx.json` file out of the output directory" fallback that
//! [`VariantRootfsTransform`](super::rootfs::VariantRootfsTransform)
//! uses. The wrapper is factored out so the rootfs transform stays
//! under the 500-line gate and so the same fallback is available to a
//! future migration transform that needs its own SBOM.

use edo::environment::Vfs;
use snafu::ResultExt;
use std::path::{Path, PathBuf};

use super::error;

/// Run `sbomtool` over `rpm_dir` and produce a single `sbom.spdx.json`
/// at `out_path`.
///
/// sbomtool writes `<name>-spdx.json` (dash-separated, matching
/// old-twoliter's `rpm2img` line 317 `variant-rpms-${format}.json`
/// convention), not `<name>.spdx.json`. We prefer the exact
/// `variant-rpms-spdx.json` name we asked sbomtool to produce and fall
/// back to the first `*-spdx.json` / `*.spdx.json` under the work dir
/// so this keeps working if a future sbomtool version changes the
/// naming again.
pub async fn generate_sbom_spdx(
    vfs: &Vfs,
    rpm_dir: &Path,
    sbom_work_dir: &Path,
    out_path: &Path,
    step_label: &str,
) -> Result<PathBuf, error::Error> {
    crate::image::sbom::generate_sbom(vfs, rpm_dir, sbom_work_dir)
        .await
        .context(error::BrImageSnafu)?;

    // Preferred: exact filename sbomtool produces today.
    let preferred = sbom_work_dir.join("variant-rpms-spdx.json");
    if vfs
        .try_exists(&preferred)
        .await
        .map_err(crate::image::Error::from)
        .context(error::BrImageSnafu)?
    {
        vfs.copy(&preferred, out_path)
            .await
            .map_err(crate::image::Error::from)
            .context(error::BrImageSnafu)?;
        return Ok(out_path.to_path_buf());
    }

    // Fallback: any `*-spdx.json` or `*.spdx.json` sbomtool may have
    // written (older/newer SDKs differ in the separator character).
    for glob in ["*-spdx.json", "*.spdx.json"] {
        let candidates = crate::image::vfs_ext::list_files(vfs, sbom_work_dir, glob)
            .await
            .context(error::BrImageSnafu)?;
        if let Some(first) = candidates.first() {
            vfs.copy(first, out_path)
                .await
                .map_err(crate::image::Error::from)
                .context(error::BrImageSnafu)?;
            return Ok(out_path.to_path_buf());
        }
    }

    Err(error::Error::BuildStep {
        step: step_label.to_string(),
        reason: format!(
            "no *-spdx.json / *.spdx.json produced by sbomtool under {}",
            sbom_work_dir.display()
        ),
    })
}
