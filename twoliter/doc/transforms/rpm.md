# Transform: `rpm`  (already implemented)

Build a single RPM from a Bottlerocket package source tree. Registered as
`"rpm"` in `src/lib.rs`. Implementation in `src/transform/package.rs`.

This spec is documentation, not a plan. It anchors the format used by every
other transform spec so they can be read together.

## TOML surface

```toml
# packages/kernel-6.1/package.toml
schema-version = "3"

[package]
name = "kernel-6.1"
# Optional; overrides the default of the parent directory name.
# package-name = "kernel-6.1"

# Fine — build-dependencies come from Cargo.toml metadata under
# [package.metadata.build-package].
[package.build]
# Optional; whether the package needs upstream fallback for lookaside.
upstream-source-fallback = false
```

The bulk of the schema is inherited from the RPM `.spec` file and the RPM
external-file table (`[[package.metadata.build-package.external-files]]` in
Cargo.toml — see `old-twoliter/tools/buildsys/src/manifest.rs::ExternalFile`).

## Inputs

- `sdk` — SDK source, from `Twoliter.toml` `[sdk]`.
- `sources` — the package's own source tree.
- `dependencies` — every RPM that this package's spec `BuildRequires`.
- `external-files` — a `lookaside` source per entry in the external-files
  table (planned; today the `rpm` transform inlines this).

## Outputs

Single artifact: one OCI-style layer containing all built `*.rpm` files under
`Packages/<name>/`. Includes debuginfo/debugsource sub-packages.

## Environment

Container farm, base = SDK. Overlays:

1. `builder-group.spec` (included via `include_str!("builder-group.spec")` in
   `src/transform/package.rs:26`) — installs a builder group for `rpmbuild`.
2. Source tree.
3. All build-dep RPMs unpacked into a local dnf repo the container can install
   from.
4. External files (upstream tarballs) placed alongside the spec.

## Algorithm

Currently in `src/transform/package.rs`:

1. Parse `package.toml` + Cargo.toml metadata → `PackageBuildOptions`.
2. Compute unique-id from source hash + SDK + build-deps + external files.
3. Stage overlays.
4. Run `rpmbuild -bb <package>.spec` in the container.
5. Collect `RPMS/**/*.rpm` into a single output layer.

## Unique-id

Already implemented; see `PackageBuildTransform::get_unique_id`.

## Old-code references

- `old-twoliter/tools/buildsys/src/main.rs::build_package`.
- `old-twoliter/tools/buildsys/src/builder.rs::DockerBuild::new_package`.
- `old-twoliter/tools/buildsys/src/cache.rs::LookasideCache` (external-file
  fetch — currently inlined in the transform; will move to the `lookaside`
  source).
- `old-twoliter/tools/buildsys/src/gomod.rs`, `rustmod.rs`, `vendormod.rs`
  (dependency vendoring — replaced by edo's `cargo-vendor` and `go-vendor`
  transforms as *upstream* dependencies of the `rpm` transform).

## Open questions

- **External files.** Currently the `rpm` transform is responsible for fetching
  them. That mixes source-fetching and building. Split them: a dedicated
  `lookaside` source (see `doc/sources/lookaside.md`) becomes an input to the
  `rpm` transform, and the transform just stages the already-fetched files.
- **Go/Rust vendoring.** Old twoliter ran `docker-go`/`docker-cargo` inside
  the package build container. Edo's `cargo-vendor` and `go-vendor` transforms
  do this more cleanly: they run in their own environment, cache their output,
  and feed vendored trees to the `rpm` transform as a dependency. The `rpm`
  spec should reference them for any package with a Cargo/Go bundle.
