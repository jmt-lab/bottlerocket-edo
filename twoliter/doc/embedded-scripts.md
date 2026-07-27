# Embedded scripts — port to Rust

`old-twoliter/twoliter/embedded/` is ~3400 lines of bash across nine files.
Old twoliter compiled all of it into its binary and unpacked it on every
invocation. New twoliter **deletes the whole tree**: every shell script that
does real work becomes a Rust module owned by the transform that used to
invoke it. There is no runtime unpacking, no `TWOLITER_TOOLS_DIR`, no
`PATH="${TWOLITER_TOOLS_DIR}:${PATH}"` prelude.

## Why re-port

- Correctness: the scripts pipe together `dd`, `sgdisk`, `mkfs.ext4`,
  `veritysetup`, `lz4`, `mksquashfs`, `sfdisk`. Every one of those is a
  subprocess, which means every failure is a subprocess-exit-code failure
  with the real error one indirection away. Rust code calling the same
  binaries via `tokio::process::Command` centralizes error handling in a
  single `Error` enum and gives us structured logs.
- Debuggability: the shell scripts are already sourced by more than one
  driver (`rpm2img` and `img2img` both source `partyplanner` and `imghelper`).
  In Rust, the shared primitives become `pub fn`s in a shared crate and the
  callers just link.
- Testability: a `#[cfg(test)] mod tests` covering "given this partition
  layout, produce this GPT" is faster and more thorough than any
  shellcheck-plus-fixture harness.
- One binary: no runtime tarball, no version skew between the twoliter
  binary and the embedded scripts.

## Fate per file

| File | LOC | Fate |
|---|---:|---|
| `Makefile.toml` | ~1500 | **Deleted.** Replaced by twoliter subcommands + edo DAG. |
| `build.Dockerfile` | ~200 | **Deleted.** Each transform declares its container overlay. |
| `build.Dockerfile.dockerignore` | ~40 | **Deleted.** |
| `builder-group.spec` | ~10 | **Kept as `include_str!` constant** in `src/transform/package.rs` (already the case today). |
| `metadata.spec` | ~30 | **Kept as `include_str!` constant** in the `variant-image` transform module. |
| `partyplanner` | 587 | **Port to Rust** as `crates/br-image/src/partition.rs`. |
| `imghelper` | 935 | **Port to Rust** as `crates/br-image/src/{image,verity,compress,sbom}.rs`. |
| `guest-images-helper` | 44 | **Port to Rust** as `crates/br-image/src/guest.rs`. |
| `rpm2img` | 747 | **Port to Rust** into the `variant-image` transform (`src/transform/variant.rs`). |
| `rpm2eif` | 569 | **Port to Rust** into the `eif` transform (`src/transform/eif.rs`). |
| `rpm2kit` | ~180 | **Superseded** — logic is already in `src/transform/kit.rs`. |
| `rpm2kmodkit` | 57 | **Port to Rust** into the `kmodkit` transform (`src/transform/kmodkit.rs`). |
| `rpm2migrations` | 56 | **Port to Rust** into the `migrations` transform (`src/transform/migrations.rs`). |
| `img2img` | 426 | **Port to Rust** into the `repack-variant` transform (`src/transform/repack.rs`). |
| `ocihelper` | 13 | **Deleted.** Replaced by `ocilot` in the Rust kit/kmodkit transforms. |
| `docker-cargo` | ~80 | **Deleted.** Replaced by edo's `cargo-vendor` transform. |
| `docker-go` | ~80 | **Deleted.** Replaced by edo's `go-vendor` transform. |
| `waves/*.toml` | ~2500 | **Kept as data.** Moved to `crates/twoliter/data/waves/`. |
| `tests/test_*.sh` | ~400 | **Deleted.** Replaced by Rust unit + integration tests in the ported crates. |

## The shared crate: `crates/br-image/`

Everything shared between the variant-image, eif, and repack-variant transforms
lands in a new `crates/br-image/` (Bottlerocket image) crate. This is what
old `partyplanner` + `imghelper` + `guest-images-helper` used to provide
across three script drivers.

Rough module layout (see [`image-building.md`](image-building.md) for the full
spec):

```
crates/br-image/
├── src/
│   ├── lib.rs               # re-exports; error type
│   ├── partition.rs         # GPT layouts, partition GUIDs, sfdisk driver
│   ├── image.rs             # dd/mkfs helpers, tight-fit vs padded sizing
│   ├── verity.rs            # dm-verity computation via `veritysetup`
│   ├── compress.rs          # lz4/gzip driver; suffix allowlist
│   ├── rpm_install.rs       # dnf install from a local repo dir
│   ├── grub.rs              # grub install + config generation
│   ├── sbeep.rs             # secure-boot key/cert wiring
│   ├── sbom.rs              # syft driver
│   ├── ovf.rs               # OVF envelope generation
│   ├── guest.rs             # copy guest-image artifacts into a rootfs
│   ├── bootconfig.rs        # boot-config fragment parser/emitter
│   └── model.rs             # shared types (Arch, ImageFormat, PartitionPlan…)
└── Cargo.toml
```

Consumers:

- `src/transform/variant.rs` (variant-image transform) — the primary caller.
- `src/transform/eif.rs` — reuses `image`, `verity`, `partition::eif_layout`.
- `src/transform/repack.rs` — reuses `image`, `verity`, `partition`.
- `src/transform/kmodkit.rs` and `src/transform/migrations.rs` — reuse
  `rpm_install` and any packaging primitives.

Every subprocess (`sfdisk`, `mkfs.ext4`, `veritysetup`, `lz4`, `syft`,
`grub-install`, `dnf`, `rpm`) is invoked via `tokio::process::Command` from
a single `subprocess.rs` module inside `br-image` that:

- Captures stdout+stderr into the edo `Log`.
- Returns a typed `SubprocessError` with the command line, exit status, and
  captured stderr tail.
- Sets `LC_ALL=C.UTF-8` and a scrubbed env by default.

## Environment: where the tools live

The scripts assumed `${TWOLITER_TOOLS_DIR}:${PATH}` — a directory unpacked at
runtime that contained the SDK-shipped binaries plus the embedded scripts.
New twoliter doesn't stage a tools dir; instead:

- Every subprocess `br-image` invokes is expected on the container's `PATH`,
  which comes from the SDK image (the base of the container environment).
  This is already the case: `sgdisk`, `mkfs.ext4`, `veritysetup`, `lz4`,
  `dnf`, `syft`, `grub-install`, `mksquashfs`, `createrepo_c` are all in
  the Bottlerocket SDK.
- Anything **not** already in the SDK image but needed by the port
  (unlikely, but possible for e.g. a new SBOM tool) becomes an explicit
  SDK-image requirement, tracked in the SDK's own release notes.

## Assets that stay embedded

A handful of small text files were static assets rather than logic:

- `builder-group.spec` — the RPM spec that installs the `builder` group;
  already `include_str!`ed into `src/transform/package.rs`.
- `metadata.spec` — the RPM spec that carries variant metadata; will be
  `include_str!`ed into `src/transform/variant.rs`.
- `waves/*.toml` — update-wave policies consumed by `twoliter publish repo`.
  Moved to `crates/twoliter/data/waves/` and loaded as data files (either
  via `include_str!` or `RustEmbed`, whichever we prefer).

## What we lose vs the old model

- **Shell familiarity.** A Bottlerocket engineer poking at a build failure
  used to be able to open `rpm2img` and read straight through. Now they
  have to open `src/transform/variant.rs`. Trade-off: the Rust version has
  compile-time checks, structured errors, and unit tests, which the shell
  version does not.
- **Late-binding customization.** Because the scripts were on `PATH`, an
  operator could shim `rpm2img` in-place for one-off debugging. New twoliter
  is a single binary; the same debugging workflow means running the
  transform with `--shell-on-failure` (edo already exposes this) and
  inspecting the staged environment interactively.

## Old-code reference

- `old-twoliter/twoliter/embedded/` — the whole tree.
- `old-twoliter/twoliter/src/tools.rs` — the tarball unpacker that goes
  away entirely.
- `old-twoliter/twoliter/src/tool-crates/embedded-bundle/build.rs` — the
  build.rs that produced the tarball; also deleted.
