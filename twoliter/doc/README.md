# New twoliter — roadmap to functional equivalence

This directory holds the specification for re-implementing twoliter on top of
[edo](https://github.com/awslabs/edo). It is written by working backward from
`old-twoliter/` (the current, shipping implementation) and forward from the
edo abstractions (`Storage`, `Source`, `Environment`, `Transform`, `Vendor`)
plus what we already have here (`src/`).

The goal is not a straight port. Old twoliter is a thin CLI that:

1. Reads a project (`Twoliter.toml`, `Twoliter.lock`, `Cargo.toml` package/kit/variant
   manifests).
2. Extracts an embedded tarball of tools + scripts + a `Makefile.toml`.
3. Sets a small set of env vars.
4. Shells out to `cargo make`, which drives everything through `docker build`
   invocations orchestrated by the embedded `buildsys` binary and shell scripts
   (`rpm2img`, `rpm2kit`, `rpm2eif`, `partyplanner`, `imghelper`, `img2img`,
   `guest-images-helper`).

Edo replaces that entire "cargo-make + docker + embedded buildsys" spine with:

- A DAG of `Node`s described in TOML (`Twoliter.toml`, `package.toml`,
  `kit.toml`, `variant.toml`, or plain `edo.toml`).
- A `Scheduler` that walks the DAG, hashes inputs, checks a content-addressed
  cache, and only invokes a `Transform` when its output is not already present.
- `Environment`s (local shell, container) instead of one Dockerfile with layered
  stages.
- `Source`s (local, git, remote, oci) that already know how to cache themselves
  in the layered storage.

So the port is a redesign, and each old twoliter capability lands in one of five
buckets:

| Bucket | What it becomes |
|---|---|
| Building an RPM | An edo `Transform` (`rpm`) — already ported. |
| Building a kit | An edo `Transform` (`kit`) — already ported. |
| Building a variant image / EIF / migrations | New edo `Transform`s per output (`variant-image`, `eif`, `kmodkit`, `migrations`, `repack-variant`, `kit-metadata`, `advisory-check`). |
| Fetching kits/SDK | New edo `Source`s (`kit-image`, `sdk-image`), plus a lookaside source for RPM external files. |
| Publishing / distributing outputs | Standalone `twoliter` subcommands (`publish kit`, `publish ami`, `publish ssm`, `publish repo`, `publish ova`, `pcrsys …`) that run **outside** the DAG once the artifacts already exist in storage. |

Everything that old twoliter shipped as an embedded shell script (`rpm2img`,
`rpm2eif`, `partyplanner`, `imghelper`, `img2img`, `rpm2kmodkit`,
`rpm2migrations`, `guest-images-helper`) is **ported to Rust**. The shared
primitives (partition tables, dm-verity, lz4 pipelines, dnf-driven rootfs
install, SBOM generation, grub install + signing) land in a new
`crates/br-image/` library that every image-building transform links.
See [`image-building.md`](image-building.md) and
[`embedded-scripts.md`](embedded-scripts.md) for the port plan.

Everything that old twoliter shipped as an embedded Rust binary is one of:

- **A `Transform` implementation.** `buildsys` (RPM/kit/variant builds) becomes
  the `rpm`/`kit`/`variant-image`/`eif`/`repack-variant` transforms. `eif-builder`,
  `pcrsys`, `advisory-checker` become transforms or subcommands.
- **A `Source` implementation.** The oci-cli-wrapper/`krane`-driven kit puller
  becomes the `kit-image` source; the SDK resolver becomes the `sdk-image`
  source.
- **A twoliter subcommand.** `pubsys` (AMI/SSM/repo/OVA publishing), `pubsys-setup`
  (TUF key/root bootstrap), and `pcrsys` (PCR prediction) live as first-class
  twoliter subcommands that operate on artifacts already produced by the DAG.
  They are not on the build critical path.
- **Deleted.** `unplug`, `pipesys`, `docker-go`, `docker-cargo`, and the whole
  `build.Dockerfile` disappear: seccomp-based network isolation, FD-passing,
  and container-in-container Go/Rust vendoring are all provided directly by
  edo's `container` environment and `cargo-vendor` / `go-vendor` transforms.

Testsys is out of scope. It is deprecated and not being ported.

## What is already done

- `src/transform/package.rs` — the `rpm` transform (builds an RPM by executing
  `rpmbuild` inside a container environment that has been staged with the
  package sources, its SDK, and its build-dependency RPMs).
- `src/transform/kit.rs` — the `kit` transform (packs a set of RPMs into an OCI
  image with kit metadata as a labeled config blob).
- `src/config/` — a partial project loader that reads `twoliter.toml`,
  `kit.toml`, `package.toml`, and forwards them to edo's `Schema`.
- `src/cmd/{build,publish,checkout,run,list,prune,update}.rs` — thin CLI
  scaffolding around edo primitives. Most are functional; `publish` currently
  only knows how to push a kit.

## What is not

The rest of this directory is what still has to be built. Docs are grouped by
what they define:

- [`architecture.md`](architecture.md) — how the pieces fit together, addressing
  scheme, DAG shape, cache slots.
- [`image-building.md`](image-building.md) — the `crates/br-image/` library
  where all the ported shell-script logic (partitioning, dm-verity, mkfs,
  compression, grub install, SBOM) lives.
- [`embedded-scripts.md`](embedded-scripts.md) — inventory of every file in
  `old-twoliter/twoliter/embedded/` and its fate under the new plan.
- [`config/`](config/) — TOML schemas (`Twoliter.toml`, `package.toml`,
  `kit.toml`, `variant.toml`, `Infra.toml`, `edo.toml` merge rules).
- [`transforms/`](transforms/) — each new `Transform` kind that must be
  registered with the edo `Registry`.
- [`sources/`](sources/) — each new `Source` kind (kit-image, sdk-image, lookaside).
- [`cli/`](cli/) — twoliter CLI subcommands and their arguments.
- [`publish/`](publish/) — publishing paths (AMI, SSM, repo, OVA, publish-kit,
  TUF setup) that run outside the DAG.

Each spec follows the same shape: **Purpose → TOML surface → Inputs (edo
Depends) → Outputs (Artifact layers) → Environment → Algorithm → Old-code
references → Open questions.** That makes it easy to read one spec, look at
the corresponding old-twoliter code, and implement.

## Reading order

If you are picking this up cold:

1. `architecture.md` — the mental model.
2. `config/twoliter-toml.md` and `config/package-toml.md` — what a project looks
   like on disk.
3. `sources/kit-image.md` and `sources/sdk-image.md` — how the graph gets its
   external inputs.
4. `transforms/rpm.md` and `transforms/kit.md` — these already exist in
   `src/transform/`; the specs describe intent, not aspiration.
5. `transforms/variant-image.md` — the largest new transform; it is what old
   twoliter's `build-variant` target does.
6. Everything else in whatever order.

## Non-goals

- Bit-for-bit reproducing old twoliter's on-disk layout. The `build/` tree,
  `Twoliter.lock` format, `external-kits/` marker files, `verification-tagger`,
  etc. are edo-owned now: caching is content-addressed, resolution is in
  `edo.lock.json`, artifacts live in edo's storage backends. Any consumer that
  reads paths under `build/` needs to migrate to `edo checkout` /
  `twoliter checkout`.
- `cargo make` compatibility. There will be no `Makefile.toml`. Old cargo-make
  targets map to `twoliter build …`, `twoliter publish …`, or direct
  `edo run //<addr>` invocations.
- Custom migrations from Twoliter 1.x. The v2/v3 schema-version migrator in
  `old-twoliter/twoliter/src/project/migrate/` is not being ported. New twoliter
  starts at schema version 3 (matching the current `src/config/mod.rs`) and does
  not attempt to auto-upgrade older projects.
- testsys. Deprecated; skipped entirely.
