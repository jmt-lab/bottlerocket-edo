# Configuration schemas

The config layer is what `src/config/` already partly implements. Each spec
here documents the TOML surface that new twoliter reads.

| Spec | File in `src/config/` |
|---|---|
| [`twoliter-toml.md`](twoliter-toml.md) | `twoliter.rs` (partial). |
| [`package-toml.md`](package-toml.md) | `package.rs` (partial). |
| [`kit-toml.md`](kit-toml.md) | `kit.rs` (partial). |
| [`variant-toml.md`](variant-toml.md) | `variant.rs`. |
| [`overrides.md`](overrides.md) | `overrides.rs` — `Twoliter.override.toml` overlay + `--override`/`--vendor` flags. |
| [`edo-toml-merge.md`](edo-toml-merge.md) | `mod.rs::edo_toml` — already implemented. |

Every twoliter node is one of:

- A **project** (`Twoliter.toml`, `schema-version = "3"`, one per root).
- A **package** (`package.toml`, sits under `packages/<name>/`).
- A **kit** (`kit.toml`, sits under `kits/<name>/`).
- A **variant** (`variant.toml`, sits under `variants/<name>/`).

Loading rules (from `src/config/mod.rs::Config::load`):

1. Read `<root>/Twoliter.toml`.
2. Walk `<root>/packages/*/` for `package.toml`.
3. Walk `<root>/kits/*/` for `kit.toml`.
4. Walk `<root>/variants/*/` for `variant.toml`.
5. In each directory, merge any adjacent `edo.toml` as an escape hatch for
   raw edo schema.

## The schema-version tag

Every twoliter TOML file starts with `schema-version = "3"`. This is a break
from old twoliter's numeric versions (`schema_version = 1` in `Twoliter.toml`,
`schema_version = 1` for lockfile). Version 3 is the current implementation
starting point in `src/config/mod.rs`. There is no auto-migration from earlier
versions.

Serde surface (from `src/config/mod.rs`):

```rust
#[serde(tag = "schema-version")]
pub enum Config {
    #[serde(rename = "3")]
    V1(ConfigType),
}

#[serde(rename_all = "kebab-case")]
pub enum ConfigType {
    Project(Twoliter),
    Package(Package),
    Kit(Kit),
    Variant(Variant),
}
```

Each config type's `to_edo(namespace)` returns an edo `Schema`.
