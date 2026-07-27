# CLI: `twoliter update`

Refresh `edo.lock.json` to pick up the latest digests for every `kit-image` /
`sdk-image` / `lookaside` node. Equivalent to old `twoliter update`.

## Usage

```
twoliter update [--addr <addr>] [--vendor <vendor>] [--no-fetch]
```

- `--addr <addr>` — refresh a single node instead of the whole lockfile.
- `--vendor <vendor>` — refresh every source in a vendor's namespace.
- `--no-fetch` — resolve tags/versions but don't actually pull the layers.

Currently `src/cmd/update.rs` calls edo's lock refresh; the spec fills in
what nodes have to participate.

## Behavior

Delegates to edo's `Context::update_lock` (or equivalent), which:

1. For every `kit-image` node, re-resolves the image URI (which may be a
   tag like `v0.1.0`) into a digest by calling `HEAD` on the registry.
2. Same for every `sdk-image` node.
3. For `lookaside` nodes, this is a no-op: they're content-addressed by
   SHA-512, and the SHA-512 comes from `package.toml`. If the SHA-512
   changes, that's a source-code change, not an `update`.
4. Writes the resolved digest + resolved URI back to `edo.lock.json`.

## Old-code references

- `old-twoliter/twoliter/src/cmd/update.rs` — the old subcommand (very
  thin).
- `old-twoliter/twoliter/src/project/lock/mod.rs::Lock::create` — the old
  lock-generation logic.
- `old-twoliter/twoliter/src/project/lock/image.rs::ImageResolver` — the
  registry HEAD dance for digest resolution.

## Open questions

- Should `twoliter update` also verify that the resolved digest still
  matches the metadata in the pulled OCI config (kit vendor/name/version
  consistency)? Old twoliter did — see
  `Lock::validate_locked_image_consistency`. Keep this check in the port.
