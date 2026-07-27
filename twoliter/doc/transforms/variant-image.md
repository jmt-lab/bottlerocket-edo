# Transform: `variant-image`

Build a Bottlerocket variant image: bootable disk artifacts (`os.img.lz4`,
`data.img.lz4`, `boot.ext4.lz4`, `root.ext4.lz4`, `verity.lz4`) plus the
associated boot config, SBOM, and manifest. This is the largest new transform;
it is what `cargo make build-variant` used to drive.

**All image-building logic is Rust, in the transform crate + `br-image`.**
There is no `rpm2img` script, no `partyplanner`, no `imghelper`. See
[`../image-building.md`](../image-building.md) for the shared library.

## TOML surface

```toml
# variants/aws-k8s-1.29/variant.toml
schema-version = "3"

[variant]
name = "aws-k8s-1.29"
version = "0.1.0"
arch = "x86_64"            # optional; scheduler default is host arch

image-format = "raw"       # raw|vmdk|qcow2
partition-plan = "split"   # split|unified
os-image-size-gib = 2
data-image-size-gib = 1
os-image-publish-size-gib = 8
data-image-publish-size-gib = 20

kernel-parameters = ["quiet", "console=ttyS0"]

[variant.image-features]
grub-set-private-var = true
systemd-networkd = true
uefi-secure-boot = true
xfs-data-partition = true
in-place-updates = false
encrypted-storage = false
erofs-root-partition = false
first-party-stack = true

# Local packages to bundle in addition to what the kits provide.
[variant.included-packages]
packages = ["pkg-e"]

# Kits (references in the shared vendor namespace).
[[variant.kit]]
name = "extra-3-kit"
vendor = "my-vendor"
version = "0.1.0"

# Optional guest images that a host variant embeds.
[[variant.guest-image]]
name = "hello-eif"
install-path = "/usr/share/bottlerocket/enclaves"
```

Same shape as old `[package.metadata.build-variant]` in
`old-twoliter/tests/projects/local-kit/variants/hello-ootb/Cargo.toml`.

## Inputs (edo `depends`)

- `sdk` — one addr resolving to the `sdk-image` source.
- `kits` — list of addrs resolving to either `kit-image` sources or local
  `kit` transforms.
- `packages` — list of addrs resolving to local `rpm` transforms (for
  `included-packages`).
- `guest-images` — list of addrs resolving to other variant/EIF outputs,
  keyed by name; only present when `[[variant.guest-image]]` is set.

Note: **no `sources` dependency for the tools layer.** The old model
staged an embedded shell tarball into `${TWOLITER_TOOLS_DIR}`. The new
model links `br-image` and calls SDK-provided binaries (`sgdisk`,
`mkfs.ext4`, `veritysetup`, `lz4`, `dnf`, `syft`, `grub-install`) directly
from the container.

## Outputs

A single artifact, layered as:

| Layer index | Content | Media type |
|---|---|---|
| 0 | `os.img.lz4` | `application/vnd.bottlerocket.image.os.lz4` |
| 1 (opt) | `data.img.lz4` | `application/vnd.bottlerocket.image.data.lz4` |
| 2 | `boot.ext4.lz4` | `application/vnd.bottlerocket.image.boot.ext4.lz4` |
| 3 | `root.ext4.lz4` | `application/vnd.bottlerocket.image.root.ext4.lz4` |
| 4 | `verity.lz4` | `application/vnd.bottlerocket.image.verity.lz4` |
| 5 | `boot-config` (raw) | `application/vnd.bottlerocket.image.bootconfig` |
| 6 | `manifest.json` (variant manifest) | `application/vnd.bottlerocket.image.manifest+json` |
| 7 | `sbom.spdx.json` | `application/spdx+json` |

`manifest.json` is the "variant manifest" that `twoliter publish ami` /
`twoliter publish repo` consume. Its schema is defined by
`br_image::metadata::VariantManifest` (ported from
`old-twoliter/tools/buildsys/src/manifest.rs::ManifestInfo`).

## Environment

- **Farm**: `//container` (kind `container`).
- **Base image**: the SDK, materialized from the `sdk-image` source.
- **Overlays applied in order**:
  1. All RPMs from the depended-upon kits + local packages, unpacked into
     `/local/rpms/` (a local dnf repo the container installs from).
  2. Any guest-image artifacts, unpacked into
     `/local/guest-images/<arch>-<name>/<version>/`.

Everything else (partition tables, dm-verity, grub install, lz4 compression)
is subprocess calls made from the Rust transform body — no shell scripts.

## Algorithm

The transform body reads more or less like a straight port of `rpm2img`,
except every step is a typed function call rather than a shell block. All
primitives live in `br-image`; see
[`../image-building.md`](../image-building.md) for the module breakdown.

`Transform::transform` (in `src/transform/variant.rs`) roughly:

```rust
async fn transform(&self, ctx: Context, log: Log, storage: Storage) -> TransformStatus {
    let opts   = self.opts.clone();
    let arch   = opts.arch;
    let layout = br_image::layout::resolved_image_layout(
        &opts.image_layout, &opts.image_features, opts.image_format)?;
    br_image::layout::validate_image_features(
        &opts.image_features, &layout, opts.image_format)?;

    // 1. Stage RPMs + guest images from cached edo layers into the
    //    container's `/local/*` overlay. All storage-level reads happen up
    //    front so the rest is pure filesystem manipulation.
    let staged = self.stage_inputs(&storage).await?;

    // 2. Enter container and run the build. We use a single ContainerScript
    //    Environment that receives the staged tree and invokes our own
    //    Rust code as its entrypoint (the twoliter binary itself, invoked
    //    inside the container).
    let env = ctx.get_environment(&self.farm).await?;

    // The actual work is performed by an internal command implemented in
    // `crates/twoliter/src/internal/build_variant.rs`, which the twoliter
    // binary exposes as `twoliter internal build-variant <input-dir> ...`.
    // Inside the container, this internal command:
    //
    //   a. compute_partition_layout(&layout, &features)   [partyplanner]
    //   b. truncate + sgdisk to build the OS disk         [partyplanner]
    //   c. mkfs on each partition                         [imghelper]
    //   d. stage_root(package_dir, root_mount)            [imghelper]
    //         - dnf install --installroot=<root> …
    //         - install CA certs
    //         - install tuf root.json
    //         - SELinux relabel
    //   e. copy_guest_artifacts(&guests, root_mount)      [guest-images-helper]
    //   f. generate_verity_root(root_image, verity_image) [imghelper]
    //   g. write boot-config with the verity params       [bootconfig]
    //   h. Grub::install + optional secure-boot signing    [imghelper]
    //   i. compress every artifact per format               [imghelper]
    //   j. generate_sbom(sbom_dir, output_dir)             [imghelper]
    //   k. write variant manifest.json                      [metadata]
    //   l. optional generate_ova(os_image, ovf_template)   [imghelper]

    env.run(build_variant_cmd(&staged)).await?;

    // 3. Collect outputs from the staging area and stage them as OCI layers.
    self.collect_outputs(&staged, &storage).await
}
```

### Why "twoliter internal build-variant" runs inside the container

Two options for driving Rust code from inside the container:

1. **Ship `br-image` as a standalone binary.** Extra artifact to build and
   distribute; extra thing that can version-skew from the twoliter binary.
2. **Have the twoliter binary invoke itself in the container** via an
   internal (hidden) subcommand `twoliter internal build-variant …`. The
   twoliter binary is already staged into the container (as an overlay
   layer, one layer per transform run — it's small enough).

Recommendation: **option 2**. The twoliter binary is a single artifact; the
container invocation is a documented internal subcommand that's stable
across the twoliter release. See [`internal-subcommands.md`](../cli/internal-subcommands.md)
for the surface.

The internal subcommand's arguments encode everything old `rpm2img` took as
flags: `--package-dir`, `--output-dir`, `--partition-plan`, feature flags,
etc. Not user-facing.

## Unique-id

```
sha256(
    "variant-image",
    node.arch,
    resolved_sdk_digest,
    for kit in kits.sorted_by_addr:
        kit.artifact_id,
    for pkg in packages.sorted_by_addr:
        pkg.artifact_id,
    for guest in guest_images.sorted_by_addr:
        guest.artifact_id,
    canonical_json(image_features),
    canonical_json(resolved_layout),
    image_format,
    kernel_parameters (in order),
    twoliter_binary_digest,        // because br-image is inside the binary
)
```

`twoliter_binary_digest` matters because the entire image-building logic is
in the twoliter binary. A twoliter release that changes `br-image` invalidates
every variant's cache — which is correct.

## Old-code references

- `old-twoliter/tools/buildsys/src/main.rs::build_variant` — the CLI entry.
- `old-twoliter/tools/buildsys/src/builder.rs::DockerBuild::new_variant` —
  the staging logic; collapses into `self.stage_inputs()`.
- `old-twoliter/tools/buildsys/src/manifest.rs` — parses variant metadata.
  The types (`ManifestInfo`, `BuildVariant`, `ImageLayout`, `ImageFeature`,
  `PartitionPlan`, `SupportedArch`, `ImageFormat`, `resolved_image_layout`,
  `validate_image_features`) move to `crates/br-image/src/model.rs` and
  `layout.rs`.
- `old-twoliter/twoliter/embedded/rpm2img` — the script whose logic is being
  ported. Read it side-by-side with the new `variant.rs`.
- `old-twoliter/twoliter/embedded/partyplanner`, `imghelper`,
  `guest-images-helper` — the helpers whose logic is being ported to
  `br-image` (see [`../image-building.md`](../image-building.md)).
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.build-variant]`.

## Open questions

- **Multi-arch.** One node per (variant, arch)? Or one node per variant with
  arch as a runtime argument? Recommendation: one node per variant, with
  `arch` as an element attribute — mirrors old twoliter's
  `BUILDSYS_ARCH=<arch> cargo make build-variant`.
- **The "internal subcommand" pattern.** Alternative: `br-image` is a
  library, and we build a thin `br-image-cli` binary that lives inside the
  container as part of the SDK. That's cleaner but introduces a new SDK
  dependency. Stick with the "twoliter invokes itself" pattern for now.
- **SELinux relabel** used to happen in `stage_images` inside the container
  because `setfiles` needs the target's policy files. It stays inside the
  container in the new model too; `br-image::selinux::relabel(root_mount)`
  shells out to `setfiles` there.
