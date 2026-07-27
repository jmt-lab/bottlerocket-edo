# Transform specifications

Each file here describes one edo `Transform` kind that twoliter registers with
the `Context` registry alongside `register_core`. The file order matches
implementation priority: earlier files unblock later ones.

| Kind | Status | Purpose |
|---|---|---|
| `rpm` | **Done** — see `src/transform/package.rs` | Build one RPM from a package source tree. |
| `kit` | **Done** — see `src/transform/kit.rs` | Pack a set of RPMs into an OCI image. |
| [`variant-image`](variant-image.md) | Todo | Build a Bottlerocket variant image (`os.img.lz4` etc). |
| [`eif`](eif.md) | Todo | Build a minimal EIF sidecar image. |
| [`kmodkit`](kmodkit.md) | Todo | Build a kernel-module kit. |
| [`migrations`](migrations.md) | Todo | Package migration binaries as an artifact. |
| [`repack-variant`](repack-variant.md) | Todo | Repack an existing variant image with a new SBOM/kits. |
| [`kit-metadata`](kit-metadata.md) | Todo | Emit the JSON metadata blob embedded in kit OCI configs. Currently inlined in the `kit` transform. |
| [`advisory-check`](advisory-check.md) | Todo | Validate advisories against packaged RPM versions. |

All the image-building transforms (`variant-image`, `eif`, `kmodkit`,
`migrations`, `repack-variant`) share a common Rust library —
[`crates/br-image/`](../image-building.md) — that hosts the ported logic
from `partyplanner`, `imghelper`, `guest-images-helper` and every subprocess
wrapper (`sgdisk`, `mkfs.*`, `veritysetup`, `lz4`, `dnf`, `syft`, `grub-install`,
`sbsign`). None of these transforms ship an embedded shell script.

The transform bodies run inside the container by having the twoliter binary
self-invoke through hidden `twoliter internal <step>` subcommands — see
[`../cli/internal-subcommands.md`](../cli/internal-subcommands.md).

Each spec follows a common shape (see `variant-image.md` for the template):

- **Purpose**
- **TOML surface** (the node's schema, and any element attributes)
- **Inputs** (edo `depends`, environment, sources)
- **Outputs** (layers, media types, labels)
- **Environment** (which farm / kind, and what overlays)
- **Algorithm** (step by step; references old code)
- **Unique-id** (what participates in the input hash)
- **Old-code references** (buildsys module, embedded scripts)
- **Open questions**

## Registration

All transforms register in `src/lib.rs::register`, matching the existing pattern:

```rust
registry.register_transform(
    "variant-image",
    Arc::new(async |element, ctx| {
        Ok(Transform::new(VariantImageTransform::new(&element, &ctx).await?))
    }),
);
```

The kind string is the lowercase, hyphen-separated one listed above.
