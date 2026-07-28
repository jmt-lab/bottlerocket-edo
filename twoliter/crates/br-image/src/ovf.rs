//! OVF/OVA generation. Ports `imghelper::generate_ova`.
//!
//! The pure part — [`render_ovf`] — is unit-tested here. The tar+sha256
//! assembly runs subprocesses (`tar`, `sha256sum` or its Rust
//! equivalent) and is skeleton-only until a transform driver exists.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::error::{Error, Result};

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

/// Assemble an OVA tarball containing `ovf`, a manifest, and the vmdk(s).
/// Ports the `tar` invocations in `generate_ova`. Skeleton: the tar
/// assembly step needs a real `tar` binary or an in-Rust tar writer.
pub async fn assemble_ova(
    _output_dir: &Path,
    _os_vmdk: &Path,
    _data_vmdk: Option<&Path>,
    _rendered_ovf: &str,
    _ova_output: &Path,
) -> Result<PathBuf> {
    Err(Error::InvalidLayout {
        context: "assemble_ova".into(),
        reason: "not implemented in br-image; call from the variant-image transform driver".into(),
    })
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
}
