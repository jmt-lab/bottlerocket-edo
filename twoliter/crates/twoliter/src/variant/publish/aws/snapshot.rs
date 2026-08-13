//! Upload a local file into an EBS snapshot via `coldsnap`.
//!
//! Ported from `old-twoliter/tools/pubsys/src/aws/ami/snapshot.rs`
//! with `log::` → `tracing::` and no other changes. `coldsnap` still
//! wants an on-disk path; the register orchestrator lands the
//! extracted image bytes in a `tempfile::NamedTempFile` before
//! calling us.
use coldsnap::{SnapshotUploader, UploadZeroBlocks};
use indicatif::{ProgressBar, ProgressStyle};
use snafu::{OptionExt, ResultExt};
use std::path::Path;

/// Progress bar template used for both the OS and data snapshot
/// upload. Match old-twoliter's rendering exactly.
pub(crate) fn build_progress_bar(verb: &str) -> Result<ProgressBar> {
    let progress_bar = ProgressBar::new(0);
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template(&["  ", verb, "  [{bar:50.white/black}] {pos}/{len} ({eta})"].concat())
            .context(error::ProgressBarTemplateSnafu)?
            .progress_chars("=> "),
    );
    Ok(progress_bar)
}

/// Upload the file at `path` as an EBS snapshot in the region tied
/// to `uploader`. Returns the resulting snapshot ID.
pub(crate) async fn snapshot_from_image<P>(
    path: P,
    uploader: &SnapshotUploader,
    desired_size: Option<i64>,
    progress_bar: Option<ProgressBar>,
) -> Result<String>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let filename = path
        .file_name()
        .context(error::InvalidImagePathSnafu {
            path: path.to_path_buf(),
        })?
        .to_string_lossy();

    uploader
        .upload_from_file(
            path,
            desired_size,
            Some(&filename),
            None,
            progress_bar,
            Some(UploadZeroBlocks::Omit),
            None,
            None,
        )
        .await
        .context(error::UploadSnapshotSnafu)
}

pub(crate) mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    #[allow(clippy::large_enum_variant)]
    pub enum Error {
        #[snafu(display("Invalid image path '{}'", path.display()))]
        InvalidImagePath { path: PathBuf },

        #[snafu(display("Failed to parse progress-bar template: {source}"))]
        ProgressBarTemplate {
            source: indicatif::style::TemplateError,
        },

        #[snafu(display("Failed to upload snapshot: {source}"))]
        UploadSnapshot {
            #[snafu(source(from(coldsnap::UploadError, Box::new)))]
            source: Box<coldsnap::UploadError>,
        },
    }
}
pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;
