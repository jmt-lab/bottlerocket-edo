# `edo.toml` merge semantics

`src/config/mod.rs::Config::edo_toml` is already implemented. This spec
documents what it does and when to use it.

## Behavior

For every directory that `Config::load` walks (project root, each
`packages/*/`, each `kits/*/`, each `variants/*/`), an adjacent
`edo.toml` — if present — is parsed as a raw edo `Schema` and merged into the
twoliter-generated schema for that directory.

Order:

1. Load the twoliter TOML file (`Twoliter.toml`, `package.toml`, `kit.toml`,
   `variant.toml`).
2. Convert it to an edo `Schema` via the type's `to_edo`.
3. If an `edo.toml` is adjacent, parse it as a `Schema` and merge — the
   raw edo schema wins on conflicts.
4. `propagate` the merged schema (so children see parent defaults).

## When to use `edo.toml`

The twoliter TOML files are opinionated: they know about packages, kits,
variants. Anything that falls outside that vocabulary needs `edo.toml`:

- Adding a custom edo `Farm` or `Storage` backend.
- Adding a `script` transform that runs at build time (e.g. codegen).
- Adding a plain `local` or `git` source for a custom asset.
- Overriding an addr's `depends` in ways twoliter doesn't expose.

## Example

```toml
# packages/hello-agent/edo.toml
#
# The package.toml already emits //packages/hello-agent as an `rpm` transform.
# This edo.toml adds an auxiliary `script` transform that regenerates a
# codegen artifact before the RPM builds.

[nodes."//packages/hello-agent/generated"]
kind = "script"
farm = "//container"
depends = ["//packages/hello-agent/sources"]
command = "protoc --rust_out=... schema.proto"

[nodes."//packages/hello-agent"]
depends = ["//packages/hello-agent/generated"]
```

## Old-code references

- No direct old equivalent. Old twoliter forced everything through
  `Makefile.toml` + `Cargo.toml` metadata; there was no escape hatch to
  express arbitrary DAG shape.
- Edo docs: `crates/edo/src/context/schema.rs` for the full schema grammar.

## Open questions

- Should we forbid `edo.toml` from redefining nodes twoliter already emitted
  (e.g. the `rpm` transform for a package)? Recommendation: no — allow the
  merge, but warn. Overriding lets users customize their build; forbidding
  it forces them to fork.
