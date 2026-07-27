# `crates/br-image/` — the shared image-building library

`br-image` is where the primitives that used to live in `partyplanner` +
`imghelper` + `guest-images-helper` land, plus every subprocess wrapper for
tools we invoke to build a Bottlerocket disk image. It is depended on by the
`variant-image`, `eif`, `kmodkit`, `migrations`, and `repack-variant`
transforms.

It is **not an edo plugin**. It is a plain Rust library. Transforms link
against it and call into it during `Transform::transform`. Nothing here talks
to edo directly — it operates on filesystem paths and returns `Result`s so
the transforms can wrap failures into `TransformStatus::Failed`.

## Module layout

```
crates/br-image/
├── src/
│   ├── lib.rs            # re-exports; top-level `Error`, `Result`
│   ├── error.rs          # snafu-based Error covering every subprocess & IO fail
│   ├── subprocess.rs     # `run(cmd, args, env) -> Result<Output>`; log capture
│   ├── model.rs          # Arch, ImageFormat, PartitionPlan, ImageFeatures,
│   │                     # ImageLayout, ResolvedLayout (ported from
│   │                     # old-twoliter/tools/buildsys/src/manifest.rs)
│   ├── partition.rs      # port of `partyplanner`
│   ├── image.rs          # port of `imghelper`'s image staging + dd/truncate/mkfs
│   ├── verity.rs         # port of `imghelper::generate_verity_root`
│   ├── compress.rs       # port of `imghelper::compress_image`, `decompress_image`
│   ├── rpm_install.rs    # dnf-based rootfs install (was inlined in rpm2img)
│   ├── grub.rs           # grub2 install + config, sbsetup, sign_shim, sign_grub,
│   │                     # unembed_grub, sign_grubcfg, sign_mokm, sign_vmlinuz
│   ├── securebooot.rs    # sbsetup_wrapup, sbsetup_aws_profile,
│   │                     # sbsetup_local_profile, provide_certs, cert_table
│   ├── ovf.rs            # port of `imghelper::generate_ova`
│   ├── sbom.rs           # syft driver + SBOM staging (`imghelper` had this
│   │                     # inlined too)
│   ├── guest.rs          # port of `guest-images-helper::copy_guest_image_artifacts`
│   ├── bootconfig.rs     # parse + emit boot-config
│   ├── selinux.rs        # SELinux file-context labeling
│   ├── metadata.rs       # variant-manifest emission (was `metadata.spec`
│   │                     # macro-expanded in rpm2img)
│   └── layout.rs         # port of `manifest::resolved_image_layout` +
│                         # `manifest::validate_image_features`
├── data/
│   ├── builder-group.spec   # moved from twoliter/embedded/, referenced via
│   │                        # `include_str!`
│   └── metadata.spec
└── Cargo.toml
```

## Key types (`model.rs`)

Copied straight from `old-twoliter/tools/buildsys/src/manifest.rs` and made
public. These are the same enums the transforms already reference through
`config/`:

```rust
pub enum Arch { X86_64, Aarch64 }              // manifest::SupportedArch
pub enum ImageFormat { Raw, Qcow2, Vmdk }      // manifest::ImageFormat
pub enum PartitionPlan { Split, Unified }      // manifest::PartitionPlan

pub struct ImageFeatures {                     // manifest::ImageFeature (bitset)
    pub grub_set_private_var: bool,
    pub systemd_networkd: bool,
    pub uefi_secure_boot: bool,
    pub xfs_data_partition: bool,
    pub in_place_updates: bool,
    pub encrypted_storage: bool,
    pub erofs_root_partition: bool,
    pub first_party_stack: bool,
}

pub struct ImageLayout {                       // manifest::ImageLayout
    pub os_image_size_gib: u16,
    pub data_image_size_gib: u16,
    pub os_image_publish_size_gib: Option<u16>,
    pub data_image_publish_size_gib: Option<u16>,
    pub partition_plan: PartitionPlan,
}

pub struct ResolvedLayout(pub ImageLayout);    // manifest::resolved_image_layout
```

## Subprocess policy (`subprocess.rs`)

Everything shell used to shell out to lands here as a typed call. Rules:

- No `sh -c`. Every subprocess is `Command::new("sfdisk").args(&[…])`.
- Stdout + stderr captured to the edo `Log` via a shared writer. The
  transform passes its `Log` to `br-image`; `br-image` writes structured
  entries labeled by subprocess.
- `env_clear()`, then set a minimal env: `PATH` (from SDK image), `LC_ALL=C.UTF-8`,
  `TZ=UTC`, plus whatever the tool documents it needs (`SOURCE_DATE_EPOCH`
  for reproducibility).
- Non-zero exit → `Error::Subprocess { cmd, exit_code, stderr_tail }`. Never
  swallow.

Tools we call: `sgdisk`, `sfdisk`, `dd`, `truncate`, `mkfs.ext4`, `mkfs.xfs`,
`mkfs.erofs`, `mkfs.vfat`, `veritysetup`, `lz4`, `gzip`, `dnf`, `rpm`,
`createrepo_c`, `mksquashfs`, `grub-install`, `grub-bios-setup`,
`grub-mkimage`, `mokutil`, `sbsign`, `openssl`, `syft`, `mount`, `umount`,
`losetup`, `debugfs`, `xxd`, `mkbootimg` (aarch64), `mkimage` (u-boot),
`ovftool`. Each gets a thin wrapper.

Not every wrapper is one call; e.g. `verity.rs::generate_verity_root` is a
sequence:

1. Compute dm-verity hash tree with `veritysetup format`.
2. Read root hash and salt from `veritysetup dump`.
3. Assemble the verity image (raw hash tree + trailing metadata).

## Ported behaviors

Below, each source file's functions map to their new home. Names are kept
close to the shell so grepping the old code stays productive.

### `partyplanner` → `partition.rs`

Old shell functions → new pub fn:

- `get_partition_sizes(plan, first_party_stack) -> PartitionSizeMap`
- `set_partition_sizes(os_gib, data_gib, plan, in_place_updates, first_party_stack)`
  → `compute_partition_layout(&ImageLayout, &ImageFeatures) -> PartitionLayout`
- `set_partition_labels()` → constants + `partition_label(part) -> &'static str`
- `set_partition_types()` → constants + `partition_typecode(part) -> Guid`
- `set_partition_uuids(plan)` → deterministic UUID derivation
- `set_eif_partition_sizes(...)` → separate `eif_partition_layout()` for EIF's
  ROOT-A + HASH-A only case

Constants (partition type GUIDs) stay as `pub const`s. See the top of
`old-twoliter/twoliter/embedded/partyplanner` for the canonical list —
`BOTTLEROCKET_BOOT_TYPECODE`, `BOTTLEROCKET_ROOT_TYPECODE`, etc.

The `IMAGE_ARTIFACT_SUFFIXES` allowlist becomes an enum:

```rust
pub enum ArtifactSuffix {
    ImgLz4,        // ".img.lz4"
    Qcow2,         // ".qcow2"
    Vmdk,          // ".vmdk"
    Ova,           // ".ova"
    Ext4Lz4,       // ".ext4.lz4"
    VerityLz4,     // ".verity.lz4"
}
```

### `imghelper` → several modules

`imghelper` is the biggest single file. Split by concern:

**`image.rs`** — filesystem staging and image assembly:
- `sanity_checks(output_fmt, plan, ovf_template, uefi_sb, sbom_dir)` →
  `validate_build_inputs(&BuildInputs) -> Result<()>`
- `stage_images(rpm_dir, root_mount, boot_mount, private_mount, data_mount)`
  → `stage_root(package_dir, root_dir) -> Result<()>`; installs RPMs via
  `dnf install --installroot` and populates the boot/private/data mounts.
- `mkfs_data_ext4`, `mkfs_data_xfs`, `mkfs_root_erofs` → `mkfs_*` functions
  in `image.rs`.
- `check_image_size`, `get_ext4_usage`, `get_erofs_usage` → `usage.rs` helpers.
- `install_ca_certs`, `install_root_json` → `install_ca_certs(root_dir)` and
  `install_tuf_root(root_dir, path)`.

**`verity.rs`** — dm-verity:
- `generate_verity_root(root_image, verity_image) -> Result<VerityInfo>`;
  returns `{ root_hash, salt, hash_algorithm, data_block_size,
  hash_block_size, data_blocks }`. The verity image on disk is the hash
  tree; the returned struct is embedded in the boot config.

**`compress.rs`**:
- `compress_image(input, output, format)` → runs `lz4`/`gzip`/none based on
  the enum. Validates the requested extension against
  `ArtifactSuffix`.
- `decompress_image(input, output)` → detects the extension and dispatches.
- `symlink_image(latest_symlink, actual)` → `create_latest_symlink(dir,
  target)`.

**`grub.rs`** — bootloader install + optional signing:
- `sbsetup_wrapup`, `sbsetup_aws_profile`, `sbsetup_local_profile`,
  `sbsetup_signing_profile` → `Sbsetup` struct that holds the resolved key
  material + `Sbsetup::apply(root_mount)`.
- `sign_shim`, `zero_shim`, `sign_mokm`, `sign_grub`, `unembed_grub`,
  `provide_certs`, `sign_vmlinuz`, `sign_grubcfg` → `Grub::sign_*`,
  `Grub::install`, `Grub::embed_config`.
- `generate_hmac` → `hmac::generate(root_mount)`.
- `undo_sign`, `do_sign` → private helpers shared by the above.

**`securebooot.rs`** — key/cert loading:
- `cert_table(profile)` → `load_cert_table(profile) -> CertTable`.

**`ovf.rs`**:
- `generate_ova(os_image, output_dir, template)` →
  `generate_ova(os_image, output_dir, template) -> Result<PathBuf>`.

**`sbom.rs`**:
- `generate_application_inventory(sbom_dir, output_dir)` →
  `generate_sbom(sbom_dir, output_dir) -> Result<PathBuf>`. Invokes `syft`.

### `guest-images-helper` → `guest.rs`

- `copy_guest_image_artifacts(host_root, guest_images, arch, version)` →
  `copy_guest_artifacts(root_mount, &[Guest], arch, version) -> Result<()>`.
  Iterates `IMAGE_ARTIFACT_SUFFIXES` from `partition.rs` and copies matching
  files into the host rootfs at the caller-supplied install path.

### `rpm2eif` script logic → `eif.rs` (in variant transform crate)

The parts of `rpm2eif` that aren't already covered above:
- `extract_vmlinux` → `extract_vmlinux(root_mount, arch) -> PathBuf`. On
  x86_64 finds `vmlinuz` and unpacks it; on aarch64 finds the flat PE
  `Image` and strips any EFI zboot wrapper.
- The EIF assembly step — invoking `eif-builder` — stays in the transform;
  the transform links `crates/eif-builder/` directly (no subprocess).

### `img2img` → `repack.rs` (in repack transform)

Distinct enough that it doesn't share code with `rpm2img` beyond the
`br-image` primitives:
- `debugfs_replace` → `filesystem::debugfs_replace(image_path, filepath,
  new_contents)`. Uses `debugfs` to overwrite a single file inside an ext4
  image without mounting.
- `write_partition` → `partition::write_partition_bytes(disk_image, part,
  bytes)`. Uses `dd` under the hood.

### `rpm2kit` → already in `src/transform/kit.rs`

No shell logic to port here beyond what's already implemented.

### `rpm2kmodkit` → `src/transform/kmodkit.rs`

The script is small (57 lines) and mostly OCI packaging. It reuses `ocihelper`
in the shell version; new twoliter reuses the OCI writer already in
`src/transform/kit.rs`. Kmodkit-specific bits (per-kernel-version metadata)
land in `src/transform/kmodkit.rs::pack_metadata`.

### `rpm2migrations` → `src/transform/migrations.rs`

Also small (56 lines). Ports as:
- `list_migration_binaries(package_dir) -> Vec<Migration>`
- `pack_migrations(&[Migration]) -> TarballLayer`

## Error type

Single `br_image::Error` covering:

- `Io { path, source }` — filesystem operations.
- `Subprocess { cmd, exit_code, stderr_tail }` — every subprocess failure.
- `Verity { source }` — dm-verity computation failure with the underlying
  `veritysetup` output.
- `Partition { plan, feature, reason }` — invalid layout combinations
  caught before we start shelling out.
- `InvalidArtifact { path, expected_suffix }` — `ArtifactSuffix` mismatch.
- `Rpm { source }` — RPM header parse failure (from the `rpm` crate).
- `Ocilot { source }` — OCI push/pull errors (only in the paths that
  actually push).

Each transform wraps `br_image::Error` in its own local
`TransformError::Implementation(Box<br_image::Error>)`.

## Testing

- Unit tests in each module. `partition.rs` is easy: given inputs, produce
  the same `PartitionLayout` we would have produced from the shell. The
  shell version's implicit "reference outputs" become `include_str!`ed
  golden files under `crates/br-image/tests/fixtures/`.
- Integration tests under `crates/br-image/tests/` that assemble a real
  image against a minimal package set. Requires the SDK image; gate behind
  an env var (`BR_IMAGE_INTEGRATION=1`) so `cargo test -p br-image` stays
  fast by default.
- Nothing in `br-image` needs mocking; all subprocess calls hit real
  binaries. The transforms above (`variant-image` etc.) mock `br-image`
  wholesale for their own tests when needed.

## What does *not* move here

- Anything that requires the edo `Storage`/`Environment`/`Source`/`Transform`
  types. Those live in `src/transform/*.rs`. `br-image` speaks only in
  paths, byte vectors, and its own types.
- Anything specific to a single transform's staging (e.g. how a
  `variant-image` sets up the container overlay). That belongs in
  `src/transform/variant.rs`.

## Old-code references

Line counts to port (approximate):

| Source file | LOC | Target module |
|---|---:|---|
| `partyplanner` | 587 | `partition.rs`, `model.rs` |
| `imghelper` | 935 | `image.rs`, `verity.rs`, `compress.rs`, `grub.rs`, `securebooot.rs`, `ovf.rs`, `sbom.rs`, `selinux.rs` |
| `guest-images-helper` | 44 | `guest.rs` |
| `rpm2img` | 747 | *driven from `src/transform/variant.rs`*, but the primitives (partitioning, mkfs, verity, install) come from `br-image` |
| `rpm2eif` | 569 | *driven from `src/transform/eif.rs`*, using `br-image` |
| `img2img` | 426 | *driven from `src/transform/repack.rs`*, using `br-image` |
| `rpm2kmodkit` | 57 | *driven from `src/transform/kmodkit.rs`* |
| `rpm2migrations` | 56 | *driven from `src/transform/migrations.rs`* |

Total lift: ~3400 LOC of shell → ~5000-6000 LOC of Rust (rough estimate;
error paths, structured logs, and typed layout code inflate the count).

## Open questions

- Should `br-image` live inside `crates/twoliter/` as a submodule, or as a
  standalone workspace crate? Recommendation: standalone crate. It has
  self-contained value and could conceivably be reused by other
  Bottlerocket-adjacent tools. Its consumer graph is closed (only twoliter
  transforms), but that's a distribution detail, not a design one.
- Do we want a `#[cfg(feature = "veritysetup-lib")]` path that links
  libveritysetup directly instead of shelling out? Nice-to-have for the
  reproducibility work; out of scope for the initial port.
- Some of `imghelper`'s functions branch on `${UEFI_SECURE_BOOT}`, others
  on `${IN_PLACE_UPDATES}`, others on `${FIRST_PARTY_STACK}`. The Rust port
  should pass the whole `ImageFeatures` bitset into every function rather
  than global-state-y env vars, and let the compiler enforce match
  exhaustiveness on feature combinations.
