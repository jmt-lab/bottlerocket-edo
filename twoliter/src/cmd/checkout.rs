use std::collections::HashMap;
use std::path::PathBuf;

use crate::Result;
use crate::error;
use async_compression::tokio::bufread::BzDecoder;
use async_compression::tokio::bufread::GzipDecoder;
use async_compression::tokio::bufread::Lz4Decoder;
use async_compression::tokio::bufread::LzmaDecoder;
use async_compression::tokio::bufread::XzDecoder;
use async_compression::tokio::bufread::ZstdDecoder;
use clap::Parser;
use edo::context::Addr;
use edo::storage::Compression;
use edo::storage::MediaType;
use snafu::{OptionExt, ResultExt};
use std::pin::Pin;
use tokio::fs::{File, create_dir_all};
use tokio::io::{AsyncRead, BufReader, copy};
use tokio_tar::Archive;

use crate::Args;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Checkout an artifact to local directory", long_about = None)]
pub struct Checkout {
    addr: String,
    output: PathBuf,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl Checkout {
    pub async fn run(&self, args: &Args) -> Result<()> {
        let ctx = super::create_context(
            &args,
            &self.addr,
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
            false,
        )
        .await?;
        let addr = Addr::parse(self.addr.as_str())?;
        let transform = ctx.get_transform(&addr).unwrap();
        let handle = ctx.get_handle();
        let id = transform.get_unique_id(&handle).await?;
        let artifact = ctx.storage().safe_open(&id).await?;
        if !self.output.exists() {
            create_dir_all(&self.output).await.context(error::IoSnafu)?;
        }
        for layer in artifact.layers() {
            // Do different things depending on the media_type
            let reader = BufReader::new(ctx.storage().safe_read(layer).await?);
            match layer.media_type() {
                MediaType::Tar(compression) => {
                    let reader: Pin<Box<dyn AsyncRead>> = match compression {
                        Compression::Bzip2 => Box::pin(BzDecoder::new(reader)),
                        Compression::Lz4 => Box::pin(Lz4Decoder::new(reader)),
                        Compression::Lzma => Box::pin(LzmaDecoder::new(reader)),
                        Compression::Xz => Box::pin(XzDecoder::new(reader)),
                        Compression::Gzip => Box::pin(GzipDecoder::new(reader)),
                        Compression::Zstd => Box::pin(ZstdDecoder::new(reader)),
                        _ => Box::pin(reader),
                    };
                    let mut archive = Archive::new(reader);
                    archive.unpack(&self.output).await.context(error::IoSnafu)?;
                }
                value => {
                    // Non-archive layer: honor the artifact's path hint
                    // (relative path recorded when the layer was
                    // emitted) and stream the raw layer bytes to
                    // `<output>/<hint>`. If no hint was recorded we
                    // refuse to guess — a digest-named file with an
                    // unknown extension is worse than a clear error
                    // pointing at the producing transform.
                    let hint = artifact
                        .config()
                        .path_hint_for(layer.digest())
                        .with_context(|| error::CheckoutLayerMissingPathHintSnafu {
                            media_type: value.to_string(),
                            digest: layer.digest().digest(),
                        })?;
                    let dest = self.output.join(hint);
                    if let Some(parent) = dest.parent() {
                        if !parent.as_os_str().is_empty() {
                            create_dir_all(parent).await.context(error::IoSnafu)?;
                        }
                    }
                    tracing::debug!(
                        "writing layer with media_type {value} to {}",
                        dest.display()
                    );
                    let mut reader: Pin<Box<dyn AsyncRead>> = Box::pin(reader);
                    let mut file = File::create(&dest).await.context(error::IoSnafu)?;
                    copy(&mut reader, &mut file).await.context(error::IoSnafu)?;
                }
            }
        }
        Ok(())
    }
}
