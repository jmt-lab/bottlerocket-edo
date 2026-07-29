//! SELinux file-context labeling helper. Ports the `setfiles -n -d -F -m -r`
//! + awk pipeline in `rpm2img` L515-517 and L620-622.
//!
//! `setfiles` prints one line per file with its computed context. The
//! caller pipes that into `debugfs -w -f - <image>` to persist the
//! labels. `br-image` provides the awk-replacement as a pure Rust
//! transformation.

use std::path::Path;

use edo::environment::Vfs;

use crate::image::error::{Error, Result};
use crate::image::vfs_ext::shell_quote_path;

/// Run `setfiles -n -d -F -m -r <mount> <contexts> <mount>` inside the
/// environment and return the raw stdout for [`build_ea_set_script`] to
/// consume.
///
/// Skeleton: requires a real `setfiles` binary from the SELinux userland.
pub async fn compute_labels(vfs: &Vfs, mount: &Path, contexts: &Path) -> Result<Vec<u8>> {
    let mount_q = shell_quote_path(mount);
    let contexts_q = shell_quote_path(contexts);
    let out = vfs
        .output(
            "selinux_setfiles",
            "setfiles",
            &[
                "-n",
                "-d",
                "-F",
                "-m",
                "-r",
                mount_q.as_str(),
                contexts_q.as_str(),
                mount_q.as_str(),
            ],
        )
        .await?;
    Ok(out)
}

/// Translate the output of `setfiles` into a debugfs script that sets
/// the `security.selinux` xattr for each path.
///
/// Ports the awk pipeline in `rpm2img`:
/// ```text
/// awk -v root="${ROOT_MOUNT}" \
///     '{gsub(root"/","/"); gsub(root,"/"); print "ea_set", $1, "security.selinux", $4}'
/// ```
///
/// Returns [`Error::ParseOutput`] when `setfiles_output` is not valid
/// UTF-8. `setfiles` emits POSIX paths and SELinux contexts, both of
/// which are UTF-8 by contract; invalid bytes here indicate a corrupt
/// run and should not be silently discarded.
pub fn build_ea_set_script(setfiles_output: &[u8], root_mount: &Path) -> Result<String> {
    let text = std::str::from_utf8(setfiles_output).map_err(|e| Error::ParseOutput {
        tool: "setfiles".into(),
        reason: format!("non-UTF-8 output: {e}"),
    })?;
    let root_str = root_mount.to_string_lossy();
    let root_slash = format!("{root_str}/");
    let mut out = String::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let path_raw = match fields.next() {
            Some(p) => p,
            None => continue,
        };
        // Skip fields 2 and 3 (user/role/type index columns).
        let _ = fields.next();
        let _ = fields.next();
        let context = match fields.next() {
            Some(c) => c,
            None => continue,
        };
        // Normalize path: replace root_mount prefix with root.
        let path_norm = if let Some(rest) = path_raw.strip_prefix(&*root_slash) {
            format!("/{rest}")
        } else if path_raw == root_str.as_ref() {
            "/".to_string()
        } else {
            path_raw.to_string()
        };
        out.push_str(&format!("ea_set {path_norm} security.selinux {context}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ea_set_script_strips_root_mount() {
        let input =
            b"/mnt/root/usr/bin/foo user_u:role_r:file_t:s0 idx1 system_u:object_r:bin_t:s0\n\
                      /mnt/root user_u:role_r:file_t:s0 idx1 system_u:object_r:root_t:s0\n";
        let script = build_ea_set_script(input, Path::new("/mnt/root")).unwrap();
        assert!(script.contains("ea_set /usr/bin/foo security.selinux system_u:object_r:bin_t:s0"));
        assert!(script.contains("ea_set / security.selinux system_u:object_r:root_t:s0"));
    }

    #[test]
    fn build_ea_set_script_ignores_short_lines() {
        let input = b"only-one-field\n";
        let script = build_ea_set_script(input, Path::new("/mnt/root")).unwrap();
        assert!(script.is_empty());
    }

    #[test]
    fn build_ea_set_script_errors_on_invalid_utf8() {
        // A stray 0xFF is not a valid UTF-8 continuation byte and must
        // surface as a typed error rather than being silently replaced
        // with an empty string.
        let input = b"\xff\xfe not utf-8";
        let err = build_ea_set_script(input, Path::new("/mnt/root")).unwrap_err();
        assert!(err.to_string().contains("non-UTF-8"), "err: {err}");
    }
}
