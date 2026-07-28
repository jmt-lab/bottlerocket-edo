//! Boot-config snippet parsing and assembly.
//!
//! The build pipeline concatenates every file under
//! `${ROOT_MOUNT}/boot/boot-config.d/` into a single
//! [`crate::partition::Part::Private`]-embedded `bootconfig.data`. Snippets
//! must be single-line `key = value` — no braces, multi-line arrays,
//! `:=`, or `+=`. See `rpm2img` L598-615 for the shell equivalent.

use std::path::Path;

use edo::environment::Vfs;
use regex::Regex;

use crate::error::{Error, Result};
use crate::vfs_ext::list_files_follow;

/// The default bootconfig payload used when there are no snippets to
/// assemble. Matches the shell:
///
/// ```text
/// kernel {}
/// init {}
/// ```
pub const NOOP_BOOTCONFIG: &str = "kernel {}\ninit {}\n";

/// Assemble a bootconfig by concatenating and sorting every regular
/// file directly under `snippet_dir` inside the environment (no
/// recursion). Returns [`NOOP_BOOTCONFIG`] if the directory does not
/// exist or contains no snippets.
///
/// Every snippet is validated with [`validate_snippet`]; the first
/// invalid line aborts with [`Error::BootconfigInvalid`].
pub async fn assemble_bootconfig(vfs: &Vfs, snippet_dir: &Path) -> Result<String> {
    if !vfs.try_exists(snippet_dir).await? {
        return Ok(NOOP_BOOTCONFIG.to_string());
    }
    // Follow symlinks so an RPM shipping a bootconfig snippet as a
    // symlink (e.g. pointing at a shared file under /usr/share) is
    // still picked up as a regular file. The shell version relied on
    // the fact that `xargs cat` follows symlinks and its `find -type f`
    // was combined with `-L`; the port matches that with
    // `list_files_follow`.
    let entries = list_files_follow(vfs, snippet_dir, "*").await?;
    if entries.is_empty() {
        return Ok(NOOP_BOOTCONFIG.to_string());
    }

    // Read each file, validate line-by-line, then concatenate the sorted
    // union of lines. The shell uses `xargs cat | sort`, which sorts
    // lines across all files rather than by filename; match that.
    let mut all_lines: Vec<String> = Vec::new();
    for p in &entries {
        let bytes = vfs.read(p).await?;
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            validate_snippet_line(p, line)?;
            all_lines.push(line.to_string());
        }
    }
    all_lines.sort();
    let mut out = String::new();
    for l in all_lines {
        out.push_str(&l);
        out.push('\n');
    }
    Ok(out)
}

/// Validate a single snippet line. Reject the same shapes the shell
/// rejects: braces, multi-line arrays (`,` at end of line), `:=`, `+=`.
/// Comments (`#`) and blank lines are always accepted.
pub fn validate_snippet_line(source_path: &Path, line: &str) -> Result<()> {
    let stripped = line.trim_start();
    if stripped.is_empty() || stripped.starts_with('#') {
        return Ok(());
    }
    let bad = bad_line_regex();
    if bad.is_match(line) {
        return Err(Error::BootconfigInvalid {
            path: source_path.to_path_buf(),
            reason: format!(
                "line uses an unsupported bootconfig construct: {line:?}. \
                 snippets must be single-line 'key = value' — no braces, \
                 multi-line arrays, `:=`, or `+=`."
            ),
        });
    }
    Ok(())
}

/// Validate a full snippet body (multi-line text). Convenience for
/// tests.
pub fn validate_snippet(source_path: &Path, text: &str) -> Result<()> {
    for line in text.lines() {
        validate_snippet_line(source_path, line)?;
    }
    Ok(())
}

fn bad_line_regex() -> Regex {
    // Matches the shell:
    //   '^\s*[^#].*([:+]=|[{]|[,]\s*$)'
    Regex::new(r"^\s*[^#].*([:+]=|\{|,\s*$)").expect("bad_line_regex is a valid regex")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(name);
        p
    }

    #[test]
    fn accepts_ordinary_kv() {
        let p = tmp("bc-ok");
        validate_snippet(&p, "kernel.name = foo").unwrap();
        validate_snippet(&p, "  key = value").unwrap();
    }

    #[test]
    fn accepts_comment_and_blank() {
        let p = tmp("bc-comment");
        validate_snippet(&p, "# a comment").unwrap();
        validate_snippet(&p, "").unwrap();
        validate_snippet(&p, "  # indented comment").unwrap();
    }

    #[test]
    fn rejects_brace() {
        let p = tmp("bc-brace");
        let err = validate_snippet(&p, "kernel {").unwrap_err();
        assert!(matches!(err, Error::BootconfigInvalid { .. }));
    }

    #[test]
    fn rejects_plus_equal() {
        let p = tmp("bc-plus");
        let err = validate_snippet(&p, "key += value").unwrap_err();
        assert!(matches!(err, Error::BootconfigInvalid { .. }));
    }

    #[test]
    fn rejects_colon_equal() {
        let p = tmp("bc-colon");
        let err = validate_snippet(&p, "key := value").unwrap_err();
        assert!(matches!(err, Error::BootconfigInvalid { .. }));
    }

    #[test]
    fn rejects_multiline_array_marker() {
        let p = tmp("bc-array");
        let err = validate_snippet(&p, "key = a,").unwrap_err();
        assert!(matches!(err, Error::BootconfigInvalid { .. }));
    }
}
