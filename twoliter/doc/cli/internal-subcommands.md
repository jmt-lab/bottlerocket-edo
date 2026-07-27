# CLI: `twoliter internal …`

Hidden subcommands that the twoliter binary calls **on itself** inside a
container environment. Not user-facing, not stable — the interface can
change with any release because both sides of the interface ship in the same
binary.

The whole existence of `twoliter internal …` is a consequence of the
"delete the shell scripts, do everything in Rust" plan
(see [`../embedded-scripts.md`](../embedded-scripts.md)):

- `variant-image`, `eif`, `kmodkit`, `migrations`, and `repack-variant`
  transforms need to run Rust code that manipulates a container-side
  filesystem (partition tables, dm-verity, dnf install, syft).
- Rather than ship a standalone `br-image-cli` binary in the SDK, the
  twoliter binary itself is staged as an overlay layer in the container.
  Inside the container, it exposes `twoliter internal <step> …` so the
  transform can invoke it as an entrypoint.

The overhead is one small overlay layer per transform run — the twoliter
binary is ~a few MiB stripped. Trade-off: no additional artifact to build,
publish, or version-skew.

## Subcommands

```
twoliter internal build-variant  <staged-input-dir> --output-dir <dir>  [flags…]
twoliter internal build-eif      <staged-input-dir> --output-dir <dir>  [flags…]
twoliter internal build-kmodkit  <staged-input-dir> --output-dir <dir>  [flags…]
twoliter internal build-migrations <staged-input-dir> --output-dir <dir>
twoliter internal repack-variant <staged-input-dir> --output-dir <dir>  [flags…]
```

Each takes:

- Positional `<staged-input-dir>` — a directory the parent transform
  populated with the RPMs, kits, guest images, and config JSON needed for
  this run. Layout is documented per subcommand in the transform's `.md`.
- `--output-dir` — where to write output artifacts. The parent transform
  reads them from here and stages them as OCI layers.
- Flags encoding whatever the old shell script took as command-line args
  (see the old `rpm2img` / `rpm2eif` argument parsers).

## Why not `unstable-`, `--experimental`, etc.

Rejected alternatives:

- **A separate `br-image-cli` binary** in the SDK. Cleaner separation but
  adds an SDK dependency that has to move in lockstep with twoliter.
- **`twoliter build variant --in-container`**. Muddies the user-facing CLI
  with a flag that only ever makes sense inside a specific transform. And
  makes hiding the args harder — the flags for `internal build-variant` are
  legion (every image feature toggle, every partition size, every path).
- **A separate `bottlerocket-image-build` binary** in the twoliter workspace.
  Nice to have, but adds a build target with no external consumers. If
  operators ever need to run it outside twoliter, we can promote it later.

Recommendation stays with `twoliter internal …` — one binary, no extra
artifact, hidden from `--help` via clap's `hide` attribute.

## Hidden from help

Clap's `#[command(hide = true)]` on the `internal` subcommand and on every
subcommand under it. `twoliter --help` never lists them. `twoliter internal
--help` still works if you know to look.

## Argument stability

Zero. The transform on the host side and the internal command inside the
container are always the same version of the twoliter binary — the transform
stages the current binary as an overlay layer at prepare time. There is no
"twoliter build variant against twoliter internal from a different release"
scenario.

## Testing

Each `internal` subcommand gets an integration test that runs it against a
fixture staged input dir, without going through the transform machinery.
This lets us exercise `br-image` code paths without spinning up a full DAG.

## Old-code references

- No direct equivalent in old twoliter. The old model was
  `${TWOLITER_TOOLS_DIR}/rpm2img …` from inside the container; twoliter
  itself never ran inside the container. New model runs the *same* twoliter
  binary on both sides.
- `old-twoliter/tools/buildsys/src/main.rs::build_{package,kit,variant}` —
  in some sense the "internal commands" of old twoliter, invoked by the
  `Makefile.toml` inside the container. New `twoliter internal build-variant`
  is the direct spiritual successor.
