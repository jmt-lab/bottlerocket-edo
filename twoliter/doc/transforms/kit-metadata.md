# Transform: `kit-metadata`

Emit the JSON metadata blob that gets base64-encoded and stored in the OCI
image config under the label `dev.bottlerocket.kit.v1.metadata` (the label
constant is `KIT_METADATA_LABEL_PREFIX + SUPPORTED_KIT_METADATA_VERSION`, see
`old-twoliter/twoliter/src/project/lock/image.rs:21`).

Right now this logic is inlined in `src/transform/kit.rs`. This spec exists to
formalize it as a standalone transform if we decide the kit transform is doing
too much — otherwise it can stay inlined.

## Why break it out?

Old twoliter had exactly one kit metadata format. New twoliter has to be able
to write metadata for:

- Kits (`kit` transform).
- Kernel-module kits (`kmodkit` transform).
- Possibly guest-image kits.

Each writes the same schema, but with different `kind` and `manifest`
sections. Rather than duplicate the encoding in three transforms, we can
either:

- Extract the metadata schema to a shared crate (`crates/kit-metadata/`) and
  keep the transforms inlining calls to it — **recommended, simplest**.
- Or make `kit-metadata` its own transform whose output layers the JSON blob
  onto its dependency's manifest as an annotation update. Only worth doing if
  we discover metadata schemas that need to change independently of the kit
  contents.

## Metadata schema (as an OCI label value)

```json
{
  "name": "my-kit",
  "vendor": "my-vendor",
  "version": "0.1.0",
  "kind": "kit" | "kmodkit" | "kernel-module-kit",
  "sdk": { "name": "…", "vendor": "…", "version": "…" },
  "kits": [ { "name": "…", "vendor": "…", "version": "…" }, … ],
  "packages": [
    { "name": "pkg-a", "version": "0.1.0-…", "arch": "x86_64",
      "requires": [{"name":"pkg-b", "version":"0.1.0-…"}, …] },
    …
  ],
  "kernel-version": "6.1.42"   // kmodkit only
}
```

The label prefix is `dev.bottlerocket.kit.` (constant in
`old-twoliter/twoliter/src/project/lock/image.rs`). The exact schema version
label is `dev.bottlerocket.kit.v3.metadata`. New twoliter keeps `v3` for
compatibility with existing consumers (see
`old-twoliter/twoliter/src/compatibility.rs`).

The blob is base64-encoded (standard, padded) before being stored as a label
value, because OCI labels are strings.

## Consumers

- `twoliter update` / `edo update` — reads the label to derive the transitive
  package set of an external kit.
- `twoliter publish repo` — assembles the update manifest.
- `twoliter checkout <kit-addr>` — writes the metadata alongside the RPMs.

## Old-code references

- `old-twoliter/twoliter/src/project/lock/image.rs::EncodedKitMetadata` — the
  encode/decode logic.
- `old-twoliter/twoliter/src/compatibility.rs` — the version constant.
- `old-twoliter/tools/buildsys/src/manifest.rs::ExternalKitMetadataView` —
  what the metadata is deserialized *into*.

## Open questions

- Should we bump to `v4` and drop the old label format? Recommendation: no,
  keep `v3` so external kits shipped by old twoliter continue to work
  unmodified.
- Should we sign the metadata? Currently unsigned. Out of scope.
