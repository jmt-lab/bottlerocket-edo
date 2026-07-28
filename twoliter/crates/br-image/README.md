# br-image

Shared primitives for building Bottlerocket disk images. Ports of the
old `partyplanner`, `imghelper`, and `guest-images-helper` shell scripts
plus the buildsys manifest types (`ImageFormat`, `PartitionPlan`,
`ImageLayout`, `ImageFeature`) into a plain Rust library.

`br-image` runs against an `edo::environment::Vfs` handle. Every I/O
and subprocess operation is dispatched inside the transform's sandboxed
environment rather than against the host filesystem. Consumers (the
`variant-image`, `eif`, `kmodkit`, `migrations`, and `repack-variant`
transforms in `crates/twoliter/`) construct a `Vfs` from their
transform's `Environment`, `Id`, and `Log`, and pass it to `br-image`
functions.

See [`doc/image-building.md`](../../doc/image-building.md) for the
design spec.

## Follow-on work

- Transform bodies (`crates/twoliter/src/transform/{variant,eif,repack,kmodkit,migrations}.rs`)
  that call into `br-image`.
- `twoliter internal build-variant` subcommand family that runs
  `br-image` inside the container.
- Integration tests (gated by `BR_IMAGE_INTEGRATION=1`) that exercise
  real `veritysetup`, `mkfs.*`, and `sgdisk` against an SDK container.
- Stdin-piping helper for `debugfs -w -f -` once edo's `Vfs` supports
  streamed stdin.
