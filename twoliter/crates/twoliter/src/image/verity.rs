//! dm-verity hash tree computation. Ports
//! `imghelper::generate_verity_root`.

use std::path::Path;

use edo::environment::Vfs;

use snafu::OptionExt;

use crate::image::error::{Error, Result, VeritySnafu};
use crate::image::vfs_ext::{shell_quote_path, size_bytes};

/// Standard verity parameters used by every Bottlerocket image.
pub const VERITY_VERSION: u32 = 1;
pub const VERITY_HASH_ALGORITHM: &str = "sha256";
pub const VERITY_DATA_BLOCK_SIZE: u32 = 4096;
pub const VERITY_HASH_BLOCK_SIZE: u32 = 4096;

/// Result of `veritysetup format`. The transform driver embeds a
/// rendered representation of these fields in the grub config as the
/// `DM_VERITY_ROOT` argument.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VerityInfo {
    pub root_hash: String,
    pub salt: String,
    /// Data blocks at `VERITY_DATA_BLOCK_SIZE`, as reported by
    /// veritysetup.
    pub data_blocks_4k: u64,
    /// Data blocks at 512 B (used by the kernel's `dm-mod.create=`
    /// argument).
    pub data_blocks_512: u64,
    /// Hash algorithm identifier (always `"sha256"` today; stored as
    /// an owned `String` so `VerityInfo` can round-trip through JSON).
    pub hash_algorithm: String,
    pub data_block_size: u32,
    pub hash_block_size: u32,
}

impl VerityInfo {
    /// Build the space-separated 16-field row the shell placed in
    /// `DM_VERITY_ROOT`.
    ///
    /// The row is passed directly to the kernel via `dm-mod.create=`
    /// and looks like:
    /// ```text
    /// root,,,ro,0 <blocks_512> verity 1 PARTUUID=$boot_uuid/PARTNROFF=1
    /// PARTUUID=$boot_uuid/PARTNROFF=2 4096 4096 <blocks_4k> 1 sha256
    /// <root_hash> <salt> 2 restart_on_corruption ignore_zero_blocks
    /// ```
    pub fn dm_verity_root_row(&self) -> String {
        format!(
            "root,,,ro,0 {blocks_512} verity {version} \
             PARTUUID=$boot_uuid/PARTNROFF=1 \
             PARTUUID=$boot_uuid/PARTNROFF=2 \
             {dbs} {hbs} {blocks_4k} 1 {algo} {root_hash} {salt} 2 \
             restart_on_corruption ignore_zero_blocks",
            blocks_512 = self.data_blocks_512,
            version = VERITY_VERSION,
            dbs = self.data_block_size,
            hbs = self.hash_block_size,
            blocks_4k = self.data_blocks_4k,
            algo = self.hash_algorithm,
            root_hash = self.root_hash,
            salt = self.salt,
        )
    }
}

/// Compute the dm-verity hash tree for `root_image`, writing the hash
/// image to `verity_image` and returning the parsed metadata. All I/O
/// runs inside `vfs`'s environment.
///
/// `root_content_bytes` is the number of *content* bytes in
/// `root_image`. `veritysetup format` hashes every 4 KiB block up to
/// that size, and the resulting `data_blocks` field in the returned
/// [`VerityInfo`] is what the kernel later verifies. If the on-disk
/// `root_image` file is larger than the actual filesystem content (e.g.
/// the whole partition slot was reserved before `dd`-ing in the erofs
/// image), pass the content size so the trailing unused blocks are not
/// signed — otherwise the kernel refuses reads to those blocks at boot.
///
/// The caller must have already ensured `root_content_bytes` is
/// - a multiple of [`VERITY_DATA_BLOCK_SIZE`], and
/// - less than or equal to `root_image`'s size on disk.
///
/// Sequence (matches `imghelper::generate_verity_root`):
/// 1. `truncate -s ${root_content_bytes} ${root_image}` (only when
///    `root_image` is currently larger than `root_content_bytes`).
/// 2. `truncate -s ${verity_mib}M ${verity_image}`
/// 3. `veritysetup format --format 1 --hash sha256 --data-block-size
///    4096 --hash-block-size 4096 <root> <verity>`
/// 4. `veritysetup verify <root> <verity> <root_hash>` for
///    defence-in-depth.
///
/// Skeleton: subprocess bodies are wired but not exercised in the crate's
/// unit tests (require a real veritysetup binary + kernel loop device).
pub async fn generate_verity_root(
    vfs: &Vfs,
    root_image: &Path,
    verity_image: &Path,
    verity_mib: u32,
    root_content_bytes: u64,
) -> Result<VerityInfo> {
    // Validate that content size is block-aligned so the kernel's
    // per-block hash check has nothing to fall off the end of.
    if !root_content_bytes.is_multiple_of(VERITY_DATA_BLOCK_SIZE as u64) {
        return Err(Error::Verity {
            reason: format!(
                "root_content_bytes {root_content_bytes} is not a multiple of \
                 VERITY_DATA_BLOCK_SIZE ({})",
                VERITY_DATA_BLOCK_SIZE
            ),
        });
    }
    if root_content_bytes == 0 {
        return Err(Error::Verity {
            reason: "root_content_bytes must be > 0".into(),
        });
    }

    // Truncate the root image down to the content size (only if it's
    // larger; growing here would mean the caller lied about its
    // content). This ensures every block that goes into the hash tree
    // is a block the kernel will actually read at boot.
    let root_len = size_bytes(vfs, root_image).await?;
    let root_image_q = shell_quote_path(root_image);
    let verity_image_q = shell_quote_path(verity_image);
    if root_len > root_content_bytes {
        let size_arg = root_content_bytes.to_string();
        vfs.command(
            "verity_truncate_root",
            "truncate",
            &["-s", size_arg.as_str(), root_image_q.as_str()],
        )
        .await?;
    } else if root_len < root_content_bytes {
        return Err(Error::Verity {
            reason: format!(
                "root_image {} is {} bytes on disk but caller claimed content is {} bytes",
                root_image.display(),
                root_len,
                root_content_bytes
            ),
        });
    }

    // Step 1: truncate the (empty) hash image to its reserved size.
    let size = format!("{verity_mib}M");
    vfs.command(
        "verity_truncate",
        "truncate",
        &["-s", size.as_str(), verity_image_q.as_str()],
    )
    .await?;

    // Step 2: veritysetup format. Capture stdout so we can parse the
    // root hash / salt / block count.
    let dbs = VERITY_DATA_BLOCK_SIZE.to_string();
    let hbs = VERITY_HASH_BLOCK_SIZE.to_string();
    let ver = VERITY_VERSION.to_string();
    let stdout = vfs
        .output(
            "verity_format",
            "veritysetup",
            &[
                "format",
                "--format",
                ver.as_str(),
                "--hash",
                VERITY_HASH_ALGORITHM,
                "--data-block-size",
                dbs.as_str(),
                "--hash-block-size",
                hbs.as_str(),
                root_image_q.as_str(),
                verity_image_q.as_str(),
            ],
        )
        .await?;

    let text = String::from_utf8_lossy(&stdout);
    let info = parse_veritysetup_output(&text)?;

    // Step 3: verify. The root-hash is a fixed-format hex string from
    // veritysetup itself (no metacharacters), but quote it for defense.
    let root_hash_q = crate::image::vfs_ext::shell_quote(&info.root_hash);
    vfs.command(
        "verity_verify",
        "veritysetup",
        &[
            "verify",
            root_image_q.as_str(),
            verity_image_q.as_str(),
            root_hash_q.as_str(),
        ],
    )
    .await?;

    Ok(info)
}

/// Parse the stdout of `veritysetup format` and return a [`VerityInfo`].
/// Exposed for unit testing.
pub fn parse_veritysetup_output(text: &str) -> Result<VerityInfo> {
    let mut root_hash = None::<String>;
    let mut salt = None::<String>;
    let mut data_blocks = None::<u64>;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Root hash:") {
            root_hash = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Salt:") {
            salt = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Data blocks:") {
            data_blocks = rest.trim().parse().ok();
        }
    }
    let root_hash = root_hash.with_context(|| VeritySnafu {
        reason: "veritysetup output missing `Root hash:`".to_string(),
    })?;
    let salt = salt.with_context(|| VeritySnafu {
        reason: "veritysetup output missing `Salt:`".to_string(),
    })?;
    let data_blocks_4k = data_blocks.with_context(|| VeritySnafu {
        reason: "veritysetup output missing `Data blocks:`".to_string(),
    })?;
    let data_blocks_512 = data_blocks_4k.checked_mul(8).with_context(|| VeritySnafu {
        reason: format!(
            "data_blocks_4k {data_blocks_4k} overflows when converted to 512-byte blocks"
        ),
    })?;
    Ok(VerityInfo {
        root_hash,
        salt,
        data_blocks_4k,
        data_blocks_512,
        hash_algorithm: VERITY_HASH_ALGORITHM.to_string(),
        data_block_size: VERITY_DATA_BLOCK_SIZE,
        hash_block_size: VERITY_HASH_BLOCK_SIZE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_veritysetup_output() {
        let text = "\
VERITY header information for /root.img
UUID:                   ...
Hash type:              1
Data blocks:            123456
Data block size:        4096
Hash block size:        4096
Hash algorithm:         sha256
Salt:                   deadbeefcafebabefeedfacefeeddeadbeefcafebabefeedfacefeeddeadbeef
Root hash:              1122334455667788990011223344556677889900112233445566778899001122
";
        let info = parse_veritysetup_output(text).unwrap();
        assert_eq!(info.data_blocks_4k, 123_456);
        assert_eq!(info.data_blocks_512, 123_456 * 8);
        assert_eq!(
            info.root_hash,
            "1122334455667788990011223344556677889900112233445566778899001122"
        );
        assert_eq!(
            info.salt,
            "deadbeefcafebabefeedfacefeeddeadbeefcafebabefeedfacefeeddeadbeef"
        );
    }

    #[test]
    fn missing_root_hash_errors() {
        let text = "Salt: aa\nData blocks: 1\n";
        let err = parse_veritysetup_output(text).unwrap_err();
        assert!(err.to_string().contains("Root hash"));
    }

    #[test]
    fn dm_verity_root_row_shape() {
        let info = VerityInfo {
            root_hash: "abcd".into(),
            salt: "ef01".into(),
            data_blocks_4k: 100,
            data_blocks_512: 800,
            hash_algorithm: VERITY_HASH_ALGORITHM.to_string(),
            data_block_size: VERITY_DATA_BLOCK_SIZE,
            hash_block_size: VERITY_HASH_BLOCK_SIZE,
        };
        let row = info.dm_verity_root_row();
        assert!(row.starts_with("root,,,ro,0 800 verity 1 "));
        assert!(row.contains("abcd"));
        assert!(row.contains("ef01"));
        assert!(row.contains("PARTNROFF=1"));
        assert!(row.contains("PARTNROFF=2"));
        assert!(row.ends_with("restart_on_corruption ignore_zero_blocks"));
    }
}
