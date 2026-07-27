# Transform: `kit`  (already implemented)

Pack a set of RPMs into an OCI image whose layers are per-package + a
`repodata` layer, and whose config carries kit metadata as a labeled blob.
Registered as `"kit"` in `src/lib.rs`. Implementation in
`src/transform/kit.rs`.

## TOML surface

```toml
# kits/core-kit/kit.toml
schema-version = "3"

[kit]
name = "core-kit"
vendor = "bottlerocket"      # matches [vendor.<name>] in Twoliter.toml
version = "0.1.0"
```

Included packages are resolved from `Cargo.toml` `[build-dependencies]` in
old twoliter. In new twoliter, they should be explicit in `kit.toml`:

```toml
[kit]
name = "core-kit"
vendor = "bottlerocket"
version = "0.1.0"

packages = [
    "//packages/pkg-a-1.27",
    "//packages/pkg-b",
]
```

## Inputs

- `sdk` — SDK source (used for the `createrepo_c` invocation).
- `packages` — every RPM transform whose output should be included.

## Outputs

An OCI image manifest with:

- One layer per package directory (`Packages/<name>/`).
- One layer for `repodata/`.
- Config blob with label
  `dev.bottlerocket.kit.v3.metadata` = base64(json of kit metadata).

## Environment

Container farm, base = SDK. Overlays: the merged RPM tree from every included
package.

## Algorithm

Currently in `src/transform/kit.rs`:

1. Parse `kit.toml` → `KitOptions`.
2. Collect included-package artifact digests.
3. Stage.
4. Run `rpm2kit` equivalent (currently done in Rust: layer-per-package tar
   generation + `createrepo_c`).
5. Write OCI manifest, config, and layers into the artifact.

## Unique-id

Already implemented; see `KitTransform::get_unique_id`.

## Old-code references

- `old-twoliter/twoliter/embedded/rpm2kit` — the old shell script.
- `old-twoliter/tools/buildsys/src/builder.rs::DockerBuild::new_kit`.
- `old-twoliter/tools/pubsys/src/kit/publish_kit/mod.rs` — the *publisher*,
  which reads the kit artifact and pushes it to a registry (see
  `doc/publish/publish-kit.md`).

## Open questions

- The `kit` transform currently supports only one flat kind of kit. When we
  add `kmodkit` (see `doc/transforms/kmodkit.md`), we'll want to share the OCI
  packing logic — refactor `src/transform/kit.rs` to expose a helper module
  before adding kmodkit.
