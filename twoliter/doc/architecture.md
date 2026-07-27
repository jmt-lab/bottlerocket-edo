# Architecture

New twoliter is a thin binary that:

1. Constructs an edo `Context` and registers the twoliter-specific plugins
   (transforms + sources) alongside `register_core`.
2. Loads the project by walking `Twoliter.toml` and any nested
   `package.toml` / `kit.toml` / `variant.toml`, turning each into edo
   `Node`s via `Schema`.
3. Delegates to edo's `Scheduler` for the build (`twoliter build …`,
   `twoliter run …`, `twoliter checkout …`, `twoliter prune …`, `twoliter
   update`) or drives an out-of-DAG subcommand (`twoliter publish …`,
   `twoliter pcrsys …`).

Everything expensive — hashing, caching, container-farm scheduling, DAG
execution, retry — is edo's job. Twoliter's job is:

- Project shape: a Bottlerocket build is a graph of `packages/*`, `kits/*`,
  `variants/*`, and their transitive kit/SDK dependencies. Twoliter knows the
  conventions (`packages.rs`, `kit.rs`, `variants.rs`, `Cargo.toml` metadata,
  `Twoliter.toml`).
- Bottlerocket-specific transforms: RPM builds, kit packing, variant image
  generation, EIF, PCR prediction.
- Publishing paths that are meaningful only to a Bottlerocket operator (AMI,
  SSM, TUF repo, OVA, publish-kit).

## Address plan

Edo `Addr`s follow `//<namespace>/<name>` with `/` separators. Twoliter uses:

| Addr | Kind | Owner |
|---|---|---|
| `//<sdk>` | `sdk-image` source | resolved from `Twoliter.toml` `[sdk]` |
| `//kits/<vendor>/<name>` | `kit-image` source *or* `kit` transform | if kit is external, it's a source; if it's local, it's a transform |
| `//packages/<name>` | `rpm` transform | one per `packages/<name>/package.toml` |
| `//variants/<name>` | `variant-image` transform | one per `variants/<name>/variant.toml` |
| `//variants/<name>/eif` | `eif` transform | opt-in per variant |
| `//variants/<name>/kmodkit` | `kmodkit` transform | opt-in per variant |
| `//variants/<name>/migrations` | `migrations` transform | opt-in per variant |
| `//variants/<name>/repack` | `repack-variant` transform | opt-in per variant |
| `//advisory-check` | `advisory-check` transform | one per project |
| `//edo-source-cache/kit-image` | `oci` vendor slot | edo builtin |
| `//edo-source-cache/lookaside` | reserved cache slot for RPM external files | new |

Vendors: `Twoliter.toml` `[vendor.<name>]` maps to an edo `Vendor` of kind `oci`
that owns the `registry` string. The current `src/config/twoliter.rs`
implementation already does this — the spec formalizes it.

## DAG shape (a concrete variant build)

```
sdk-image source (//bottlerocket-sdk)
  ├── used by: every //packages/<x> rpm transform (as environment base)
  └── used by: //variants/<v> variant-image transform (as base rootfs)

kit-image source (//kits/bottlerocket/core-kit)  ← external kit
  └── used by: local kit that depends on it, or variant-image directly

rpm transform (//packages/kernel-6.1)
  ├── depends on: sdk source, lookaside source, package sources
  ├── output: single-layer artifact containing *.rpm files
  └── used by: //kits/<local kit> that lists it, and //variants/<v> (via kit)

kit transform (//kits/my-project/my-kit)
  ├── depends on: all rpm transforms in the kit's included-packages set
  ├── output: OCI manifest layers = one per package + repodata + labels
  └── used by: any variant-image that declares this kit

variant-image transform (//variants/aws-k8s-1.29)
  ├── depends on: sdk, all local + external kits declared in Cargo.toml
  ├── output: os.img.lz4, data.img.lz4 (optional), boot.ext4.lz4, root.ext4.lz4,
  │           verity.lz4, boot-config, sbom, manifest.json
  └── environment: container, running rpm2img from the tools layer

eif transform (//variants/hello-eif/eif)  ← only if variant.toml enables
  ├── depends on: same rpm set + sdk
  └── output: {name}.eif, {name}-disk.img, {name}-kernel

publish is orthogonal: `twoliter publish ami …` reads an already-materialized
variant-image artifact out of edo storage and drives AWS.
```

## Cache slots and content-addressing

Edo's storage is layered: `local + source-cache(s) + build-cache? + output-cache?`.
Twoliter uses:

- **`//edo-local-cache`** (default): everything not covered below.
- **`//edo-source-cache/kit-image`** (new): OCI blobs and layers for kit and
  SDK images. Rooted per-vendor.
- **`//edo-source-cache/lookaside`** (new): RPM external files (upstream
  tarballs by SHA-512), keyed by `(name, sha512)`. The old `LookasideCache`
  fetch-and-verify logic (`old-twoliter/tools/buildsys/src/cache.rs`) lives
  here.
- **`//edo-build-cache`** (optional): reusable across CI runs. Enable via
  `[storage.build-cache]` in `Twoliter.toml`.
- **`//edo-output-cache`** (optional): distributes final artifacts back to CI.

The RPM's unique-id (input hash) is derived from:

1. Source tree under `packages/<name>/` (rerun-if-changed set).
2. The referenced SDK's OCI digest.
3. Every build-dep RPM's artifact id.
4. Every external-file's SHA-512 + upstream fallback flag.
5. `arch`, `version-image`, `version-build`.

The `kit`'s unique-id is `(vendor, name, version, {rpm-artifact-ids...})`.
The `variant-image`'s unique-id is `(sdk-digest, {included-kit-artifact-ids...},
{included-package-rpm-ids...}, image-features, image-layout,
kernel-parameters, first-party-stack)`.

This is what already lives in `src/transform/package.rs::get_unique_id` and
`src/transform/kit.rs::get_unique_id`. New transforms have to follow the same
pattern.

## Environments

Two edo `Environment` kinds cover everything:

- `local` — the host shell. Used for cheap CPU-only work: manifest parsing,
  advisory validation, PCR prediction, TUF repo signing, AWS publishing.
- `container` — a rootful container built on top of the SDK image. Used for
  every rpm/kit/variant transform. The old `build.Dockerfile` (multi-stage,
  network-controlled, seccomp-jailed via `unplug`) collapses to:
  - Base image = SDK image (from `sdk-image` source).
  - Overlay = tools layer, packaged as a `local` source (was
    `twoliter/embedded/`).
  - Optional overlay per transform = source tree, per-arch build-deps, kits.
  - Network access = controlled by edo's `container` farm settings, not by
    `unplug`. Vendored deps come from edo's `cargo-vendor` / `go-vendor`
    transforms.

Farms live at `//<farm>`. Default `//default` is `local`. A `//container` farm
of kind `container` is expected. Users can add more (arm builder pool, etc.).

## What used to be shell scripts

The `twoliter/embedded/` tree is inventoried in
[`embedded-scripts.md`](embedded-scripts.md); shared image primitives are
spec'd in [`image-building.md`](image-building.md). Summary:

- `rpm2img`, `rpm2eif`, `img2img`, `rpm2kmodkit`, `rpm2migrations`,
  `partyplanner`, `imghelper`, `guest-images-helper`: **ported to Rust**.
  Shared primitives (GPT layout, dm-verity, mkfs, lz4/gzip, dnf-install-to-
  rootfs, grub + secure-boot, SBOM, OVA) land in `crates/br-image/`. The
  driver scripts (`rpm2img`, `rpm2eif`, `img2img`, `rpm2kmodkit`,
  `rpm2migrations`) collapse into the corresponding transforms
  (`variant-image`, `eif`, `repack-variant`, `kmodkit`, `migrations`).
- `rpm2kit`: superseded by `src/transform/kit.rs` (already implemented).
- `docker-go`, `docker-cargo`: **delete**. Replace with edo's `cargo-vendor`
  and `go-vendor` transforms.
- `ocihelper`: **delete**. Replaced by the OCI writer already used by
  `src/transform/kit.rs`.
- `build.Dockerfile`, `build.Dockerfile.dockerignore`: **delete**. There is no
  monolithic Dockerfile; each transform declares its own environment overlay.
- `Makefile.toml`: **delete**. Targets become twoliter subcommands or DAG addrs.

The image-building transforms invoke their Rust bodies inside the container
via a hidden `twoliter internal …` subcommand family — see
[`cli/internal-subcommands.md`](cli/internal-subcommands.md). This means one
binary on both sides of the container boundary; no separate `br-image-cli`
artifact.

## What used to be embedded tool binaries

The old `twoliter/src/tool-crates/` tree is a set of Rust binaries stamped into
the twoliter binary and unpacked into `build/tools/` on every invocation. The
new plan is one of the following for each:

| Old tool | New home |
|---|---|
| `buildsys` | Absorbed into `rpm`, `kit`, `variant-image`, `eif`, `kmodkit`, `migrations`, `repack-variant` transforms. |
| `advisory-checker` | `advisory-check` transform + `twoliter check advisories` subcommand. |
| `eif-builder` | Called by `eif` transform; keep the `eif-builder` library crate (`old-twoliter/tools/eif-builder/`) as-is under `crates/eif-builder/`. |
| `pcrsys` | `twoliter pcrsys` subcommand (also usable standalone). No transform: it operates on already-built artifacts on demand. |
| `pubsys` | `twoliter publish …` subcommands, one per pubsys subcommand. |
| `pubsys-setup` | `twoliter publish setup-tuf` subcommand. |
| `tuftool` | External dependency, invoked from `twoliter publish repo`. |
| `krane` | Delete. Edo's `oci` vendor uses `ocilot` which already handles this. |
| `oci-cli-wrapper` | Delete. Replaced by `ocilot`. |
| `unplug` | Delete. Handled by container farm network policy. |
| `pipesys` | Delete. Handled by edo's storage/environment layering. |
| `testsys` | Delete (deprecated). |
| `update-metadata` | Move to `crates/update-metadata/` — still needed by `twoliter publish repo` to write `manifest.json`. |
| `parse-datetime` | Move to `crates/parse-datetime/` — needed by repo expiration parsing. |
| `bottlerocket-variant` | Move to `crates/bottlerocket-variant/` — still needed to parse variant strings in publishing. |
| `buildsys-config`, `pubsys-config`, `testsys-config` | Absorbed into `crates/twoliter/src/config/` (twoliter's own config module). testsys-config is deleted. |
| `error-utils` | Optionally kept as `crates/error-utils/` if publishing subcommands still need it. |
| `include-env-compressed`, `serde-templated(-derive)` | Delete if unused after publishing subcommands are ported; otherwise move to `crates/`. |
| `amispec` | Move to `crates/amispec/` — used by `twoliter publish ami`. |

Anything moved to `crates/` should keep its old crate name so grep in old-twoliter
still finds the same code.

## Lockfile and Twoliter.lock

Old twoliter kept a `Twoliter.lock` at project root, populated on `twoliter update`
and verified on `twoliter fetch`/`twoliter build`. It stored `(name, vendor,
version, source-image-uri, digest)` for each kit and the SDK.

New twoliter uses **edo's** lockfile: `edo.lock.json` at project root. Kit and
SDK digests live there, keyed by addr. `twoliter update` runs edo's lock
refresh; `twoliter build` respects the lock unless `--no-lock` is passed.

The `override` feature (`Twoliter.override`) becomes an edo `Schema` overlay:
`twoliter --config Twoliter.override.toml build …` (or a dedicated
`[overrides]` table in `Twoliter.toml`). Spec detail in
[`config/overrides.md`](config/overrides.md).

## Concurrency and progress

Edo already provides:

- `tokio::sync::Semaphore` of N workers (default 8).
- Interactive dialoguer prompt on transform failure (view log / retry / shell / quit).
- `tracing-indicatif` progress bars.
- `LogManager` for serializing console output.

Twoliter inherits all of that. There is no separate `TWOLITER_QUIET` variable —
the standard edo `--quiet` / `--verbose` flags apply.

## Error surface

The `src/main.rs::error::Error` enum is the single top-level error. Each new
subsystem (each transform module, each subcommand module) defines its own
`Error` in a local `error` submodule, snafu-transparent-forwards edo's own
errors, and bubbles up. Same pattern as edo core.
