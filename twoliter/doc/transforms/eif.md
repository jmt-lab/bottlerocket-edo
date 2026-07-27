# Transform: `eif`

Build a minimal Nitro Enclaves EIF sidecar image plus its associated bootable
disk image and bare kernel. Replaces the `rpm2eif` embedded script + the
`eif-builder` binary as a **single Rust transform** that links both.

## TOML surface

```toml
# variants/hello-eif/variant.toml   (or an aux node under an existing variant)
schema-version = "3"

[eif]
name = "hello-eif"
version = "0.1.0"
arch = "aarch64"

kernel-cmdline = "reboot=k panic=30 pci=off nomodules random.trust_cpu=on"
memory-bytes = 536870912
cpus = 2
pcie-flags = "0x0240"       # optional; hex

os-image-size-gib = 0        # 0 = tight-fit, else pad
```

EIF is constrained. `br-image::layout::validate_eif_features` enforces:
- `first-party-stack` **must** be false.
- `erofs-root-partition` **must** be true.
- `encrypted-storage`, `uefi-secure-boot`, `in-place-updates`,
  `xfs-data-partition` **must** all be false.

These are the same rejection rules the old `rpm2eif` script encoded in its
argument parser. See `old-twoliter/twoliter/embedded/rpm2eif` (usage block +
guard section).

## Inputs

- `sdk` — SDK source.
- `packages` — set of RPM transforms that make up the sidecar rootfs.

No tools-layer dependency — same as `variant-image`.

## Outputs

Single artifact:

| Layer | File | Media type |
|---|---|---|
| 0 | `{name}.eif` | `application/vnd.aws.enclave.eif` |
| 1 | `{name}-disk.img` | `application/vnd.bottlerocket.image.eif-disk` |
| 2 | `{name}-kernel` | `application/vnd.bottlerocket.image.eif-kernel` |
| 3 | `manifest.json` | `application/vnd.bottlerocket.image.eif-manifest+json` |

The disk image is a GPT with a ROOT-A (erofs) + HASH-A (verity) pair. Layer
2 is byte-identical to what the EIF embedded (flat PE `Image` on aarch64;
uncompressed `vmlinux` on x86_64).

## Environment

Container farm, base = SDK. Overlays = staged RPMs. Same shape as
`variant-image`.

## Algorithm

The transform is essentially `variant-image`'s pipeline restricted to
"single-bank, erofs root + verity, no data partition, no secure boot",
followed by an EIF assembly step:

```rust
async fn transform(&self, ctx: Context, log: Log, storage: Storage) -> TransformStatus {
    let opts = self.opts.clone();
    br_image::layout::validate_eif_features(&opts.image_features)?;

    let staged = self.stage_inputs(&storage).await?;
    let env = ctx.get_environment(&self.farm).await?;

    // Inside the container, the twoliter internal subcommand does:
    //   a. eif_partition_layout(...)                           [partyplanner]
    //   b. sgdisk on the disk image with ROOT-A + HASH-A only  [partyplanner]
    //   c. mkfs.erofs for ROOT-A                                [imghelper]
    //   d. stage_root(package_dir, root_mount) [minimal]        [imghelper]
    //   e. generate_verity_root(root_image, hash_image)         [imghelper]
    //   f. extract_vmlinux(root_mount, arch)                    [rpm2eif]
    //         → flat kernel bytes (arch-dependent)
    //   g. eif_builder::build(kernel_bytes, cmdline, mem, cpus,
    //                         pcie_flags, arch, versions)      [eif-builder]
    //         → writes {name}.eif directly
    //   h. write manifest.json                                   [metadata]

    env.run(build_eif_cmd(&staged)).await?;
    self.collect_outputs(&staged, &storage).await
}
```

`eif-builder` (currently `old-twoliter/tools/eif-builder/`) becomes
`crates/eif-builder/` and is linked directly by the twoliter binary — no
subprocess. That crate already exposes `build_eif(kernel, cmdline, output,
mem, cpus, pcie_flags, arch, build_tool_version, kernel_version)` as a
library function; the `main.rs` of the old binary goes away.

`extract_vmlinux` is worth breaking out because it is arch-dependent:

- **x86_64**: find `/boot/vmlinuz`, run `extract-vmlinux` (a script from the
  linux kernel source tree; SDK ships it) or replicate its logic in Rust
  (decompress the compressed section from the ELF).
- **aarch64**: find the flat `Image` (or `Image.gz`, or the EFI zboot
  wrapped variant), unwrap and decompress as needed to produce a flat
  PE arm64 `Image`.

Ports to `br_image::eif::extract_vmlinux(root_mount, arch) -> Vec<u8>`.

## Unique-id

```
sha256(
    "eif",
    node.arch,
    resolved_sdk_digest,
    for pkg in packages.sorted_by_addr: pkg.artifact_id,
    kernel_cmdline,
    memory_bytes,
    cpus,
    pcie_flags,
    os_image_size_gib,
    twoliter_binary_digest,     // br-image + eif-builder live in the binary
)
```

## Old-code references

- `old-twoliter/twoliter/embedded/rpm2eif` — the script. Full port lands in
  `src/transform/eif.rs` + `crates/br-image/src/eif.rs`.
- `old-twoliter/tools/eif-builder/` — becomes `crates/eif-builder/`. Keep
  as a library. Delete the `main.rs` binary.
- `old-twoliter/twoliter/embedded/imghelper` — verity, mkfs, erofs sizing
  helpers. Shared with `variant-image` via `br-image`.
- `old-twoliter/twoliter/embedded/partyplanner::set_eif_partition_sizes` —
  ports to `br_image::partition::eif_partition_layout`.

## Open questions

- Should EIF and variant-image share the same `variant-image` transform kind
  with a `format: eif` variant? Recommendation: **two transforms**. EIF has
  three output artifact classes (an EIF, a disk, a kernel), a distinct set
  of accepted features, and its own internal ordering (extract kernel →
  wrap into EIF). Sharing the transform kind buys nothing.
- Does `extract-vmlinux` (the kernel-source-tree script) come in from the
  SDK image? Currently yes. If we want to drop that dependency, port it to
  Rust — the algorithm is documented in the linux tree
  (`scripts/extract-vmlinux`). Not blocking.
