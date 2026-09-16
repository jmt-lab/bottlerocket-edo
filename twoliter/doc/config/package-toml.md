# `package.toml` schema

Per-package configuration. One per directory under `packages/`. The file is
optional: a directory with only an RPM `.spec` file and no `package.toml` is
picked up by directory walk but contributes no schema of its own. Any
`edo.toml` alongside `package.toml` is merged in after the derived schema
(see `src/config/mod.rs::Config::package_toml`).

## Shape

```toml
schema-version = "3"

[package]
name = "kernel-6.1"
# depends: RPMs that must be built before this one. Each entry `foo`
# becomes an edge to `//packages/foo/rpm`.
depends = ["glibc", "openssl"]
# Optional: non-default Go toolchain for `go-vendor` transforms. Mirrors
# the `export GO_MAJOR="X.Y"` line that some spec files put in their
# `%build` section (kubernetes, containerd-2.3, etc.). Omit to use the
# SDK's default Go.
go-major = "1.26"

# Upstream tarball fetched via edo's `remote` source. The `[lookaside]`
# S3 source cache in Twoliter.toml (if present) sits transparently in
# front of this fetch — cache hit reads from S3, miss falls through to
# `url` and verifies against `ref`.
[[package.source]]
url = "https://cdn.example.com/kernel-6.1.42.tar.xz"
ref = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
out = "kernel-6.1.42.tar.xz"
# Optional: vendor Go modules found in this source tree. Each entry is a
# module path, optionally suffixed with `@<name>` to override the output
# name (defaults to `bundled-<out>`).
go-modules = ["vendor/github.com/foo/bar"]
# Optional: vendor Cargo projects. Each entry is a path to a Cargo.toml
# relative to the source root.
cargo-projects = ["Cargo.toml"]

# A local directory dropped into the RPM build tree.
[[package.source]]
path = "files"
out = "files"

# A shared source tree referenced by edo address. Typically points at
# `//sources/<name>` defined elsewhere in the project.
[[package.source]]
addr = "//sources/hello-agent"
# Optional: other addressed sources that this one vendors from; wired
# through to the RPM transform as `vendored`.
vendored = ["//sources/another-crate"]
```

## Fields

### Required

- `schema-version = "3"` — top-level version tag on the outer `Config`
  enum (`src/config/mod.rs`). All three of `package.toml`, `kit.toml`,
  and `Twoliter.toml` share this tag; the `[package]` table
  discriminates.
- `[package].name` — the package name. Used as the RPM package name and
  as the address suffix for the emitted transform (`//packages/<name>/rpm`).

### Optional

- `[package].depends` — flat list of package names this package's RPM
  build depends on. Each `foo` becomes an edge to `//packages/foo/rpm`.
  The RPM `.spec`'s own `BuildRequires`/`Requires` still drive install
  semantics; `depends` only shapes the DAG.
- `[[package.source]]` — zero or more input sources. Untagged
  enum discriminated by which fields are present:
  - **URL source** (`url` + `ref` + `out`): fetched via edo's `remote`
    source. `ref` is the SHA-512 (or whatever digest edo's `remote`
    accepts). `out` is the filename dropped into `SOURCES/`.
    Short-circuited by the `[lookaside]` cache in `Twoliter.toml` when
    configured.
  - **Local source** (`path` + `out`): a directory checked into the
    project. `path` is relative to the package directory; `out` is where
    it lands in the RPM build tree.
  - **Addressed source** (`addr` + optional `vendored`): a reference to
    an edo address defined elsewhere (typically `//sources/<name>`).
    `vendored` lists sibling addresses that this source vendors from and
    is forwarded to the RPM transform.
- `go-modules` / `cargo-projects` on URL and local sources — trigger a
  `go-vendor` or `cargo-vendor` transform against that source. The
  vendored artifact is fed into the RPM transform as `bundled-<name>`.
- `[package].go-major` — non-default Go toolchain major version
  (e.g. `"1.26"`) exported as `GO_MAJOR=<value>` for every `go-vendor`
  transform emitted for this package. The Bottlerocket SDK ships more
  than one Go and selects between them by inspecting `GO_MAJOR`;
  packages whose `%build` section sets `export GO_MAJOR="X.Y"` must
  mirror the same value here so `go mod vendor` runs under the matching
  toolchain. `just sync-package` extracts it from the spec
  automatically. Omit for the SDK default.

## Emitted schema

`Package::to_edo` (in `src/config/package.rs`) produces, under
`//packages/<name>/`:

1. `package-src` — a `local` source pointing at the package directory
   (contains the `.spec` file and any adjacent files).
2. For each `[[package.source]]`:
   - `src-<i>` — a `remote` (URL), `local` (path), or address reference.
   - `src-<i>-go-vendored-<j>` — one `go-vendor` transform per
     `go-modules` entry, running under `//sdk-open`.
   - `src-<i>-cargo-vendored` — one `cargo-vendor` transform if
     `cargo-projects` is non-empty, running under `//sdk-open`.
3. `rpm` — the `rpm` transform running under `//sdk-isolated`, wired to:
   - `sources`: package dir + every local/addressed source.
   - `archives`: every URL source.
   - Config: `package` name, `depends` (as `//packages/<name>/rpm`
     edges), `vendored` addresses, `bundled` map.

## Design notes

- **No dedicated `lookaside` source kind.** External upstream tarballs
  are just `url`-style sources. The S3 lookaside cache is a
  `[cache.source.lookaside]` element seeded by `[lookaside]` in
  `Twoliter.toml` — see [`../sources/lookaside.md`](../sources/lookaside.md).
- **No `force-upstream` / `upstream-source-fallback`.** Edo always falls
  through to the upstream URL on cache miss; the old buildsys toggles
  are not ported.
- **No build-vs-runtime dep split.** `depends` shapes the DAG only.
  Install-time `Requires` come from the RPM spec file itself.
- **No `variant-sensitive` / `package-features`.** Old buildsys
  hard-failed on both; the new schema drops them.

## Old-code references

- `old-twoliter/tools/buildsys/src/manifest.rs::Manifest`,
  `BuildPackage`, `ExternalFile`, `BundleModule` — the pre-migration
  reference schema (embedded in `Cargo.toml`).
- `old-twoliter/tools/buildsys/src/cache.rs::LookasideCache` — old
  external-file fetch logic, now subsumed by edo's `remote` source plus
  the `[lookaside]` S3 cache.
