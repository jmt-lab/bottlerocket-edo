//! Layer-emission helpers shared by every `variant-*` transform.
//!
//! The pattern
//!   1. `safe_start_layer` → writer
//!   2. `env.read_stream(src, writer)`
//!   3. `safe_finish_layer(writer, LayerOptions::media_type(Custom(...)))`
//!   4. Push layer, register `path_hint` for the layer digest so
//!      downstream `env.stage` puts the file back at the same
//!      relative path.
//!
//! …appears verbatim in every stage's `transform()`. Factoring the
//! sequence keeps each stage's transform body focused on what makes
//! that stage distinct rather than boilerplate.
//!
//! Also lives here:
//! - [`copy_guest_trees`]: the two-level walk over
//!   `deps/guest-images/<arch-name>/<version>/` that the rootfs
//!   stage does *before* mkfs.erofs.

use edo::{
    context::{Handle, Log},
    environment::{Environment, Vfs},
    storage::{Artifact, Compression, Layer, LayerOptions, MediaType},
    transform::TransformError,
};
use snafu::ResultExt;
use std::path::{Path, PathBuf};
use tracing::trace;

use super::error;

/// Push a single layer onto `artifact` from a file at `src`, tagging
/// it with `media`, and registering `hint` as its path hint so
/// downstream `env.stage` places it back at `<stage_path>/<hint>`.
///
/// The `type_` slot is the transform kind label used in trace logs
/// (e.g. `"variant-rootfs"`).
pub async fn write_layer(
    ctx: &Handle,
    env: &Environment,
    src: &Path,
    media: &str,
    hint: &str,
    type_: &str,
    artifact: &mut Artifact,
) -> Result<Layer, TransformError> {
    let writer = ctx.storage().safe_start_layer().await?;
    env.read_stream(src, writer.clone()).await?;
    let layer = ctx
        .storage()
        .safe_finish_layer(
            &writer,
            &LayerOptions::builder()
                .media_type(MediaType::Custom(media.to_string(), Compression::None))
                .build(),
        )
        .await?;
    let digest = layer.digest().digest();
    trace!(
        component = "transform",
        r#type = type_,
        "pushed layer {digest} ({media}) hint={hint}"
    );
    artifact.layers_mut().push(layer.clone());
    artifact
        .config_mut()
        .path_hints_mut()
        .insert(digest, PathBuf::from(hint));
    Ok(layer)
}

/// Walk `deps/guest-images/<arch-name>/<version>/` and copy each
/// leaf's artifacts into `root_mount/local/guest-images/<arch-name>/<version>/`.
///
/// Called during the rootfs transform's build step. The
/// `<arch-name>/<version>/` shape is invented by `stage()` (via
/// `env.stage` with a per-guest `path`), so this helper deals in
/// filesystem traversal only, not in edo addresses.
pub async fn copy_guest_trees(
    vfs: &Vfs,
    _log: &Log,
    guest_umbrella: &Path,
    root_mount_path: &Path,
) -> Result<(), error::Error> {
    if !vfs
        .try_exists(guest_umbrella)
        .await
        .map_err(crate::image::Error::from)
        .context(error::BrImageSnafu)?
    {
        return Ok(());
    }
    for arch_name_dir in crate::image::vfs_ext::list_dir_all(vfs, guest_umbrella)
        .await
        .context(error::BrImageSnafu)?
    {
        let arch_name = match arch_name_dir.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        for version_dir in crate::image::vfs_ext::list_dir_all(vfs, &arch_name_dir)
            .await
            .context(error::BrImageSnafu)?
        {
            let version = match version_dir.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            let dst = root_mount_path.join(format!("local/guest-images/{arch_name}/{version}"));
            crate::image::guest::copy_guest_artifacts(vfs, &version_dir, &dst)
                .await
                .context(error::BrImageSnafu)?;
        }
    }
    Ok(())
}
