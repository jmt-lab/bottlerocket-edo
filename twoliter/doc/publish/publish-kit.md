# Publish: `kit`

Push a locally-built kit's OCI image (all arches) to a registry.

## Usage

```
twoliter publish kit <addr> <vendor> [--kit-repo <alt-repo>] [--multiplatform]
```

- `<addr>` — a `kit` or `kit-image` addr, e.g. `//kits/my-project/my-kit`.
- `<vendor>` — the twoliter vendor name to publish under. The registry URL
  comes from `Twoliter.toml` `[vendor.<vendor>].registry`.
- `--kit-repo <alt-repo>` — override the repository within the registry
  (defaults to the kit's `name`). Matches `PUBLISH_KIT_REPO` in the old
  Makefile.

## Behavior

Ports `old-twoliter/tools/pubsys/src/kit/publish_kit/mod.rs`:

1. Resolve `<addr>` and locate its artifact in storage. Refuse to publish if
   the artifact isn't there (tell the user to `twoliter build kit …` first).
2. Load the artifact's layers as an OCI-image on disk.
3. For each arch present, push the arch-specific manifest to
   `<registry>/<repo>:v<version>-<arch>`.
4. Compose a multi-arch manifest list at `<registry>/<repo>:v<version>` and
   push it too.
5. Emit a summary: each pushed reference + its digest.

## Environment

Local farm. Uses `ocilot` for push (auth via docker credential helpers).

## Old-code references

- `old-twoliter/tools/pubsys/src/kit/publish_kit/mod.rs::publish_kit`.
- `old-twoliter/twoliter/src/cmd/publish_kit.rs`.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.publish-kit]`.

Already partially wired in `src/cmd/publish.rs::PublishKit`; the rest is
filling in the push logic.

## Open questions

- Multi-vendor publish? A single `twoliter publish kit … …` publishes to one
  vendor. Users who need to mirror to multiple registries run it multiple
  times.
