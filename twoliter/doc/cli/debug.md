# CLI: `twoliter debug`

Introspection commands for troubleshooting builds, matching old twoliter's
`twoliter debug check-tools` and a few new things edo makes possible.

## Subcommands

```
twoliter debug graph [--addr <addr>] [--format dot|json|text]
twoliter debug lockfile
twoliter debug tools [--to <dir>]
twoliter debug artifact <addr>
twoliter debug environment <farm>
```

### `twoliter debug graph`

Dump the DAG. `--format dot` produces graphviz output; `--format json`
produces `{ nodes: [{addr,kind,depends:[…]}], edges: [{from,to,kind}] }`.
Useful for figuring out "why is this rebuilding?"

### `twoliter debug lockfile`

Pretty-print `edo.lock.json`. Show every resolved addr, its digest, and
whether it's currently pinned in storage.

### `twoliter debug tools [--to <dir>]`

Old twoliter's `debug check-tools`. Unpacks the embedded tool bundle to
`<dir>` (or a tempdir) and prints the path. Since new twoliter's "tools" are
the container-side scripts embedded in `crates/twoliter/scripts/`, this
becomes: write the scripts to disk for inspection.

### `twoliter debug artifact <addr>`

Show the OCI manifest for the artifact at `<addr>` — layers, media types,
sizes, labels. Doesn't unpack (use `checkout` for that).

### `twoliter debug environment <farm>`

Show the resolved farm configuration for `<farm-addr>`: kind, image, network
policy, mounts, resource limits.

## Old-code references

- `old-twoliter/twoliter/src/cmd/debug.rs` — the whole subcommand.
