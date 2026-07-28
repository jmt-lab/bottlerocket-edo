//! OVF/OVA generation. Ports `imghelper::generate_ova`.
//!
//! The pure part — [`render_ovf`] — is unit-tested here. The tar+sha256
//! assembly runs subprocesses (`tar`, `sha256sum` or its Rust
//! equivalent) and is skeleton-only until a transform driver exists.

use std::path::{Path, PathBuf};

use edo::environment::Vfs;
use regex::Regex;

use snafu::OptionExt;

use crate::error::{Error, InvalidLayoutSnafu, Result};
use crate::vfs_ext::shell_quote_path;

/// Bytes-per-GiB used to render the disk-size fields in the OVF.
pub const BYTES_IN_GIB: u64 = 1024 * 1024 * 1024;

/// Placeholder tokens the OVF template must (or must not) contain.
pub const OS_DISK_TOKEN: &str = "{{OS_DISK}}";
pub const DATA_DISK_TOKEN: &str = "{{DATA_DISK}}";
pub const OS_DISK_BYTES_TOKEN: &str = "{{OS_DISK_BYTES}}";
pub const DATA_DISK_BYTES_TOKEN: &str = "{{DATA_DISK_BYTES}}";
pub const PK_CERT_DER_HEX_TOKEN: &str = "{{PK_CERT_DER_HEX}}";
pub const KEK_CERT_DER_HEX_TOKEN: &str = "{{KEK_CERT_DER_HEX}}";
pub const DB_CERT_DER_HEX_TOKEN: &str = "{{DB_CERT_DER_HEX}}";
pub const DBX_EMPTY_HASH_HEX_TOKEN: &str = "{{DBX_EMPTY_HASH_HEX}}";

/// Hex-encoded DER certs used by [`render_ovf`] when secure boot is on.
#[derive(Debug, Clone, Default)]
pub struct SecureBootCerts {
    pub pk_der_hex: String,
    pub kek_der_hex: String,
    pub db_der_hex: String,
    pub dbx_empty_sha256_hex: String,
}

/// Render an OVF envelope from `template`. Ports the `sed` invocations
/// in `imghelper::generate_ova`.
///
/// - `os_vmdk_name` / `data_vmdk_name` are the filenames referenced in
///   the OVF; `data_vmdk_name = ""` for `unified` layouts.
/// - `os_gib` / `data_gib` are the publish-time sizes in GiB (per
///   [`crate::model::ImageLayout::publish_image_sizes_gib`]).
/// - When `certs` is `Some`, the four secure-boot placeholders are also
///   substituted.
///
/// Fails with [`Error::InvalidLayout`] if the rendered text still
/// contains any `{{...}}` token.
pub fn render_ovf(
    template: &str,
    os_vmdk_name: &str,
    data_vmdk_name: &str,
    os_gib: u32,
    data_gib: u32,
    certs: Option<&SecureBootCerts>,
) -> Result<String> {
    let os_bytes = os_gib as u64 * BYTES_IN_GIB;
    let data_bytes = data_gib as u64 * BYTES_IN_GIB;
    let mut text = template.to_string();
    text = text.replace(OS_DISK_TOKEN, os_vmdk_name);
    text = text.replace(DATA_DISK_TOKEN, data_vmdk_name);
    text = text.replace(OS_DISK_BYTES_TOKEN, &os_bytes.to_string());
    text = text.replace(DATA_DISK_BYTES_TOKEN, &data_bytes.to_string());
    if let Some(c) = certs {
        text = text.replace(PK_CERT_DER_HEX_TOKEN, &c.pk_der_hex);
        text = text.replace(KEK_CERT_DER_HEX_TOKEN, &c.kek_der_hex);
        text = text.replace(DB_CERT_DER_HEX_TOKEN, &c.db_der_hex);
        text = text.replace(DBX_EMPTY_HASH_HEX_TOKEN, &c.dbx_empty_sha256_hex);
    }
    // Scan for surviving `{{TOKEN}}` placeholders. A bare `{{` / `}}`
    // check would false-positive on legitimate content containing
    // double-braces; restrict to the placeholder shape.
    let leftover = leftover_placeholders(&text);
    if !leftover.is_empty() {
        return Err(Error::InvalidLayout {
            context: "ovf-template".into(),
            reason: format!(
                "template still contains unresolved placeholder(s): {}",
                leftover.join(", ")
            ),
        });
    }
    Ok(text)
}

/// Return every `{{TOKEN}}` remaining in `text` (uppercase ASCII, digits,
/// and underscore). Deduplicated in first-seen order.
fn leftover_placeholders(text: &str) -> Vec<String> {
    // A regex compiled once per call is fine here; render_ovf is not on
    // a hot path.
    let re =
        Regex::new(r"\{\{([A-Z_][A-Z0-9_]*)\}\}").expect("leftover_placeholders regex is valid");
    let mut seen = Vec::new();
    for cap in re.captures_iter(text) {
        let tok = format!("{{{{{}}}}}", &cap[1]);
        if !seen.contains(&tok) {
            seen.push(tok);
        }
    }
    seen
}

/// Render the `.mf` manifest text an OVA package contains.
///
/// Each `entries` pair is `(referenced_filename, hex_sha256)`; the
/// output is one `SHA256(<name>)= <hex>\n` line per entry, in the order
/// given (which must match the `References` section of the OVF per
/// DSP0243 § OVA manifest). Pure so it can be unit-tested without a
/// live filesystem.
pub fn render_ova_manifest(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, hex) in entries {
        out.push_str(&format!("SHA256({name})= {hex}\n"));
    }
    out
}

/// Compute the sha256 of `path` by shelling out to `sha256sum` and
/// parsing the first whitespace-delimited token from its stdout. The
/// GNU coreutils format is `<hex>  <path>` — we ignore the path so
/// this works whether `sha256sum` printed an absolute or relative
/// name.
///
/// Exposed as a helper so [`assemble_ova`] and any future OVA-related
/// callers do not each need to repeat the parse.
pub async fn sha256_of(vfs: &Vfs, path: &Path) -> Result<String> {
    let path_q = shell_quote_path(path);
    let stdout = vfs.output("ova_sha256", "sha256sum", &[path_q.as_str()]).await?;
    let text = String::from_utf8_lossy(&stdout);
    let first = text.split_whitespace().next().ok_or_else(|| Error::ParseOutput {
        tool: "sha256sum".into(),
        reason: format!("empty output for {}", path.display()),
    })?;
    if first.len() != 64 || !first.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::ParseOutput {
            tool: "sha256sum".into(),
            reason: format!("unexpected token {first:?} for {}", path.display()),
        });
    }
    Ok(first.to_string())
}

/// Assemble an OVA tarball. Ports the `tar` block at
/// `old-twoliter/twoliter/embedded/imghelper::generate_ova` L755-790.
///
/// The OVA is a plain tar file with three or four members, in this
/// specific order (per DSP0243 § "OVA File Format"): the OVF, the
/// manifest (`.mf`), then each referenced disk file in the same order
/// the OVF's `References` section names them.
///
/// - `ovf`: absolute path to the already-rendered OVF *inside* `vfs`.
///   Its directory is the "OVF dir".
/// - `output_dir`: directory containing `os_vmdk` and (if present)
///   `data_vmdk`. Usually the transform's `out/` dir.
/// - `os_vmdk` / `data_vmdk`: filenames or absolute paths of the vmdks
///   as they appear in `output_dir`.
/// - `ova_output`: final `.ova` file path.
///
/// Returns the OVA path on success.
pub async fn assemble_ova(
    vfs: &Vfs,
    output_dir: &Path,
    os_vmdk: &Path,
    data_vmdk: Option<&Path>,
    ovf: &Path,
    ova_output: &Path,
) -> Result<PathBuf> {
    // File names (leaf only) — used verbatim in the manifest and as
    // `tar -C <dir> <name>` args.
    let ovf_dir = ovf
        .parent()
        .ok_or_else(|| Error::InvalidLayout {
            context: "assemble_ova".into(),
            reason: format!("ovf path {} has no parent directory", ovf.display()),
        })?;
    let ovf_name = leaf_name(ovf, "ovf")?;
    let os_vmdk_name = leaf_name(os_vmdk, "os_vmdk")?;
    let data_vmdk_name = match data_vmdk {
        Some(p) => Some(leaf_name(p, "data_vmdk")?),
        None => None,
    };

    // 1. Compute sha256 for each referenced file. Order matches
    //    DSP0243: OVF first, then every file appearing in `References`
    //    (OS disk, then data disk).
    let os_sha = sha256_of(vfs, &output_dir.join(&os_vmdk_name)).await?;
    let data_sha = match &data_vmdk_name {
        Some(name) => Some(sha256_of(vfs, &output_dir.join(name)).await?),
        None => None,
    };
    let ovf_sha = sha256_of(vfs, ovf).await?;

    // 2. Build the manifest text and write it next to the OVF.
    let mut manifest_entries: Vec<(String, String)> = Vec::with_capacity(4);
    manifest_entries.push((os_vmdk_name.clone(), os_sha));
    if let (Some(name), Some(hex)) = (&data_vmdk_name, &data_sha) {
        manifest_entries.push((name.clone(), hex.clone()));
    }
    manifest_entries.push((ovf_name.clone(), ovf_sha));
    let manifest_text = render_ova_manifest(&manifest_entries);
    let manifest_name = format!(
        "{}.mf",
        std::path::Path::new(&ovf_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("bottlerocket")
    );
    let manifest_path = ovf_dir.join(&manifest_name);
    vfs.write(&manifest_path, manifest_text.as_bytes()).await?;

    // 3. tar -cvf <ova> -C <ovf_dir> <ovf> <mf>
    let ova_q = shell_quote_path(ova_output);
    let ovf_dir_q = shell_quote_path(ovf_dir);
    let output_dir_q = shell_quote_path(output_dir);
    let ovf_name_q = crate::vfs_ext::shell_quote(&ovf_name);
    let manifest_name_q = crate::vfs_ext::shell_quote(&manifest_name);
    vfs.command(
        "ova_tar_create",
        "tar",
        &[
            "-cvf",
            ova_q.as_str(),
            "-C",
            ovf_dir_q.as_str(),
            ovf_name_q.as_str(),
            manifest_name_q.as_str(),
        ],
    )
    .await?;

    // 4. tar -rvf <ova> -C <output_dir> <os_vmdk>
    let os_vmdk_q = crate::vfs_ext::shell_quote(&os_vmdk_name);
    vfs.command(
        "ova_tar_append_os",
        "tar",
        &[
            "-rvf",
            ova_q.as_str(),
            "-C",
            output_dir_q.as_str(),
            os_vmdk_q.as_str(),
        ],
    )
    .await?;

    // 5. tar -rvf <ova> -C <output_dir> <data_vmdk> — split plan only.
    if let Some(name) = data_vmdk_name {
        let data_q = crate::vfs_ext::shell_quote(&name);
        vfs.command(
            "ova_tar_append_data",
            "tar",
            &[
                "-rvf",
                ova_q.as_str(),
                "-C",
                output_dir_q.as_str(),
                data_q.as_str(),
            ],
        )
        .await?;
    }

    Ok(ova_output.to_path_buf())
}

/// Extract the leaf filename of `path` as a UTF-8 String. Fails with
/// [`Error::InvalidLayout`] when the path has no file name (root, `..`,
/// etc.) or when the leaf contains non-UTF-8 bytes — both cases would
/// break the OVA tar's `-C <dir> <name>` construction.
fn leaf_name(path: &Path, role: &str) -> Result<String> {
    let os_name = path.file_name().context(InvalidLayoutSnafu {
        context: "assemble_ova".to_string(),
        reason: format!("{role} path {} has no filename component", path.display()),
    })?;
    let s = os_name.to_str().context(InvalidLayoutSnafu {
        context: "assemble_ova".to_string(),
        reason: format!("{role} filename is not valid UTF-8: {}", path.display()),
    })?;
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NON_SB_TEMPLATE: &str = "\
<Envelope>
  <File href=\"{{OS_DISK}}\" size=\"{{OS_DISK_BYTES}}\"/>
  <File href=\"{{DATA_DISK}}\" size=\"{{DATA_DISK_BYTES}}\"/>
</Envelope>
";

    const SB_TEMPLATE: &str = "\
<Envelope>
  <File href=\"{{OS_DISK}}\" size=\"{{OS_DISK_BYTES}}\"/>
  <PK>{{PK_CERT_DER_HEX}}</PK>
  <KEK>{{KEK_CERT_DER_HEX}}</KEK>
  <db>{{DB_CERT_DER_HEX}}</db>
  <dbx>{{DBX_EMPTY_HASH_HEX}}</dbx>
</Envelope>
";

    #[test]
    fn renders_non_secure_boot() {
        let out = render_ovf(NON_SB_TEMPLATE, "os.vmdk", "data.vmdk", 2, 20, None).unwrap();
        assert!(out.contains("os.vmdk"));
        assert!(out.contains("data.vmdk"));
        assert!(out.contains(&(2u64 * BYTES_IN_GIB).to_string()));
        assert!(out.contains(&(20u64 * BYTES_IN_GIB).to_string()));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn renders_secure_boot_fields() {
        let certs = SecureBootCerts {
            pk_der_hex: "aa".into(),
            kek_der_hex: "bb".into(),
            db_der_hex: "cc".into(),
            dbx_empty_sha256_hex: "dd".into(),
        };
        let template = format!(
            "{{{{OS_DISK}}}}={{{{OS_DISK_BYTES}}}} {{{{DATA_DISK}}}}={{{{DATA_DISK_BYTES}}}}\n{}",
            SB_TEMPLATE
        );
        let out = render_ovf(&template, "os.vmdk", "data.vmdk", 2, 20, Some(&certs)).unwrap();
        assert!(out.contains("<PK>aa</PK>"));
        assert!(out.contains("<KEK>bb</KEK>"));
        assert!(out.contains("<db>cc</db>"));
        assert!(out.contains("<dbx>dd</dbx>"));
    }

    #[test]
    fn errors_when_placeholder_remains() {
        let template = "no {{THIS_DOES_NOT_EXIST}} substitution";
        let err = render_ovf(template, "a", "b", 1, 1, None).unwrap_err();
        assert!(
            err.to_string().contains("{{THIS_DOES_NOT_EXIST}}"),
            "err: {err}"
        );
    }

    #[test]
    fn secure_boot_without_certs_errors() {
        let err = render_ovf(SB_TEMPLATE, "os.vmdk", "", 2, 0, None).unwrap_err();
        // At least one of the four SB tokens must survive.
        let msg = err.to_string();
        assert!(
            msg.contains("PK_CERT_DER_HEX")
                || msg.contains("KEK_CERT_DER_HEX")
                || msg.contains("DB_CERT_DER_HEX")
                || msg.contains("DBX_EMPTY_HASH_HEX"),
            "err: {err}"
        );
    }

    #[test]
    fn accepts_literal_double_braces_in_non_placeholder_shape() {
        // A `{{ x }}` shape (with lowercase or spaces) should not
        // trigger the leftover-placeholder check.
        let text = "{{ ok }} and {{lower_case}} and {{Mixed}}";
        assert!(leftover_placeholders(text).is_empty());
    }

    #[test]
    fn deduplicates_leftover_tokens() {
        let text = "{{FOO}} then {{FOO}} then {{BAR}}";
        let out = leftover_placeholders(text);
        assert_eq!(out, vec!["{{FOO}}", "{{BAR}}"]);
    }

    #[test]
    fn render_ova_manifest_matches_shell_layout() {
        // The old imghelper writes lines like
        //   SHA256(os.vmdk)= <hex>
        //   SHA256(data.vmdk)= <hex>
        //   SHA256(bottlerocket.ovf)= <hex>
        // in exactly that order. Assert on the raw bytes.
        let entries = vec![
            ("bottlerocket-vmware-k8s.vmdk".to_string(), "aa".repeat(32)),
            ("bottlerocket-vmware-k8s-data.vmdk".to_string(), "bb".repeat(32)),
            ("bottlerocket-vmware-k8s.ovf".to_string(), "cc".repeat(32)),
        ];
        let text = render_ova_manifest(&entries);
        let expected = format!(
            "SHA256(bottlerocket-vmware-k8s.vmdk)= {aa}\n\
             SHA256(bottlerocket-vmware-k8s-data.vmdk)= {bb}\n\
             SHA256(bottlerocket-vmware-k8s.ovf)= {cc}\n",
            aa = "aa".repeat(32),
            bb = "bb".repeat(32),
            cc = "cc".repeat(32),
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn render_ova_manifest_empty_input_produces_empty_string() {
        assert_eq!(render_ova_manifest(&[]), "");
    }

    #[test]
    fn render_ova_manifest_preserves_entry_order() {
        // Reordering entries produces a different manifest — the OVA
        // spec requires the manifest match the OVF's References order
        // exactly, so this is load-bearing.
        let a = vec![
            ("a".to_string(), "1".repeat(64)),
            ("b".to_string(), "2".repeat(64)),
        ];
        let b = vec![
            ("b".to_string(), "2".repeat(64)),
            ("a".to_string(), "1".repeat(64)),
        ];
        assert_ne!(render_ova_manifest(&a), render_ova_manifest(&b));
    }
}
