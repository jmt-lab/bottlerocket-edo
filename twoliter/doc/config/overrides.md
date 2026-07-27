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
schema-version = "3"

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
twoliter --config Twoliter.override.toml build variant aws-k8s-1.29
```

(The `--config` flag can appear multiple times, later files override
earlier ones. Reuse edo's existing overlay merge — this is what
`schema.merge` already does in `src/config/mod.rs`.)

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

## Open questions

- Should override files be committed or gitignored? Old twoliter committed
  the override file for tests but expected day-to-day users to gitignore it.
  Recommendation: gitignore the plain `Twoliter.override.toml` name by
  default; support `--config <any-path>` for anything else.
- Do we need "override the SDK digest"? Yes — same shape as kit override,
  under `[sdk-override]`. Same reasons: dev pinning, hotfix.
