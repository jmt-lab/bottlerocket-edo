# `Twoliter.toml` schema

Project-root configuration. One per project.

## Shape

```toml
schema-version = "3"
release-version = "1.42.0"

[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "0.66.0"

[vendor.bottlerocket]
registry = "public.ecr.aws/bottlerocket"

[vendor.my-vendor]
registry = "public.ecr.aws/my-vendor"

# Kits from vendors (external dependencies).
[[kit]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"

[[kit]]
name = "another-kit"
vendor = "my-vendor"
version = "0.1.0"

# Optional: lookaside cache configuration.
[lookaside]
url = "https://cache.bottlerocket.aws"
upstream-source-fallback = false

# Optional: extra farms.
[[farm]]
addr = "//container"
kind = "container"

[[farm]]
addr = "//arm-builders"
kind = "container"
# ... farm-specific fields
```

## Fields

### Required

- `schema-version = "3"`.
- `release-version` — the "image version" used across the whole project.
  Ends up as `BUILDSYS_VERSION_IMAGE` in old twoliter. Referenced by
  transforms that stamp version strings into artifacts (rpm's `Release:`,
  variant manifest, kit metadata).
- `[sdk]` — the SDK image everything builds against. `name`, `vendor`,
  `version`.

### Optional

- `[vendor.<name>]` — each vendor gets a `registry`. Referenced by `[sdk]`
  and `[[kit]]`. If a kit references a vendor that has no `[vendor.<name>]`
  entry, loading fails.
- `[[kit]]` — external kit dependencies at project level. Loaded as
  `kit-image` sources (see `doc/sources/kit-image.md`).
- `[lookaside]` — override the default lookaside URL and upstream fallback.
- `[[farm]]` — extra edo farms beyond the default `//default`. Kind
  `container` is expected for RPM/kit/variant builds.

## What already works

`src/config/twoliter.rs::Twoliter` handles the minimal `[registry]` case and
`Twoliter::to_edo()` returns a Schema. This spec extends that to include
`[sdk]`, `[[kit]]`, `[[farm]]`, `[lookaside]`, and per-vendor entries.

## Loading

`src/config/mod.rs::Config::twoliter_toml` reads and dispatches. The output
Schema propagates upward via `Schema::propagate` and merges with any
`edo.toml` in the same directory.

## Old-code references

- `old-twoliter/twoliter/src/project/mod.rs::Project` — the corresponding
  struct in old twoliter.
- `old-twoliter/tests/projects/local-kit/Twoliter.toml` — an example.
- `old-twoliter/twoliter/src/project/vendor.rs::ArtifactVendor` — how vendors
  are resolved in old twoliter.

## Open questions

- Should `release-version` be a semver (`1.42.0`) or a build-date string?
  Old twoliter accepted arbitrary strings. Recommendation: constrain to
  semver in v3. It participates in unique-ids.
- Do we support `[vendor.<name>]` overrides via CLI flag
  (`--vendor bottlerocket=public.ecr.aws/foo`)? Recommendation: yes, and
  it merges as an overlay on top of the parsed schema.
