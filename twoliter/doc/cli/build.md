# CLI: `twoliter build`

`twoliter build` runs the DAG for a specific artifact. Every subcommand
resolves an edo `Addr` and hands off to the scheduler.

## Subcommands

```
twoliter build package <name>              [--arch <arch>]
twoliter build kit <name>                  [--arch <arch>] [--vendor <vendor>]
twoliter build variant <name>              [--arch <arch>]
twoliter build eif <name>                  [--arch <arch>]
twoliter build kmodkit <name>              [--arch <arch>] [--vendor <vendor>]
twoliter build migrations <name>           [--arch <arch>]
twoliter build repack <variant>            [--arch <arch>]
twoliter build all                         [--arch <arch>]
```

The `package`, `kit` subcommands already exist in `src/cmd/build.rs`; the
others follow the same pattern.

### `twoliter build package <name>`

Resolves to `//packages/<name>` and runs `Scheduler::run`. Equivalent to the
old `cargo make -e PACKAGE=<name> build-package`.

### `twoliter build kit <name>`

Resolves to `//kits/<vendor>/<name>` (defaulting `<vendor>` to the project's
first vendor if unambiguous, else erroring). Equivalent to the old
`cargo make -e BUILDSYS_KIT=<name> build-kit`.

### `twoliter build variant <name>`

Resolves to `//variants/<name>` (`variant-image` transform). Equivalent to
`cargo make -e BUILDSYS_VARIANT=<name> build-variant`.

### `twoliter build eif <name>`

Resolves to `//variants/<name>/eif`.

### `twoliter build kmodkit <name>`

Resolves to `//kits/<vendor>/<name>` where the node's kind is `kmodkit`.

### `twoliter build migrations <name>`

Resolves to `//variants/<name>/migrations`.

### `twoliter build repack <variant>`

Resolves to `//variants/<variant>/repack`.

### `twoliter build all`

Enumerates every leaf (`variant-image`, `eif`, standalone `kit`) in the
project graph and runs them. Equivalent to the old `build-all` target.

## Arguments common to all

- `--arch <x86_64|aarch64>` — passed to the resolver; overrides the project's
  default arch. Nodes that don't care about arch (e.g. a project-wide
  advisory-check) ignore it.
- `--lookaside-cache <url>` — override `Twoliter.toml` `[lookaside].url`.
- `--upstream-source-fallback` — override `Twoliter.toml`
  `[lookaside].upstream-source-fallback`.
- `--jobs <N>` — worker semaphore size (default 8).
- `--no-lock` — ignore `edo.lock.json`.

## Old-code references

- `old-twoliter/twoliter/src/cmd/build.rs` — old subcommand structure.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.build-*]` — targets
  each new subcommand replaces.

## Open questions

- Should `twoliter build all` also run publish-side checks (advisories,
  lockfile validity)? Recommendation: no. Keep `build` DAG-only; add a
  separate `twoliter check all`.
- Should there be a `twoliter build project` that builds every declared
  variant in `Twoliter.toml`? Probably folded into `build all`.
