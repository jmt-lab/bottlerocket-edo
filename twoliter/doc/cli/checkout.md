# CLI: `twoliter checkout`

Copy the materialized artifact at an addr out of edo storage into a directory
on disk. This is the moral equivalent of "look, I built a thing, let me poke
at it."

## Usage

```
twoliter checkout <addr> [--to <dir>] [--layer <name>] [--arch <arch>]
```

- `<addr>` — any edo addr whose transform / source has run.
  Examples: `//packages/kernel-6.1`, `//kits/bottlerocket/core-kit`,
  `//variants/aws-k8s-1.29`.
- `--to <dir>` — output directory. Defaults to
  `./out/<addr-slug>-<arch>/`.
- `--layer <name>` — restrict to a single output layer. Useful for
  `--layer os.img.lz4` on a variant.
- `--arch` — for multi-arch artifacts, pick the arch. Errors on ambiguity.

Already partly implemented in `src/cmd/checkout.rs`. This spec describes the
target semantics.

## Behavior

1. Load the context, resolve `<addr>`.
2. If the node's artifact is not in storage, fail with an error suggesting
   `twoliter build <name>`. Do **not** silently trigger a build — checkout is
   meant to be fast and to reflect the current state.
3. Copy each layer (or the selected layer) out. For OCI images, unpack tars
   into `<dir>/<layer-name>/`. For raw binary layers, write as a file.
4. Also write a `manifest.json` at the top level, containing the addr, the
   artifact's id (digest), and a list of layers with their sizes and media
   types.

## Fetch alias

`twoliter fetch <addr>` is a shortcut for
`twoliter checkout <addr> --to <edo-source-cache>/…`, useful for pulling a
kit-image without unpacking. Old twoliter's `twoliter fetch` was "resolve
lockfile and pull all kits"; new twoliter subsumes that under `edo update`
plus a per-addr `checkout`.

## Old-code references

- `old-twoliter/twoliter/src/cmd/fetch.rs` — the old `fetch` subcommand.
- `old-twoliter/twoliter/src/cmd/debug.rs::CheckToolArgs` — the old debug
  helper that unpacked embedded tools; equivalent is
  `twoliter checkout //edo-source-cache/tools --to /tmp/…` or a
  `twoliter debug tools` subcommand.

## Open questions

- Do we ever want to trigger a build implicitly on missing artifact? No —
  keep it explicit, and print the exact command to run.
