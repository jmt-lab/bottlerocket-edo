# CLI: `twoliter prune`

Delete cache entries and build outputs from edo storage. This is the moral
replacement for old twoliter's `clean`, `clean-*`, and `purge-*` targets.

## Usage

```
twoliter prune                             # unused local-cache entries
twoliter prune --all                       # everything, including source caches
twoliter prune --older-than <duration>     # e.g. 7d, 24h
twoliter prune --addr <addr>               # a specific node's artifact
twoliter prune --kind <kind>               # e.g. rpm, kit, variant-image
twoliter prune --sources                   # only source caches
twoliter prune --outputs                   # only variant-image / eif outputs
```

`src/cmd/prune.rs` has a starting stub.

## Behavior

Delegates to edo's storage backend prune API. Order of operations:

1. Compute the set of artifacts referenced by the current project graph
   (roots + all transitive artifacts). Never delete these unless
   `--force` is passed.
2. Walk each storage slot (local, source-caches, build-cache, output-cache).
3. For each slot, apply the filters.
4. Emit a summary: entries removed, bytes freed.

## Which cargo-make targets it replaces

- `clean`, `clean-workspace`, `clean-packages`, `clean-kits`, `clean-images`,
  `clean-repos`, `clean-metadata`, `clean-sources`, `clean-logs`,
  `clean-state`, `clean-tools`, `purge-cache`, `purge-go-vendor`,
  `purge-cargo`.

Most of these map to `--kind` or `--sources`/`--outputs` filters. A few
(`clean-logs`, `clean-tools`) become no-ops: edo owns the log stream and
there's no persistent tools directory anymore.

## Old-code references

- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.clean*]` and
  `[tasks.purge-*]`.
- `old-twoliter/twoliter/src/cmd/build_clean.rs` — the old `twoliter build
  clean` entry point.

## Open questions

- Should `prune` require confirmation when it would free >1 GiB? Probably.
  Add a `--yes` flag to skip.
