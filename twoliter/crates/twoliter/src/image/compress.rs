//! Image compression / decompression, keyed off the canonical set of
//! image-artifact suffixes.
//!
//! Ports `IMAGE_ARTIFACT_SUFFIXES`, `compress_image`, `decompress_image`,
//! and `symlink_image` from
//! `old-twoliter/twoliter/embedded/imghelper`. The pure part —
//! [`ArtifactSuffix`] parsing and rendering — is exercised by unit tests
//! here; the shell-out cases ([`compress_image`], [`decompress_image`],
//! [`create_latest_symlink`]) run inside the transform's `Vfs` and are
//! covered by integration tests in the transform that drives them.

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::image::error::{Error, Result};
use crate::image::vfs_ext::{remove_if_present, shell_quote, shell_quote_path};

/// Every image artifact suffix the build pipeline produces. This is the
/// single source of truth: `compress_image` refuses any extension not on
/// this list, and `guest::copy_guest_artifacts` uses it to pick which
/// files to embed into a host rootfs.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ArtifactSuffix {
    /// `.img.lz4` — raw disk image, lz4-compressed.
    ImgLz4,
    /// `.qcow2` — QCOW2 disk image.
    Qcow2,
    /// `.vmdk` — VMDK disk image.
    Vmdk,
    /// `.ova` — Open Virtualization Appliance.
    Ova,
    /// `.ext4.lz4` — ext4 filesystem image, lz4-compressed.
    Ext4Lz4,
    /// `.verity.lz4` — dm-verity hash image, lz4-compressed.
    VerityLz4,
}

impl ArtifactSuffix {
    /// The trailing extension (without leading `.`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            ArtifactSuffix::ImgLz4 => "img.lz4",
            ArtifactSuffix::Qcow2 => "qcow2",
            ArtifactSuffix::Vmdk => "vmdk",
            ArtifactSuffix::Ova => "ova",
            ArtifactSuffix::Ext4Lz4 => "ext4.lz4",
            ArtifactSuffix::VerityLz4 => "verity.lz4",
        }
    }

    /// Every suffix, in the same order as the shell's
    /// `IMAGE_ARTIFACT_SUFFIXES` array.
    pub const ALL: &'static [ArtifactSuffix] = &[
        ArtifactSuffix::ImgLz4,
        ArtifactSuffix::Qcow2,
        ArtifactSuffix::Vmdk,
        ArtifactSuffix::Ova,
        ArtifactSuffix::Ext4Lz4,
        ArtifactSuffix::VerityLz4,
    ];

    /// Parse an extension string (without leading `.`) as an
    /// [`ArtifactSuffix`]. Returns [`Error::InvalidArtifact`] on miss.
    pub fn try_from_extension(ext: &str, path: &Path) -> Result<Self> {
        for s in Self::ALL {
            if s.as_str() == ext {
                return Ok(*s);
            }
        }
        Err(Error::InvalidArtifact {
            path: path.to_path_buf(),
            expected_suffix: format!(
                "one of: {}",
                Self::ALL
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
    }

    /// Does the filename (without directory) end with this suffix? Uses
    /// a literal `.<ext>` match, matching the shell's `*.${suffix}`.
    pub fn matches_filename(&self, name: &str) -> bool {
        let want = format!(".{}", self.as_str());
        name.ends_with(&want)
    }
}

/// Compress `input` to `output` inside `vfs`, using the compressor
/// implied by `suffix`.
///
/// Matches the semantics of `imghelper::compress_image`: `img.lz4` /
/// `ext4.lz4` / `verity.lz4` invoke `lz4 -9v`; `qcow2` and `vmdk` invoke
/// `qemu-img convert` (with `subformat=streamOptimized` for vmdk to
/// support OVA embedding); `ova` has no compressor and is rejected here
/// (OVAs are assembled by [`crate::image::ovf::assemble_ova`], not this
/// function).
pub async fn compress_image(
    vfs: &Vfs,
    input: &Path,
    output: &Path,
    suffix: ArtifactSuffix,
) -> Result<()> {
    let input_q = shell_quote_path(input);
    let output_q = shell_quote_path(output);
    match suffix {
        ArtifactSuffix::ImgLz4 | ArtifactSuffix::Ext4Lz4 | ArtifactSuffix::VerityLz4 => {
            vfs.command(
                "compress_image_lz4",
                "lz4",
                &["-9v", "-f", input_q.as_str(), output_q.as_str()],
            )
            .await?;
        }
        ArtifactSuffix::Qcow2 => {
            vfs.command(
                "compress_image_qcow2",
                "qemu-img",
                &[
                    "convert",
                    "-f",
                    "raw",
                    "-O",
                    "qcow2",
                    input_q.as_str(),
                    output_q.as_str(),
                ],
            )
            .await?;
        }
        ArtifactSuffix::Vmdk => {
            vfs.command(
                "compress_image_vmdk",
                "qemu-img",
                &[
                    "convert",
                    "-f",
                    "raw",
                    "-O",
                    "vmdk",
                    "-o",
                    "subformat=streamOptimized",
                    input_q.as_str(),
                    output_q.as_str(),
                ],
            )
            .await?;
        }
        ArtifactSuffix::Ova => {
            return Err(Error::InvalidArtifact {
                path: output.to_path_buf(),
                expected_suffix: "ova is assembled by assemble_ova, not compress_image".into(),
            });
        }
    }
    Ok(())
}

/// Decompress `input` (whose extension implies the source format) into
/// the raw disk image at `output`. Mirrors
/// `imghelper::decompress_image`.
pub async fn decompress_image(
    vfs: &Vfs,
    input: &Path,
    output: &Path,
    suffix: ArtifactSuffix,
) -> Result<()> {
    let input_q = shell_quote_path(input);
    let output_q = shell_quote_path(output);
    match suffix {
        ArtifactSuffix::ImgLz4 | ArtifactSuffix::Ext4Lz4 | ArtifactSuffix::VerityLz4 => {
            vfs.command(
                "decompress_image_unlz4",
                "unlz4",
                &["-f", input_q.as_str(), output_q.as_str()],
            )
            .await?;
        }
        ArtifactSuffix::Qcow2 => {
            vfs.command(
                "decompress_image_qcow2",
                "qemu-img",
                &[
                    "convert",
                    "-f",
                    "qcow2",
                    "-O",
                    "raw",
                    input_q.as_str(),
                    output_q.as_str(),
                ],
            )
            .await?;
        }
        ArtifactSuffix::Vmdk => {
            vfs.command(
                "decompress_image_vmdk",
                "qemu-img",
                &[
                    "convert",
                    "-f",
                    "vmdk",
                    "-O",
                    "raw",
                    input_q.as_str(),
                    output_q.as_str(),
                ],
            )
            .await?;
        }
        ArtifactSuffix::Ova => {
            return Err(Error::InvalidArtifact {
                path: input.to_path_buf(),
                expected_suffix: "ova must be extracted with tar, not decompress_image".into(),
            });
        }
    }
    Ok(())
}

/// Create a stable-name symlink in `dir` pointing at `target`.
///
/// Ports the `ln -sf` from `imghelper::symlink_image`. Removes any
/// existing symlink at the target path first. The `target` argument is
/// the *link target string* stored in the symlink, typically the
/// filename of the versioned artifact.
pub async fn create_latest_symlink(
    vfs: &Vfs,
    dir: &Path,
    link_name: &str,
    target: &str,
) -> Result<PathBuf> {
    let link_path = dir.join(link_name);
    // Remove any existing entry so `ln -sf` starts from a clean state.
    remove_if_present(vfs, &link_path).await?;
    let link_path_q = shell_quote_path(&link_path);
    let target_q = shell_quote(target);
    vfs.command(
        "create_symlink",
        "ln",
        &["-sf", target_q.as_str(), link_path_q.as_str()],
    )
    .await?;
    Ok(link_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn artifact_suffix_all_covers_shell_list() {
        // These must match `IMAGE_ARTIFACT_SUFFIXES` in imghelper, in
        // that order, or `guest::copy_guest_artifacts` will drift.
        let expected: &[&str] = &["img.lz4", "qcow2", "vmdk", "ova", "ext4.lz4", "verity.lz4"];
        let actual: Vec<&str> = ArtifactSuffix::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn artifact_suffix_try_from_accepts_every_variant() {
        let p = Path::new("/tmp/x");
        for s in ArtifactSuffix::ALL {
            let parsed = ArtifactSuffix::try_from_extension(s.as_str(), p).unwrap();
            assert_eq!(parsed, *s);
        }
    }

    #[test]
    fn artifact_suffix_rejects_unknown_extension() {
        let p = Path::new("/tmp/x.tar.gz");
        let err = ArtifactSuffix::try_from_extension("tar.gz", p).unwrap_err();
        assert!(matches!(err, Error::InvalidArtifact { .. }));
    }

    #[test]
    fn artifact_suffix_rejects_partial_match() {
        // `lz4` alone is not an accepted suffix — only `img.lz4`,
        // `ext4.lz4`, `verity.lz4` are.
        let p = Path::new("/tmp/x.lz4");
        let err = ArtifactSuffix::try_from_extension("lz4", p).unwrap_err();
        assert!(matches!(err, Error::InvalidArtifact { .. }));
    }

    #[test]
    fn matches_filename_covers_every_suffix() {
        assert!(ArtifactSuffix::ImgLz4.matches_filename("bottlerocket.img.lz4"));
        assert!(ArtifactSuffix::Ext4Lz4.matches_filename("bottlerocket-boot.ext4.lz4"));
        assert!(ArtifactSuffix::VerityLz4.matches_filename("bottlerocket-root.verity.lz4"));
        assert!(ArtifactSuffix::Qcow2.matches_filename("bottlerocket.qcow2"));
        assert!(ArtifactSuffix::Vmdk.matches_filename("bottlerocket.vmdk"));
        assert!(ArtifactSuffix::Ova.matches_filename("bottlerocket.ova"));
    }

    #[test]
    fn matches_filename_requires_dot_prefix() {
        // `foo-lz4` shouldn't match `img.lz4`.
        assert!(!ArtifactSuffix::ImgLz4.matches_filename("bottlerocket-img.lz4"));
        assert!(!ArtifactSuffix::Ova.matches_filename("bottlerocket.ova.bak"));
    }
}
