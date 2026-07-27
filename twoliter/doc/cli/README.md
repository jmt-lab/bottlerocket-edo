# Twoliter CLI specifications

The twoliter binary is a thin front-end over an edo `Context`. Every
subcommand either (a) drives edo's `Scheduler`, or (b) reads from edo's
storage and performs an out-of-DAG action.

Current `src/cmd/` already provides the DAG-driving skeleton:

| Subcommand | File | Status |
|---|---|---|
| `build` | `src/cmd/build.rs` | Basic — has `package` and `kit` subcommands. |
| `checkout` | `src/cmd/checkout.rs` | Basic — pulls an artifact out of storage. |
| `list` | `src/cmd/list.rs` | Basic — enumerates addrs. |
| `prune` | `src/cmd/prune.rs` | Basic — clears storage. |
| `publish` | `src/cmd/publish.rs` | Basic — only `kit` today. |
| `run` | `src/cmd/run.rs` | Basic — arbitrary addr build. |
| `update` | `src/cmd/update.rs` | Basic — refreshes `edo.lock.json`. |

This spec directory covers each one plus everything old twoliter shipped as a
subcommand or as a `cargo make` target.

| Spec | What it covers |
|---|---|
| [`build.md`](build.md) | `twoliter build package \| kit \| variant \| eif \| kmodkit \| migrations \| repack-variant`. |
| [`publish.md`](publish.md) | `twoliter publish kit \| ami \| ssm \| repo \| ova`, plus `twoliter publish setup-tuf`. See also `doc/publish/`. |
| [`checkout.md`](checkout.md) | `twoliter checkout <addr> [--to <dir>]`. |
| [`update.md`](update.md) | `twoliter update` (lockfile refresh). |
| [`fetch.md`](fetch.md) | `twoliter fetch` (pull sources into cache; alias for `checkout` on source addrs). |
| [`list.md`](list.md) | `twoliter list` (enumerate addrs; describe artifacts). |
| [`prune.md`](prune.md) | `twoliter prune` (cache eviction). |
| [`run.md`](run.md) | `twoliter run <addr>` (drive the DAG for an arbitrary addr). |
| [`check.md`](check.md) | `twoliter check advisories`, `twoliter check lockfile`, etc. |
| [`debug.md`](debug.md) | `twoliter debug` (dump tools, dump graph, dump lockfile). |
| [`pcrsys.md`](pcrsys.md) | `twoliter pcrsys disk \| ami` (PCR prediction). |
| [`internal-subcommands.md`](internal-subcommands.md) | `twoliter internal …` (hidden; used by container-side transform bodies). |

## Global flags

Already present in `src/main.rs::Args`:

- `--debug` / `--trace` — log level.
- `--config <path>` — override the project's `Twoliter.toml` path.
- `--storage <dir>` — edo storage root (defaults to `.edo/`).
- `--event-log <path|none|default>` — where to write JSONL build events.

Additional flags to add:

- `--project-path <dir>` — for compatibility with old twoliter; equivalent to
  `--config <project>/Twoliter.toml`.
- `--infra-config <path>` — for `publish` subcommands only; passthrough to
  `Infra.toml`.
- `--no-lock` — for `build` / `run`; ignore `edo.lock.json` for this run.
- `--arch <x86_64|aarch64>` — for `build` / `checkout`; defaults to host arch.

## Preflight

Old twoliter had a `preflight()` step that checked for `gzip` and `lz4`. New
twoliter's preflight lives in `main` before `create_context`, and checks:

- `gzip` and `lz4` on `PATH` (for lz4/lz4-decompress operations old
  scripts run inside the container — the container has them, but the host
  needs them for `checkout`).
- `docker` or `podman` on `PATH` if the project's `//container` farm is
  `container`-kind.
- No `Twoliter.toml` v1 or v2 at the project path (old-format hard-fail).

The port point is `old-twoliter/twoliter/src/preflight.rs`.
