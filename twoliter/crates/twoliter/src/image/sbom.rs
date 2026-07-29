//! SBOM assembly via `syft`. Ports the SBOM staging + merge logic in
//! `rpm2img` L306-351.
//!
//! Skeleton: the syft invocation and the multi-source merge are wired
//! through `Vfs` but not exercised in-crate.

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::image::error::Result;
use crate::image::vfs_ext::shell_quote_path;

/// SBOM output format. Bottlerocket emits both.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SbomFormat {
    Spdx,
    Cyclonedx,
}

impl SbomFormat {
    pub fn ext(&self) -> &'static str {
        match self {
            SbomFormat::Spdx => "spdx",
            SbomFormat::Cyclonedx => "cyclonedx",
        }
    }
}

/// Generate SBOMs for the RPMs under `package_dir` using `sbomtool`,
/// writing them to `output_dir` inside the environment.
///
/// Ports `sbomtool generate --name variant-rpms --build-dir …
/// --spdx --cyclonedx`.
pub async fn generate_sbom(vfs: &Vfs, package_dir: &Path, output_dir: &Path) -> Result<PathBuf> {
    vfs.create_dir(output_dir).await?;

    let package_dir_q = shell_quote_path(package_dir);
    let output_dir_q = shell_quote_path(output_dir);
    vfs.command(
        "sbom_generate",
        "sbomtool",
        &[
            "generate",
            "--name",
            "variant-rpms",
            "--build-dir",
            package_dir_q.as_str(),
            "--out-dir",
            output_dir_q.as_str(),
            "--spdx",
            "--cyclonedx",
        ],
    )
    .await?;
    Ok(output_dir.to_path_buf())
}

/// Merge multiple SBOMs into `output` via `sbomtool merge`. Skeleton.
pub async fn merge_sboms(vfs: &Vfs, output: &Path, inputs: &[PathBuf]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let output_q = shell_quote_path(output);
    let input_qs: Vec<String> = inputs.iter().map(|p| shell_quote_path(p)).collect();
    let mut argsv: Vec<&str> = vec!["merge", "--output", output_q.as_str()];
    for p in &input_qs {
        argsv.push(p.as_str());
    }
    vfs.command("sbom_merge", "sbomtool", &argsv).await?;
    Ok(())
}
