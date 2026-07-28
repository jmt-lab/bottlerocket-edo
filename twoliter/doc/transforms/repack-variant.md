# Transform: `repack-variant`

Take an already-built variant image and repack it — replace the SBOM, swap in a
new set of external kits, resize partitions, or otherwise rewrite the on-disk
layout without redoing package builds. Replaces the `img2img` embedded script
(426 lines) with a Rust transform that links `br-image`.

## TOML surface

```toml
# variants/aws-k8s-1.29/repack.toml
schema-version = "3"

[repack]
name = "aws-k8s-1.29-repack"
version = "0.1.1"
arch = "x86_64"

# The variant-image we are repacking.
source-variant = "//variants/aws-k8s-1.29"

# Additional or replacement kits.
[[repack.kit]]
name = "hotfix-kit"
vendor = "my-vendor"
version = "0.1.5"

# Overrides for partition sizes.
data-image-publish-size-gib = 40
```

## Inputs

- `source-variant` — a `variant-image` transform's artifact.
- `kits` — additional/replacement kits (either sources or transforms).

No tools-layer dependency.

## Outputs

Same layer shape as `variant-image`, but marked with a
`bottlerocket.repack-of` annotation on the OCI manifest pointing at the source
variant's OCI digest.

## Environment

Container farm, base = SDK.

## Algorithm

Ports the `img2img` script into `src/transform/repack.rs`. The heavy lifting
comes from `br-image`; the transform mostly orchestrates.

```rust
async fn transform(&self, log: &Log, ctx: &Handle, env: &Environment) -> TransformStatus {
    let id = self.get_unique_id(ctx).await?;
    let vfs = Vfs::new(&id, env, log).await?;

    // 1. Materialize the source variant's layers into the Vfs.
    // Contains: os.img.lz4, boot.ext4.lz4, root.ext4.lz4, verity.lz4,
    // boot-config, manifest.json, sbom.spdx.json.
    self.stage_source(&vfs, ctx).await?;

    // 2. Stage the replacement/additional kits' RPMs.
    self.stage_extra_rpms(&vfs, ctx).await?;

    // 3. Repack. Every step below is a call into `br-image`; every
    //    subprocess is a `vfs.command(...)` that edo dispatches into the
    //    container.
    //    a. Decompress the OS image.
    //    b. Loop-mount ROOT-A.
    //    c. dnf --installroot=<root> install <extra RPMs>
    //       + dnf remove <packages removed by replacement kits>
    //    d. debugfs-based single-file replace for anything that can't be
    //       re-`dnf`-ed (e.g. an updated /etc file). Ports
    //       img2img::debugfs_replace via br_image::filesystem::debugfs_replace.
    //    e. Regenerate SBOM.
    //    f. Regenerate dm-verity hash tree (br_image::verity).
    //    g. Update the boot-config with the new verity hash.
    //    h. Re-compress everything.
    //    i. Update variant manifest.json.
    self.build(&vfs, ...).await?;

    // 4. Collect outputs, marking the OCI manifest with the annotation
    //    bottlerocket.repack-of=<source-variant-digest>.
    self.collect_outputs(&vfs, ctx).await
}
```

Ported `br-image` primitives:
- `img2img::debugfs_replace` → `br_image::filesystem::debugfs_replace(image,
  path_in_image, new_bytes)`
- `img2img::write_partition` → `br_image::partition::write_partition_bytes(
  disk_image, partition, bytes)`
- `img2img::cleanup` → RAII via `Drop`s in the Rust code (temp dirs, loop
  devices, mount points).

The dnf install / remove logic is shared with `variant-image` (both call
`br_image::rpm_install::install_to_root`).

## Unique-id

```
sha256(
    "repack-variant",
    source_variant.artifact_id,
    for kit in kits.sorted_by_addr: kit.artifact_id,
    canonical_json(resized_partitions_config),
    twoliter_binary_digest,
)
```

## Old-code references

- `old-twoliter/twoliter/embedded/img2img` — 426 lines of shell; full port
  target.
- `old-twoliter/tools/buildsys/src/builder.rs::DockerBuild::repack_variant`
  — the builder branch that invoked `img2img`.
- `old-twoliter/tools/buildsys/src/main.rs::repack_variant` — the CLI entry.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.repack-variant]`.

## Open questions

- The old repack skipped kit-lockfile verification. In new twoliter, kit
  verification is a `Source` concern; `repack-variant` naturally inherits
  whatever validation its `kits` inputs perform. No special case needed.
- `debugfs_replace` requires the source file to be present with the exact
  same size unless the ext4 image has been resized. Do we ever need to
  grow the ext4 image during repack? Old `img2img` did — it resized ROOT-A
  after installs. Yes, port that path too (`br_image::filesystem::grow_ext4`).
- Loop-device management is finicky (races on stale loop devices, cleanup
  on error). Wrap in a `LoopDevice` RAII type that ensures `losetup -d` on
  drop, even on panic.
