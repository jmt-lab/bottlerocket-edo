//! Small helpers layered over [`edo::environment::Vfs`].
//!
//! `Vfs` deliberately exposes a narrow set of ops (mkdir / read / write /
//! copy / rename / remove / command / output). A handful of br-image
//! call sites need composed operations that appear repeatedly: a
//! best-effort remove that succeeds when the target is absent; a size
//! lookup via `stat -c %s`; a `find`-based directory listing since
//! `Vfs` has no `read_dir` primitive. Rather than open-code these at
//! every call site, they live here.
//!
//! ### Shell-quoting invariant
//!
//! Both [`Vfs::command`](edo::environment::Vfs::command) and
//! [`Vfs::output`](edo::environment::Vfs::output) build their command
//! string by joining the program and args with single spaces and
//! dispatching the result through `sh -c` inside the environment.
//! Every non-fixed value br-image hands to those APIs — paths, globs,
//! caller-supplied strings — must therefore be shell-quoted first, or
//! the enclosing shell will glob-expand `*` / `?` / brace-expand `{}`
//! before the child process sees the argument. Use [`shell_quote`] for
//! every such value; fixed tokens like `-name`, `-type`, `1`, or `f`
//! can be passed verbatim.

use std::path::{Path, PathBuf};

use edo::environment::Vfs;

use crate::error::Result;

/// Wrap `s` in POSIX single quotes, escaping any embedded single quote
/// as `'\''`. Safe against every shell metacharacter — the shell does
/// no expansion inside single quotes except for `'` itself.
///
/// The returned string is suitable to concatenate directly into a
/// command line that will be handed to `sh -c`. Callers should apply
/// this to every path, glob, and caller-supplied argument before
/// handing it to [`edo::environment::Vfs::command`] or
/// [`edo::environment::Vfs::output`].
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close the current single-quoted run, insert an escaped
            // single quote, then reopen the single-quoted run.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Convenience: shell-quote a filesystem path via its lossy string
/// representation. `Vfs` itself operates on `to_string_lossy` when
/// composing the shell command, so no additional fidelity is available.
pub fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

/// Remove `path` if it exists; do nothing (successfully) if it does
/// not. Analogous to `rm -f`.
///
/// The current [`Vfs::remove_file`](edo::environment::Vfs::remove_file)
/// treats any non-zero exit as an error, so a "remove if present"
/// pattern requires a `try_exists` guard first. This mirrors the
/// `Err(NotFound) => Ok(())` idiom that the pre-Vfs `br-image` code
/// used with `tokio::fs::remove_file`.
pub async fn remove_if_present(vfs: &Vfs, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if vfs.try_exists(path).await? {
        vfs.remove_file(path).await?;
    }
    Ok(())
}

/// Return the size in bytes of `path` inside `vfs`. Implemented via
/// `stat -c %s` since `Vfs` exposes no `metadata` op.
///
/// Fails when `stat` reports non-zero (e.g. the file does not exist)
/// or when its stdout is not a decimal `u64`.
pub async fn size_bytes(vfs: &Vfs, path: impl AsRef<Path>) -> Result<u64> {
    let quoted = shell_quote_path(path.as_ref());
    let out = vfs
        .output("stat_size", "stat", &["-c", "%s", quoted.as_str()])
        .await?;
    let text = String::from_utf8_lossy(&out);
    let trimmed = text.trim();
    trimmed
        .parse::<u64>()
        .map_err(|e| crate::error::Error::ParseOutput {
            tool: "stat".into(),
            reason: format!("expected decimal size, got {trimmed:?}: {e}"),
        })
}

/// List every regular file directly under `dir` whose filename
/// satisfies `name_glob` (a `find -name` pattern). Returns paths sorted
/// by their string representation. Set `name_glob` to `"*"` to list
/// everything. Symlinks are *not* followed; use [`list_files_follow`]
/// if snippet-style symlinks-to-files should be included.
///
/// Runs `find <dir> -mindepth 1 -maxdepth 1 -type f -name <glob>`. The
/// directory path and the glob are both shell-quoted so `find` sees
/// them verbatim even when the shell would otherwise brace- or
/// glob-expand them.
pub async fn list_files(vfs: &Vfs, dir: impl AsRef<Path>, name_glob: &str) -> Result<Vec<PathBuf>> {
    list_files_inner(vfs, dir.as_ref(), name_glob, /* follow */ false).await
}

/// Like [`list_files`], but follow symlinks so a symlink pointing at a
/// regular file is returned as one of the matches. Ports the shell's
/// `find -L <dir> -type f -name <glob>` idiom used by
/// `assemble_bootconfig` where RPM packages sometimes ship snippets as
/// symlinks into `/usr/share`.
pub async fn list_files_follow(
    vfs: &Vfs,
    dir: impl AsRef<Path>,
    name_glob: &str,
) -> Result<Vec<PathBuf>> {
    list_files_inner(vfs, dir.as_ref(), name_glob, /* follow */ true).await
}

async fn list_files_inner(
    vfs: &Vfs,
    dir: &Path,
    name_glob: &str,
    follow: bool,
) -> Result<Vec<PathBuf>> {
    let dir_q = shell_quote_path(dir);
    let glob_q = shell_quote(name_glob);
    let mut args: Vec<&str> = Vec::with_capacity(11);
    if follow {
        args.push("-L");
    }
    args.push(dir_q.as_str());
    args.extend_from_slice(&["-mindepth", "1", "-maxdepth", "1", "-type", "f", "-name"]);
    args.push(glob_q.as_str());
    let out = vfs.output("list_files", "find", &args).await?;
    Ok(parse_find_output(&out))
}

/// List every entry directly under `dir` (files, directories, and
/// symlinks). Returns paths sorted by their string representation.
///
/// Runs `find <dir> -mindepth 1 -maxdepth 1`; the directory path is
/// shell-quoted.
pub async fn list_dir_all(vfs: &Vfs, dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let dir_q = shell_quote_path(dir.as_ref());
    let out = vfs
        .output(
            "list_dir_all",
            "find",
            &[dir_q.as_str(), "-mindepth", "1", "-maxdepth", "1"],
        )
        .await?;
    Ok(parse_find_output(&out))
}

/// Resolve a symlink at `path` and return its target as an unparsed
/// string (may be relative). Implemented via `readlink`; fails if
/// `path` is not a symlink.
pub async fn read_link(vfs: &Vfs, path: impl AsRef<Path>) -> Result<PathBuf> {
    let quoted = shell_quote_path(path.as_ref());
    let out = vfs
        .output("read_link", "readlink", &[quoted.as_str()])
        .await?;
    let text = String::from_utf8_lossy(&out);
    Ok(PathBuf::from(text.trim()))
}

/// Parse the stdout of a `find ...` call: split on newlines, drop
/// empty lines, sort, and materialize as `PathBuf`s.
fn parse_find_output(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<PathBuf> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    lines.sort();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_find_output_sorts_and_strips_blanks() {
        let bytes = b"/tmp/b.rpm\n/tmp/a.rpm\n\n/tmp/c.rpm\n";
        let out = parse_find_output(bytes);
        assert_eq!(
            out,
            vec![
                PathBuf::from("/tmp/a.rpm"),
                PathBuf::from("/tmp/b.rpm"),
                PathBuf::from("/tmp/c.rpm"),
            ]
        );
    }

    #[test]
    fn parse_find_output_empty_input() {
        assert!(parse_find_output(b"").is_empty());
        assert!(parse_find_output(b"\n\n").is_empty());
    }

    #[test]
    fn parse_find_output_no_trailing_newline() {
        let bytes = b"/x\n/y";
        let out = parse_find_output(bytes);
        assert_eq!(out, vec![PathBuf::from("/x"), PathBuf::from("/y")]);
    }

    #[test]
    fn shell_quote_preserves_ordinary_string() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_wraps_glob_metacharacters_verbatim() {
        // The whole point: `*` must survive intact so `find -name *.rpm`
        // isn't glob-expanded by the enclosing `sh -c`.
        assert_eq!(shell_quote("*.rpm"), "'*.rpm'");
        assert_eq!(shell_quote("*"), "'*'");
        assert_eq!(shell_quote("?[a-z]"), "'?[a-z]'");
        assert_eq!(shell_quote("{a,b}.txt"), "'{a,b}.txt'");
    }

    #[test]
    fn shell_quote_wraps_path_with_spaces() {
        assert_eq!(
            shell_quote("/tmp/dir with spaces/foo"),
            "'/tmp/dir with spaces/foo'"
        );
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        // POSIX idiom: close the quote, insert an escaped ', reopen.
        // `it's` becomes `'it'\''s'`.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        // Multiple quotes and a leading quote are handled the same way.
        assert_eq!(shell_quote("'a'"), "''\\''a'\\'''");
    }

    #[test]
    fn shell_quote_handles_shell_special_chars() {
        // Backticks, dollar signs, backslashes are all inert inside single quotes.
        assert_eq!(shell_quote("$HOME `pwd` \\n"), "'$HOME `pwd` \\n'");
        assert_eq!(shell_quote("a;b&&c||d"), "'a;b&&c||d'");
        assert_eq!(shell_quote("a>b<c|d"), "'a>b<c|d'");
    }

    #[test]
    fn shell_quote_empty_string() {
        // An empty string quotes to two single quotes — a valid shell
        // token that expands to nothing.
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_path_matches_shell_quote_of_string() {
        assert_eq!(
            shell_quote_path(Path::new("/tmp/a b.rpm")),
            shell_quote("/tmp/a b.rpm")
        );
    }
}
