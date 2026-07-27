# CLI: `twoliter pcrsys`

Predict TPM Platform Configuration Register (PCR) values for a Bottlerocket
image without booting it. Direct port of the old `pcrsys` binary as a
twoliter subcommand.

Not a transform. PCR prediction is a read-only inspection of an already-built
image or an already-published AMI — no need for it to participate in the
build DAG.

## Subcommands

```
twoliter pcrsys disk --image <path> --efi-vars <path> [--platform aws|vmware|metal]
twoliter pcrsys ami  --ami-id <id>  [--region <r>] [--profile <p>]
```

Same shape as old `pcrsys` — see `old-twoliter/tools/pcrsys/src/main.rs`.

## Behavior

### `disk`

1. Read `efi-vars` JSON (typically produced by the variant build).
2. Open the disk image; find primary GPT, ESP, boot/root partitions.
3. Extract shim, grub, vmlinuz, grub.cfg, bootconfig, root partition UUID.
4. Compose a `PcrContext` and evaluate PCRs 0, 1, 2, 3, 4, 5, 6, 7, 9, 10,
   11, 12, 13, 14, 15.
5. Emit JSON: `{ "pcrs": { "0": "sha256:…", "1": "…", … } }`.

### `ami`

Same, but download the snapshot via EBS `coldsnap` and extract EFI vars from
AMI metadata first.

## Integration with `twoliter build`

`twoliter pcrsys disk --image build/…/os.img.lz4 …` operates on the raw
image. To make this less awkward, add a helper:

```
twoliter pcrsys variant <variant>  [--arch <arch>]
```

which resolves the addr, unpacks the `os.img.lz4` layer to a tempdir, and
runs the disk prediction against it. Convenience only.

## Old-code references

- `old-twoliter/tools/pcrsys/src/main.rs`.
- `old-twoliter/tools/pcrsys/src/predict.rs` (core algorithm).
- `old-twoliter/tools/pcrsys/src/pcrs/pcr*.rs` (one file per PCR).
- `old-twoliter/tools/pcrsys/src/aws/ami.rs` (AWS integration).
- `old-twoliter/tools/pcrsys/src/diskfs.rs`, `gpt.rs`, `efi/`, `pe.rs`,
  `parsers/` — all supporting infra.

## Open questions

- The old `pcrsys` had its own `Cargo.toml` and produced its own binary. Keep
  it as `crates/pcrsys/` (a library) plus twoliter subcommand wiring? Yes —
  the crate size (10+ modules, non-trivial parsers) makes it worth keeping
  separate.
- Should PCR prediction be verified against real hardware in CI? Out of
  scope for the port.
