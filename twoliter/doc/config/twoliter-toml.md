# `Twoliter.toml` schema

Project-root configuration. One per project.

## Shape

```toml
schema-version = "3"
release-version = "1.42.0"

[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "=0.66.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[vendor.my-vendor]
registry = "public.ecr.aws/my-vendor"

# Kits from vendors (external dependencies).
[[kit]]
name = "core-kit"
vendor = "bottlerocket"
version = "=1.42.0"

[[kit]]
name = "another-kit"
vendor = "my-vendor"
version = "=0.1.0"

# Optional: S3-backed lookaside source cache.
[lookaside]
bucket = "bottlerocket-lookaside-v2"
prefix = "cache"
```

## Fields

### Required

- `schema-version = "3"` — schema discriminant. Older `"1"` and `"2"`
  documents are not accepted.
- `release-version` — the "image version" stamp used across the whole
  project. Typed as strict semver (`X.Y.Z`); non-semver strings are
  rejected at parse time. Corresponds to `BUILDSYS_VERSION_IMAGE` in old
  twoliter. Emitted into the schema's top-level `[bottlerocket]` config as
  `release-version = "v<X.Y.Z>"`, which the RPM package transform in
  `src/transform/package.rs` reads to build the `.<timestamp>.<version>.br1`
  dist tag. Individual kits may override this project-level default by
  emitting their own `[bottlerocket]` config in `Kit::to_edo()`; the last
  merger wins, so a per-kit `release-version` takes precedence over the
  project-level one.
- `[sdk]` — the SDK image everything builds against.
  - `name` — image name; also the requirement address (`//<name>`).
  - `vendor` — the `[vendor.<name>]` key this SDK is pulled from.
  - `version` — semver requirement on the SDK image tag.

### Optional

- `[vendor.<name>]` — one entry per OCI registry the project pulls kits
  or an SDK from. `registry` is the base URI (e.g.
  `public.ecr.aws/bottlerocket`). Each vendor produces two schema
  elements internally: a `kit-image` vendor at `//vendor/<name>` and an
  `image` vendor at `//vendor/<name>-sdk`. Because of the `-sdk` suffix
  collision, declaring both `foo` and `foo-sdk` as vendors is a hard
  error.
- `[[kit]]` — external kit dependencies loaded as `kit-image` sources.
  Fields `name`, `vendor` (must match a `[vendor.<name>]` key), and
  `version` (semver requirement). Each entry becomes a requirement at
  `//kits/<vendor>/<name>`.
- `[lookaside]` — S3-backed source cache mounted in front of
  `[[package.external-files]]` fetches. On cache hit the blob comes from
  S3; on miss edo falls through to the upstream URL. Fields:
  - `bucket` (required) — S3 bucket holding the shared cache blobs.
  - `prefix` (optional) — key prefix inside the bucket.

  Region and credentials come from the ambient AWS config
  (`aws_config::load_defaults`). Field names and semantics mirror
  `edo-core`'s `S3BackendOptions`. Omitting `[lookaside]` leaves the
  `"lookaside"` source-cache slot empty, so an `edo.toml` overlay in the
  same directory can define one without collision.

## What is emitted

`Twoliter::to_edo()` (in `src/config/twoliter.rs`) builds a `Schema` with:

- One `kit-image` vendor element per `[vendor.<name>]` at
  `//vendor/<name>`, plus a parallel `image` vendor element at
  `//vendor/<name>-sdk` for the SDK.
- One `image` requirement at `//<sdk.name>`, resolved through
  `//vendor/<sdk.vendor>-sdk`.
- One `kit-image` requirement at `//kits/<vendor>/<name>` per `[[kit]]`,
  resolved through `//vendor/<vendor>`.
- Two container environments both sourced from the resolved SDK image:
  `//sdk-isolated` (no network) and `//sdk-open` (network enabled).
- A local source-cache slot at `//edo-source-cache/kit-image` under
  `target/edo/source-cache/kit-image` (may be overridden by an `edo.toml`
  overlay).
- If `[lookaside]` is present, an `s3`-kind source-cache slot at
  `//edo-source-cache/lookaside` with the configured bucket and prefix.
- A top-level config table `[bottlerocket]` carrying
  `release-version = "v<X.Y.Z>"`.

Overrides (`Twoliter.override.toml`, `--override <path>`, `--vendor <name>=<registry>`)
are applied on top of this schema before an adjacent `edo.toml` is merged
in; see `doc/config/overrides.md`.

## Loading

`src/config/mod.rs::Config::twoliter_toml` reads the file, applies
overrides, merges any adjacent `edo.toml`, and calls
`Schema::propagate`. Kits and packages under `kits/` and `packages/` are
then walked and merged into the same schema.

## Old-code references

- `old-twoliter/twoliter/src/project/mod.rs::Project` — the corresponding
  struct in old twoliter.
- `old-twoliter/tests/projects/local-kit/Twoliter.toml` — an example.
- `old-twoliter/twoliter/src/project/vendor.rs::ArtifactVendor` — how
  vendors are resolved in old twoliter.

## Notes

- `release-version` is typed as `semver::Version` (strict `X.Y.Z`). This
  is more restrictive than old twoliter, which accepted arbitrary
  strings.
- `--vendor <name>=<registry>` is a CLI overlay handled in
  `Config::load`'s `inline_vendor_overrides` and merged with any
  `Twoliter.override.toml` and `--override <path>` files. Later entries
  win.
