# `package.toml` schema

Per-package configuration. One per directory under `packages/`.

## Shape

```toml
schema-version = "3"

[package]
name = "kernel-6.1"
# Optional; overrides the default package-name derived from the directory.
# package-name = "kernel"

# RPM external files (upstream tarballs).
[[package.external-files]]
url = "https://cdn.example.com/kernel-6.1.42.tar.xz"
sha512 = "e3b0c44…"
path = "kernel-6.1.42.tar.xz"           # optional; rename on disk
force-upstream = false                  # optional
bundle-modules = ["go"]                 # optional: "go" | "rust"

# Optional: sources this package uses (from //sources/foo).
# Each is either a path relative to the project root, or an edo addr.
[package.source-groups]
crates = ["hello-agent", "another-crate"]

# Build-dependencies: RPMs that must exist to build this one.
# In old twoliter these came from Cargo.toml build-dependencies.
[[package.build-dependency]]
name = "openssl-devel"

[[package.build-dependency]]
addr = "//packages/glibc-devel"

# Runtime deps (used by rpm2img during variant build).
[[package.dependency]]
name = "systemd"

# Optional: opt into upstream fallback per-package.
[package.build]
upstream-source-fallback = true
```

## Fields

### Required

- `schema-version = "3"`.
- `[package].name` — the crate name (matches the directory name unless
  `package-name` is set, in which case that is the RPM's `Name:` field).

### Optional

- `[package.package-name]` — override for RPM name; needed when the crate
  name is not a valid RPM name.
- `[[package.external-files]]` — one entry per upstream tarball. Same schema
  as `old-twoliter/tools/buildsys/src/manifest.rs::ExternalFile`.
- `[package.source-groups]` — extra source trees under
  `sources/<name>/` that this package should watch for rerun-if-changed.
  Old twoliter had this in `Cargo.toml` metadata.
- `[[package.build-dependency]]` and `[[package.dependency]]` — either
  `name` (resolves to `//packages/<name>` locally) or explicit `addr`.

## Loading

`src/config/package.rs::Package` handles a minimal case today. Extending it:

1. Deserialize.
2. For each `external-files` entry, produce a `lookaside` source node at
   `//packages/<pkg>/external/<sha512-slug>`.
3. For each build-dep, add an `edges` entry in the emitted schema.
4. Emit the `rpm` transform node.

## Old-code references

- `old-twoliter/tools/buildsys/src/manifest.rs::Manifest`,
  `BuildPackage`, `ExternalFile`, `BundleModule` — the reference schema.
- `old-twoliter/tests/projects/local-kit/packages/pkg-a-1.27/Cargo.toml` —
  an example of the old Cargo.toml-based schema; new schema replaces this
  file with `package.toml`.

## Open questions

- Should we keep supporting `Cargo.toml`-embedded metadata alongside
  `package.toml`, for a transition period? Recommendation: no, keep the
  schemas separate. `Cargo.toml` under `packages/*` continues to exist for
  the crate-level dep graph, but twoliter reads `package.toml` for build
  metadata. This is cleaner and closer to how `kit.toml` already works.
- Do we still need `package.build-package.variant-sensitive` /
  `package-features`? Old buildsys hard-fails on both; new twoliter drops
  them entirely.
