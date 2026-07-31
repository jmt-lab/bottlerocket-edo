# Implementation roadmap

Concrete ordering for turning the specs in this directory into working code.
Every phase is independently useful — a partial port stays functional for the
subset of Bottlerocket workflows already covered.

## Phase 0 — done

- `rpm` transform (`src/transform/package.rs`).
- `kit` transform (`src/transform/kit.rs`).
- Basic project loader (`src/config/`) — `Twoliter.toml`, `package.toml`,
  `kit.toml` minimal cases.
- CLI scaffolding (`src/cmd/`) — `build`, `run`, `checkout`, `update`,
  `list`, `prune`, `publish`.

## Phase 1 — sources and lockfile

Goal: a clean project can `twoliter update`, `twoliter fetch`, and `twoliter
build package` end-to-end without shelling out to the old tool.

- [`sources/kit-image`](sources/kit-image.md) — kit source with metadata check.
- [`sources/sdk-image`](sources/sdk-image.md) — schema fix: point the SDK
  requirement at edo-core's built-in `image` source and `image` vendor
  (no new twoliter source kind).
- [`sources/lookaside`](sources/lookaside.md) — RPM external files.
- [`cli/update`](cli/update.md) — wire to edo's lock refresh.
- [`cli/fetch`](cli/fetch.md) — cache warm-up.
- [`config/overrides`](config/overrides.md) — override overlays.
- Extend [`config/twoliter-toml`](config/twoliter-toml.md) to handle
  `[[kit]]`, `[[farm]]`, `[lookaside]`.
- Extend [`config/package-toml`](config/package-toml.md) to emit `lookaside`
  source nodes from `[[package.external-files]]`.

Milestone: `twoliter build package kernel-6.1 --arch x86_64` on a fresh
checkout builds the RPM.

## Phase 2 — `br-image` and variant-image

Goal: variant image builds, natively in Rust. Biggest single lift in the
whole port.

- [`image-building`](image-building.md) — bring up `crates/br-image/`.
  Ports ~1500 LOC of `partyplanner` + `imghelper` +
  `guest-images-helper` and every subprocess wrapper the image build needs.
  Splits into `partition.rs`, `image.rs`, `verity.rs`, `compress.rs`,
  `rpm_install.rs`, `grub.rs`, `securebooot.rs`, `ovf.rs`, `sbom.rs`,
  `guest.rs`, `bootconfig.rs`, `selinux.rs`, `layout.rs`, `model.rs`.
- Move buildsys manifest types (`ImageLayout`, `ImageFeature`,
  `PartitionPlan`, `SupportedArch`, `ImageFormat`, `resolved_image_layout`,
  `validate_image_features`) from
  `old-twoliter/tools/buildsys/src/manifest.rs` into
  `crates/br-image/src/{model,layout}.rs`.
- [`transforms/variant-image`](transforms/variant-image.md) — the transform
  itself. Ports the ~747-line `rpm2img` driver into
  `src/transform/variant.rs`. The transform body drives the container
  through its `edo::environment::Vfs`, the same pattern `package.rs` and
  `kit.rs` already use.
- [`config/variant-toml`](config/variant-toml.md) — `variant.toml` schema.
- [`cli/build`](cli/build.md) `twoliter build variant` subcommand.

Milestone: `twoliter build variant aws-k8s-1.29` produces the same
`os.img.lz4`, `data.img.lz4`, etc. that old twoliter's `build-variant`
produces — with zero embedded shell scripts on the runtime path.

## Phase 3 — publishing minimum viable

Goal: after a variant build, produce AMIs and a signed TUF repo.

- [`publish/publish-kit`](publish/publish-kit.md) — flesh out the existing stub.
- [`publish/ami`](publish/ami.md) — port `tools/pubsys/src/aws/ami/`.
- [`publish/repo`](publish/repo.md) — port `tools/pubsys/src/repo.rs`.
- [`publish/setup-tuf`](publish/setup-tuf.md) — port `tools/pubsys-setup/`.
- [`publish/infra-toml`](publish/infra-toml.md) — port
  `tools/pubsys-config/` to `crates/pubsys-config/`.
- Move `update-metadata`, `amispec`, `parse-datetime`,
  `bottlerocket-variant`, `error-utils` from `old-twoliter/tools/` to
  `crates/`.

Milestone: `twoliter publish repo aws-k8s-1.29` uploads a signed TUF repo,
and `twoliter publish ami aws-k8s-1.29` registers AMIs.

## Phase 4 — auxiliary transforms

Goal: parity for less-common but still shipping build outputs. Every one of
these leans on `crates/br-image/` from Phase 2.

- [`transforms/eif`](transforms/eif.md) — `eif-builder` becomes
  `crates/eif-builder/` (already close to a library). Port ~569-line
  `rpm2eif` into `src/transform/eif.rs` + `br_image::eif::extract_vmlinux`.
- [`transforms/kmodkit`](transforms/kmodkit.md) — port the 57-line
  `rpm2kmodkit` into `src/transform/kmodkit.rs`. Extract the OCI writer
  from `src/transform/kit.rs` into `src/transform/oci_pack.rs` first.
- [`transforms/migrations`](transforms/migrations.md) — port the 56-line
  `rpm2migrations` into `src/transform/migrations.rs`. Local farm, no
  container needed.
- [`transforms/repack-variant`](transforms/repack-variant.md) — port the
  426-line `img2img` into `src/transform/repack.rs` +
  `br_image::filesystem::{debugfs_replace,grow_ext4,write_partition_bytes}`.
- [`transforms/advisory-check`](transforms/advisory-check.md) — port
  `advisory-checker` binary.
- [`transforms/kit-metadata`](transforms/kit-metadata.md) — refactor if the
  `kit` and `kmodkit` transforms both need it.

Milestone: every variant declared in a Bottlerocket-scale project builds
and publishes, with no shell scripts on the runtime path.

## Phase 5 — publishing extras and inspection

Goal: parity for the long tail of pubsys subcommands.

- [`publish/ssm`](publish/ssm.md) — port `tools/pubsys/src/aws/ssm/`,
  `promote_ssm/`, `validate_ssm/`.
- [`publish/ova`](publish/ova.md) — port `tools/pubsys/src/vmware/`.
- `publish/ami` grant/revoke/validate/public sub-subcommands.
- [`cli/pcrsys`](cli/pcrsys.md) — port `tools/pcrsys/` to
  `crates/pcrsys/` and wire the twoliter subcommand.
- [`cli/check`](cli/check.md) — the whole `twoliter check *` family.
- [`cli/debug`](cli/debug.md) — inspection subcommands.

Milestone: no meaningful cargo-make target is left un-ported. Everything an
operator runs today has a twoliter equivalent.

## Phase 6 — cleanup

- Delete `old-twoliter/` (or move it to an archive branch).
- Delete `unplug`, `pipesys`, `krane`, `oci-cli-wrapper`, `testsys`,
  `testsys-config`, `docker-go`, `docker-cargo`, `ocihelper`, `rpm2kit`,
  `build.Dockerfile*`, `Makefile.toml` from the tree.
- Move surviving `tools/*` crates to `crates/*` with the same names.
- Update integration tests under `tests/` to run against new twoliter.

## What lives outside twoliter forever

Bottlerocket-project-side, unchanged by this port:

- The `.spec` files that describe each RPM.
- The `sources/` tree of first-party Rust and Go code.
- The Bottlerocket variant `Cargo.toml`s (kept, just not as
  buildsys-metadata carriers).

None of that logic moves; only how twoliter reads it does.

## Non-goals for the port

Restated from [`README.md`](README.md):

- Bit-for-bit reproducing old twoliter's `build/` on-disk layout.
- `cargo make` compatibility.
- Auto-migrating v1/v2 `Twoliter.toml` files.
- testsys.
