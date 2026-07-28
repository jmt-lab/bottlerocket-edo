//! Port of `guest-images-helper::copy_guest_image_artifacts`.
//!
//! Walks a directory non-recursively inside the environment, picks up
//! every regular file (and symlink) whose filename ends in one of the
//! [`ArtifactSuffix`] extensions, and copies it (or the symlink target)
//! into a destination directory. Returns [`Error::NoGuestArtifacts`]
//! if no files matched.

use std::path::Path;

use edo::environment::Vfs;

use crate::compress::ArtifactSuffix;
use crate::error::{Error, Result};
use crate::vfs_ext::{list_dir_all, shell_quote_path};

/// Copy every image-artifact file (and symlink) from `src` into `dst`
/// inside the environment. Preserves symlinks by re-emitting `ln -s`
/// against the resolved target so stable-name links continue to
/// resolve.
pub async fn copy_guest_artifacts(vfs: &Vfs, src: &Path, dst: &Path) -> Result<usize> {
    if !vfs.try_exists(src).await? {
        return Err(Error::NoGuestArtifacts {
            src: src.to_path_buf(),
        });
    }
    vfs.create_dir(dst).await?;

    let entries = list_dir_all(vfs, src).await?;
    let mut copied = 0usize;
    for path in entries {
        let name_os = match path.file_name() {
            Some(n) => n,
            None => continue,
        };
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        if !name_matches_any(name) {
            continue;
        }
        let dst_path = dst.join(name);
        // Vfs has no symlink primitive; `cp -a` preserves the link if
        // the source is a symlink, otherwise performs a regular copy.
        // We use `cp -a` uniformly so both cases go through the same
        // command and the environment's log records a single op per
        // artifact.
        let src_q = shell_quote_path(&path);
        let dst_q = shell_quote_path(&dst_path);
        vfs.command(
            "copy_guest_artifact",
            "cp",
            &["-a", src_q.as_str(), dst_q.as_str()],
        )
        .await?;
        copied += 1;
    }

    if copied == 0 {
        return Err(Error::NoGuestArtifacts {
            src: src.to_path_buf(),
        });
    }
    Ok(copied)
}

fn name_matches_any(name: &str) -> bool {
    ArtifactSuffix::ALL.iter().any(|s| s.matches_filename(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_any_covers_every_suffix() {
        assert!(name_matches_any("bottlerocket.img.lz4"));
        assert!(name_matches_any("bottlerocket.qcow2"));
        assert!(name_matches_any("bottlerocket.vmdk"));
        assert!(name_matches_any("bottlerocket.ova"));
        assert!(name_matches_any("boot.ext4.lz4"));
        assert!(name_matches_any("root.verity.lz4"));
    }

    #[test]
    fn name_matches_any_rejects_unrelated() {
        assert!(!name_matches_any("artifact-metadata.json"));
        assert!(!name_matches_any("README.md"));
        assert!(!name_matches_any("bottlerocket-img.lz4")); // missing '.'
    }
}
