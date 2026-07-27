# CLI: `twoliter fetch`

Pull every source for the project (or a specific addr) into edo's storage,
without running any transforms. Useful in CI to warm caches, and as the
first step of a hermetic build.

## Usage

```
twoliter fetch [--addr <addr>] [--arch <arch>]
```

- `--addr <addr>` — fetch a single source (any node with a `Source`). Defaults
  to fetching everything the project references.
- `--arch <arch>` — only pull layers relevant to the selected arch (relevant
  for kit-image and sdk-image, which are multi-arch).

## Behavior

1. Load the project.
2. Enumerate every `Source` node in the graph.
3. Call `Source::cache(log, storage)` for each, in parallel, respecting the
   worker semaphore.
4. Emit a summary: how many nodes, cache hits, cache misses, bytes fetched.

## Old-code references

- `old-twoliter/twoliter/src/cmd/fetch.rs` — the direct old equivalent, but
  new twoliter's version is broader (also handles lookaside, not just kit
  images).
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.fetch]`,
  `[tasks.fetch-sdk]`, `[tasks.fetch-sources]`, `[tasks.fetch-vendored]` —
  four separate cargo-make targets that collapse into `twoliter fetch`.

## Open questions

- Should `twoliter fetch` also run edo's `cargo-vendor` / `go-vendor`
  transforms (which fetch dependencies)? Recommendation: yes, in
  `--include-vendored` mode. Vendored trees are effectively sources; a cold
  CI run needs them warm.
