# Transform: `kmodkit`

Build a kernel-module kit: a specialized kit whose contents are kernel modules
compiled against a specific kernel-headers RPM, packaged as an OCI image the
same way a normal kit is. Replaces `rpm2kmodkit` (a 57-line shell script) with
a Rust transform.

## TOML surface

```toml
# kits/my-kmods-kit/kit.toml
schema-version = "3"

[kmodkit]
name = "my-kmods-kit"
vendor = "my-vendor"
version = "0.1.0"
arch = "x86_64"

# The kernel we build against.
kernel-package = "//packages/kernel-6.1"

# Modules to include.
packages = [
    "//packages/kmod-nvidia-open",
    "//packages/kmod-something",
]
```

## Inputs

- `sdk` — SDK source.
- `kernel-package` — a single `rpm` transform providing the kernel headers.
- `packages` — the module RPM transforms.

No tools-layer dependency.

## Outputs

An OCI image manifest, layered per-module (mirroring the `kit` transform's
layer-per-package scheme), plus a `kit-metadata` JSON blob in the OCI
config's labels. Distinct config media type
`application/vnd.bottlerocket.kmodkit.v1+json` so consumers can distinguish.

## Environment

Container farm, base = SDK. Same overlay stack as `kit`.

## Algorithm

Ports the whole `rpm2kmodkit` script logic into
`src/transform/kmodkit.rs`. Because the file is small and mostly OCI packing,
the Rust body reuses the OCI writer already present in
`src/transform/kit.rs::write_dir_layer` (extract into a helper module
`src/transform/oci_pack.rs` first).

```rust
async fn transform(&self, ctx: Context, log: Log, storage: Storage) -> TransformStatus {
    // 1. Read kernel version out of the kernel-package's RPM metadata.
    let kernel_version = self.read_kernel_version(&storage).await?;

    // 2. For each module RPM, verify its `Requires:` matches
    //    `kernel-devel-<kernel_version>`.
    self.verify_kernel_compat(&storage, &kernel_version).await?;

    // 3. Stage each module RPM as its own OCI layer.
    let mut layers = Vec::new();
    for pkg in &self.opts.packages {
        let layer = oci_pack::pack_dir_as_layer(
            pkg_artifact_dir(pkg),
            format!("Packages/{}", pkg.name),
        )?;
        layers.push(layer);
    }
    // Add repodata layer.
    layers.push(oci_pack::pack_repodata(...)?);

    // 4. Emit kit-metadata for a kmodkit.
    let metadata = KitMetadata {
        name: self.opts.name.clone(),
        vendor: self.opts.vendor.clone(),
        version: self.opts.version.clone(),
        kind: KitKind::KmodKit,
        kernel_version: Some(kernel_version),
        packages: self.list_packages(&storage).await?,
        // ...
    };

    // 5. Assemble OCI manifest + config with the metadata label; commit
    //    to storage as the artifact.
    self.write_oci_artifact(layers, metadata, &storage).await
}
```

`br_image::rpm_install::read_kernel_version(rpm_path)` reads the RPM
header (via the `rpm` crate) and extracts `Version` + `Release`.

`br_image::rpm_install::read_requires(rpm_path)` returns the RPM's
`Requires:` list; the compat check just looks for the `kernel-devel-*`
line and compares versions.

## Unique-id

```
sha256(
    "kmodkit",
    kernel_package.artifact_id,
    for pkg in packages.sorted_by_addr: pkg.artifact_id,
    metadata (name, vendor, version, arch),
    twoliter_binary_digest,
)
```

## Old-code references

- `old-twoliter/twoliter/embedded/rpm2kmodkit` — 57 lines of shell; full
  port target.
- `old-twoliter/twoliter/embedded/ocihelper` — old OCI helpers; deleted.
  Replaced by shared Rust helpers under `src/transform/oci_pack.rs`.
- `src/transform/kit.rs::write_dir_layer` — existing OCI-layer writer; the
  kmodkit transform reuses it.

## Open questions

- Should kmodkit be its own transform, or a `kit` transform variant? Both
  produce OCI kit-format images; the differences are (a) the compat check
  against a specific kernel, (b) the extra `kernel-version` field in
  metadata, (c) the distinct media type. Recommendation: **separate
  transform**. The compat check is worth expressing as an explicit input
  edge (`kernel-package` is a first-class dependency, not just a note).
