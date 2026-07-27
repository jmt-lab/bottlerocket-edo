# `kit.toml` schema

Per-kit configuration. One per directory under `kits/`.

## Shape

```toml
schema-version = "3"

[kit]
name = "core-kit"
vendor = "bottlerocket"         # must match a [vendor.<name>] in Twoliter.toml
version = "0.1.0"

# The packages this kit bundles. Each is an edo addr (typically to a local
# rpm transform).
packages = [
    "//packages/pkg-a-1.27",
    "//packages/pkg-b",
    "//packages/pkg-c",
]

# Optional: architecture restrictions. If absent, kit is built for every arch
# the project supports.
arches = ["x86_64", "aarch64"]

# Optional: transitively include the packages from another kit. Old twoliter
# expressed this via Cargo.toml build-dependencies pointing at other kit
# directories.
[[kit.includes-kit]]
addr = "//kits/another-kit"
```

## Fields

### Required

- `schema-version = "3"`.
- `[kit].name`, `[kit].vendor`, `[kit].version`.
- `[kit].packages` — list of addrs.

### Optional

- `[kit].arches` — restrict to a subset.
- `[[kit.includes-kit]]` — transitive kit inclusion, for when one kit is a
  strict superset of another.

## Loading

`src/config/kit.rs::Kit` handles a minimal case (name only). Extend to
support `packages`, `vendor`, `version`, `arches`, `includes-kit`. Emit a
`kit` transform node at `//kits/<name>` with `depends = [ … packages … ]`.

## Old-code references

- `old-twoliter/tools/buildsys/src/manifest.rs::BuildKit` — reference
  schema (very minimal — just `kit_name`, since the old world derived
  package list from Cargo.toml).
- `old-twoliter/tests/projects/local-kit/kits/core-kit/Cargo.toml` —
  example old format.

## Open questions

- Should packages be a list of names (`["pkg-a-1.27", "pkg-b"]`) resolved
  against `//packages/*`, or a list of addrs? Recommendation: addrs, because
  it lets kits pull in packages from other namespaces (e.g. a shared package
  namespace across multiple kits in one project).
- Should `[kit].version` be inferred from `Twoliter.toml`'s
  `release-version`? Old twoliter did — everything got the project's version.
  Recommendation: allow both. If `version` is missing, fall back to
  `Twoliter.toml`'s `release-version`.
