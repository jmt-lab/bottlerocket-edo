//! Stages a variant's OS and data images out of the edo artifact cache.
//!
//! Old pubsys was handed already-decompressed `.img` files by the
//! `[tasks.ami]` cargo-make target, which unlz4'd them out of
//! `build/images/<arch>-<variant>/<version>-<build-id>/`. In twoliter 1.x
//! that directory is a tar layer on the `//variants/<name>` artifact, so we
//! resolve the artifact the same way `publish kit` does and decode the
//! images straight out of the layer stream.

use crate::cmd::create_context;
use async_compression::tokio::bufread::{
    BzDecoder, GzipDecoder, Lz4Decoder, LzmaDecoder, XzDecoder, ZstdDecoder,
};
use edo::context::Addr;
use edo::storage::{Compression, MediaType};
use futures::StreamExt;
use snafu::{OptionExt, ResultExt, ensure};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use tempfile::TempDir;
use tokio::fs::{File, create_dir_all};
use tokio::io::{AsyncRead, BufReader, copy};
use tokio_tar::Archive;
use tracing::{debug, info};

/// `imghelper.sh` names the compressed OS image `<prefix>.img.lz4` and the
/// data image `<prefix>-data.img.lz4`.
const IMAGE_SUFFIX: &str = ".img.lz4";
const DATA_IMAGE_SUFFIX: &str = "-data.img.lz4";

/// The decompressed images staged out of the artifact cache.
///
/// Dropping this deletes the staged files, so it must outlive the snapshot
/// uploads that read them.
pub(crate) struct StagedImages {
    pub(crate) os_image: PathBuf,
    pub(crate) data_image: Option<PathBuf>,
    _dir: TempDir,
}

/// Resolves `//variants/<variant>` for `arch` and decodes its OS and
/// (optional) data images into a temporary directory.
pub(crate) async fn stage_variant_images(
    args: &crate::Args,
    variant: &str,
    arch: &str,
) -> Result<StagedImages> {
    let addr_s = format!("//variants/{variant}");
    let addr = Addr::parse(&addr_s)?;
    let ctx = create_context(
        args,
        &addr_s,
        HashMap::from([("arch".to_string(), arch.to_string())]),
        true,
        false,
    )
    .await?;

    let handle = ctx.get_handle();
    let transform = handle.get(&addr).context(error::NoVariantSnafu {
        addr: addr_s.clone(),
    })?;
    let id = transform.get_unique_id(&handle).await?;
    ensure!(
        ctx.storage().has_local(&id).await?,
        error::MissingArtifactSnafu { variant, arch }
    );
    debug!("Found variant-image artifact {id:?} for {addr_s} ({arch})");

    // Stage alongside the artifact cache rather than in `/tmp`: these are
    // multi-gigabyte raw images and the cache is on a filesystem sized for
    // them.
    let staging_root = args
        .storage
        .clone()
        .unwrap_or_else(|| PathBuf::from(".edo"))
        .join("publish");
    create_dir_all(&staging_root)
        .await
        .context(error::StagingDirSnafu {
            path: staging_root.clone(),
        })?;
    let dir = tempfile::Builder::new()
        .prefix("ami-")
        .tempdir_in(&staging_root)
        .context(error::StagingDirSnafu {
            path: staging_root.clone(),
        })?;

    let artifact = ctx.storage().safe_open(&id).await?;
    let mut os_image = None;
    let mut data_image = None;
    for layer in artifact.layers() {
        let MediaType::Tar(compression) = layer.media_type() else {
            continue;
        };
        let reader = decode(
            BufReader::new(ctx.storage().safe_read(layer).await?),
            compression,
        );
        let mut archive = Archive::new(reader);
        let mut entries = archive.entries().context(error::ReadLayerSnafu)?;
        while let Some(entry) = entries.next().await {
            let mut entry = entry.context(error::ReadLayerSnafu)?;
            // The output directory also holds stable-name symlinks to each
            // image; only the regular files carry data.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path().context(error::ReadLayerSnafu)?.to_path_buf();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".lz4") else {
                continue;
            };
            if !name.ends_with(IMAGE_SUFFIX) {
                continue;
            }
            let (slot, kind) = if name.ends_with(DATA_IMAGE_SUFFIX) {
                (&mut data_image, "data")
            } else {
                (&mut os_image, "os")
            };
            ensure!(
                slot.is_none(),
                error::MultipleImagesSnafu {
                    kind,
                    variant,
                    arch
                }
            );

            let dest = dir.path().join(stem);
            info!("Staging {kind} image from {addr_s} to {}", dest.display());
            let mut decoder = Lz4Decoder::new(BufReader::new(&mut entry));
            let mut file = File::create(&dest)
                .await
                .context(error::WriteImageSnafu { path: dest.clone() })?;
            copy(&mut decoder, &mut file)
                .await
                .context(error::WriteImageSnafu { path: dest.clone() })?;
            *slot = Some(dest);
        }
    }

    let os_image = os_image.context(error::NoOsImageSnafu { variant, arch })?;
    Ok(StagedImages {
        os_image,
        data_image,
        _dir: dir,
    })
}

/// Wraps `reader` in the decompressor matching the layer's compression.
fn decode<R: tokio::io::AsyncBufRead + Send + Unpin + 'static>(
    reader: R,
    compression: &Compression,
) -> Pin<Box<dyn AsyncRead + Send>> {
    match compression {
        Compression::Bzip2 => Box::pin(BzDecoder::new(reader)),
        Compression::Lz4 => Box::pin(Lz4Decoder::new(reader)),
        Compression::Lzma => Box::pin(LzmaDecoder::new(reader)),
        Compression::Xz => Box::pin(XzDecoder::new(reader)),
        Compression::Gzip => Box::pin(GzipDecoder::new(reader)),
        Compression::Zstd => Box::pin(ZstdDecoder::new(reader)),
        _ => Box::pin(reader),
    }
}

mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(transparent)]
        Root { source: crate::error::Error },

        #[snafu(transparent)]
        Context { source: edo::context::ContextError },

        #[snafu(transparent)]
        Storage { source: edo::storage::StorageError },

        #[snafu(transparent)]
        Transform {
            source: edo::transform::TransformError,
        },

        #[snafu(display(
            "No variant-image artifact for '{variant}' ({arch}) in the build cache; run \
             `twoliter build variant {variant}` first"
        ))]
        MissingArtifact { variant: String, arch: String },

        #[snafu(display(
            "Found more than one {} image for '{}' ({}) in the variant artifact",
            kind,
            variant,
            arch
        ))]
        MultipleImages {
            kind: String,
            variant: String,
            arch: String,
        },

        #[snafu(display("There is no variant in the project at address {addr}"))]
        NoVariant { addr: String },

        #[snafu(display(
            "The variant artifact for '{}' ({}) has no '*{}' image; only variants built in \
             'raw' image format can be registered as AMIs",
            variant,
            arch,
            super::IMAGE_SUFFIX
        ))]
        NoOsImage { variant: String, arch: String },

        #[snafu(display("Failed to read the variant artifact's image layer: {}", source))]
        ReadLayer { source: std::io::Error },

        #[snafu(display("Failed to create staging directory '{}': {}", path.display(), source))]
        StagingDir {
            path: PathBuf,
            source: std::io::Error,
        },

        #[snafu(display("Failed to write staged image '{}': {}", path.display(), source))]
        WriteImage {
            path: PathBuf,
            source: std::io::Error,
        },
    }
}
pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;
