# Overrides

Old twoliter supported a `Twoliter.override` file that let a project pin
kits/SDK to alternative registries or explicit digests for local dev,
testing, or emergency hotfixes.

`old-twoliter/twoliter/src/project/mod.rs::TWOLITER_OVERRIDES` = `"Twoliter.override"`.
`ArtifactVendor::Overridden` was the type carrying the substitution.

## Two paths in new twoliter

### 1. Vendor rewriting (recommended)

`Twoliter.override.toml` next to `Twoliter.toml`:

```toml
# Redirect any kit that uses the "bottlerocket" vendor to a private registry
# for this build only.
[vendor.bottlerocket]
registry = "public.ecr.aws/my-mirror/bottlerocket"

# Or pin a single kit to a specific digest.
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
digest = "sha256:abcdef…"

# Or point a kit at a local OCI archive.
[[kit-override]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
local-path = "./mock-kits/core-kit.oci"
```

Loading rule: the override file is read after `Twoliter.toml` and merged as
an overlay. Overridden entries win.

CLI:

```
twoliter --override Twoliter.override.toml build variant aws-k8s-1.29
```

`--override` is a repeatable global flag; later files win. The file next
to `Twoliter.toml` is auto-discovered and merged first, so a plain
`twoliter build …` picks it up without any flag.

The user-visible spelling is `--override`, not `--config`: twoliter
inherits edo's global `--config <path>` flag, which points at the
user-level `~/.config/edo.toml`. Repurposing that name would break the
edo contract, so overrides get their own flag.

Vendor-only overrides reuse edo's `Schema::merge` overlay behaviour
(last-writer-wins on vendor addresses). Kit and SDK pins go one step
further: the base `[requires.<addr>]` entry is removed and a
pre-resolved source element is injected at the same address, so
`Project::build` skips the resolver for that address entirely and the
build consumes the pinned digest or local archive. Every override run
therefore causes a lockfile refresh (`edo.lock.json` is rewritten).

### 2. Per-invocation flag

```
twoliter build variant aws-k8s-1.29 \
    --vendor bottlerocket=public.ecr.aws/my-mirror/bottlerocket
```

`--vendor <name>=<registry>` composes an inline override at load time. Useful
in CI when you don't want to commit an override file.

## Old-code references

- `old-twoliter/twoliter/src/project/mod.rs::TWOLITER_OVERRIDES`.
- `old-twoliter/twoliter/src/project/vendor.rs::ArtifactVendor::Overridden`.
- `old-twoliter/twoliter/src/test/data/override/Twoliter-override-1.toml` —
  test fixture showing the old shape.

## Notes

- `Twoliter.override.toml` should be gitignored by default. Old
  twoliter committed the override file for tests but expected day-to-day
  users to gitignore it. Anything that needs to live under version
  control can be passed via `--override <any-path>`.
- SDK pinning is supported via a `[sdk-override]` table with the same
  shape as `[[kit-override]]`. Only one SDK override is meaningful per
  build, so it is a single table rather than an array.
- `local-path` overrides emit an edo `local` source at the require
  address rather than a `kit-image` source, because the `kit-image`
  source pulls through `ocilot` and does not accept `file://` URIs. The
  kit-metadata label check that `KitImageSource::fetch` performs is
  therefore skipped for local overrides; use `digest` if you need that
  check.

## Implementation

- `src/config/overrides.rs` — the `TwoliterOverride` struct, its
  `merge`/`apply`/`from_cli` methods, and the inline tests.
- `src/config/mod.rs::Config::load` — takes `overrides: &[PathBuf]` and
  `inline_vendor_overrides: &[(String, String)]`, auto-discovers
  `Twoliter.override.toml`, merges the file/CLI overlays in order, and
  calls `TwoliterOverride::apply` before merging any adjacent
  `edo.toml`.
- `src/main.rs` — the `--override` and `--vendor` global flags on
  `Args`, threaded into `src/cmd/mod.rs::create_context`.
