# `variant.toml` schema

Per-variant configuration. One per directory under `variants/`.

## Shape

```toml
schema-version = "3"

[variant]
name = "aws-k8s-1.29"
version = "0.1.0"          # optional; defaults to 0.0.0
arch = "x86_64"            # optional; scheduler default is host arch

# Optional: restrict this variant to a subset of arches.
# Absent = any arch.
supported-arches = ["x86_64", "aarch64"]

# Kernel parameters passed to the built image.
kernel-parameters = ["quiet", "console=ttyS0"]

# Extra individual packages, on top of what the project's kits provide.
[variant.included-packages]
packages = ["pkg-e"]

# Image format / layout.
[variant.image]
format = "raw"                              # raw|vmdk|qcow2|eif
partition-plan = "split"                    # split|unified
os-image-size-gib = 2
data-image-size-gib = 1
publish-image-size-hint-gib = 22            # single hint; the loader broadcasts
                                            # this to both `os-image-publish-size-gib`
                                            # and `data-image-publish-size-gib` on
                                            # the transform config.

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

## Kits

`variant.toml` does **not** declare kits. The kit set is defined once at
the project root in `Twoliter.toml`'s `[[kit]]` list and every variant
implicitly receives all of them. A `[[variant.kit]]` block is a hard
parse error (caught by `deny_unknown_fields` on the `Variant` struct).

### Tradeoff worth knowing

Every variant depends on every project kit in the transform graph. Two
practical consequences:

- **Cache invalidation is coarse.** A change to any project kit
  invalidates the rootfs cache for every variant, even variants that
  don't use anything the kit provides.
- **Scheduling is serial-ish.** The scheduler waits on every kit
  before starting any variant.

This is a deliberate simplification of old twoliter, which selected
kits per-variant via Cargo `[build-dependencies]`. The current shape
suits projects with a small kit set that every variant uses. If a
project outgrows this — many kits, variants with disjoint needs — the
fix is to add a narrowing `kits = ["name", ...]` field on `Variant` at
that point. Not implemented today.

## Fields

### Required

- `schema-version = "3"`.
- `[variant].name`.

### Optional

Everything else defaults sensibly. Notably:

- `arch` — defaults to host arch. Can be overridden at build time by
  `twoliter build variant … --arch <arch>`.
- `supported-arches` — allowlist. Absent = any arch. Enforcement lives
  on the transform side (currently a TODO in
  `crates/twoliter/src/transform/variant/options.rs`).
- `[variant.image]` — has defaults matching
  `crate::image::ImageLayout::default()`.
- `[variant.image.features]` — every feature defaults to `false` except
  `first-party-stack` which defaults to `true`.

## Loading

`src/config/variant.rs::Variant::to_edo(namespace, project_kits)` emits:

- `//variants/<name>/rootfs` — `variant-rootfs` transform.
- `//variants/<name>/disk` — `variant-disk` transform.
- `//variants/<name>/grub` — `variant-grub` transform.
- `//variants/<name>` — `variant-image` finalizer.

`project_kits` is the resolved `//kits/<vendor>/<name>` address list
that `config/mod.rs::Config::variant_toml` derives from the project's
`[[kit]]` list before calling `to_edo`.

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
- Old-twoliter's `BuildVariant` also carried `platform`, `runtime`,
  `family`, `flavor` overrides. Not carried over — reintroduce if a real
  project needs them.
