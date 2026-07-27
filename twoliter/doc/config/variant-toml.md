# `variant.toml` schema

Per-variant configuration. One per directory under `variants/`. **Not yet
implemented** in `src/config/`.

## Shape

```toml
schema-version = "3"

[variant]
name = "aws-k8s-1.29"
version = "0.1.0"          # optional; defaults to release-version
arch = "x86_64"            # optional; scheduler default is host arch

# Kits this variant includes (references to //kits/… nodes).
[[variant.kit]]
addr = "//kits/bottlerocket/core-kit"

[[variant.kit]]
addr = "//kits/my-project/extra-3-kit"

# Extra individual packages, on top of what the kits provide.
[variant.included-packages]
packages = ["pkg-e"]

# Kernel parameters passed to the built image.
kernel-parameters = ["quiet", "console=ttyS0"]

# Image format / layout.
[variant.image]
format = "raw"                              # raw|vmdk|qcow2
partition-plan = "split"                    # split|unified
os-image-size-gib = 2
data-image-size-gib = 1
os-image-publish-size-gib = 8
data-image-publish-size-gib = 20

# Feature toggles. Match old ImageFeature enum in
# old-twoliter/tools/buildsys/src/manifest.rs.
[variant.image.features]
grub-set-private-var = true
systemd-networkd = true
uefi-secure-boot = true
xfs-data-partition = true
in-place-updates = false
encrypted-storage = false
erofs-root-partition = false
first-party-stack = true

# Guest images embedded in this (host) variant.
[[variant.guest-image]]
addr = "//variants/hello-eif/eif"
install-path = "/usr/share/bottlerocket/enclaves"

# Optional: opt into an EIF sidecar.
[variant.eif]
enabled = false
# ... same fields as doc/transforms/eif.md.

# Optional: opt into migrations.
[variant.migrations]
enabled = false
packages = ["//packages/migration-foo"]

# Optional: opt into repack.
[variant.repack]
enabled = false
```

## Fields

### Required

- `schema-version = "3"`.
- `[variant].name`.
- At least one `[[variant.kit]]` **or** at least one entry in
  `included-packages`.

### Optional

Everything else defaults sensibly. Notably:

- `arch` — defaults to host arch. Can be overridden at build time by
  `twoliter build variant … --arch <arch>`.
- `[variant.image]` — has defaults matching
  `manifest::ImageLayout::default()`.
- `[variant.image.features]` — every feature defaults to `false` except
  `first-party-stack` which defaults to `true`.

## Loading

Add `variant.rs` to `src/config/`. `Variant::to_edo(namespace)` emits:

- One `variant-image` transform node at `//variants/<name>`.
- Optionally, one `eif` transform node at `//variants/<name>/eif`.
- Optionally, one `migrations` transform node at
  `//variants/<name>/migrations`.
- Optionally, one `repack-variant` transform node at
  `//variants/<name>/repack`.

## Old-code references

- `old-twoliter/tools/buildsys/src/manifest.rs::BuildVariant`,
  `ImageLayout`, `ImageFeature`, `PartitionPlan`, `SupportedArch`,
  `ImageFormat`, `resolved_image_layout`, `validate_image_features`.
- `old-twoliter/tests/projects/local-kit/variants/hello-ootb/Cargo.toml` —
  example old format.

## Open questions

- Should EIF / migrations / repack be inlined into `variant.toml` (as
  above), or split into their own files under the variant dir? Two files
  are cleaner when a variant has *lots* of aux artifacts. Recommendation:
  inline in v3, split later if it gets unwieldy.
- How to express "this variant is only for aarch64"? Add a top-level
  `arches = ["aarch64"]` at the same level as `name`.
