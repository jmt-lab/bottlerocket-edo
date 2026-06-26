# Bottlerocket Twoliter/Edo Test Package

This repository is a **proof-of-concept / work-in-progress** for a new
[`twoliter`](https://github.com/bottlerocket-os/twoliter) CLI that wraps
[`edo`](https://github.com/bottlerocket-os/edo) as its build engine.

It currently implements:

- The `PackageBuildTransform` and `KitTransform` for `edo`.
- `build` and `publish` subcommands for both packages and kits.
- A `justfile` that automates cloning Bottlerocket kit repositories and
  generating their `edo`-compatible configuration files.

The goal is to validate that an `edo`-driven `twoliter` can reproduce the
existing Bottlerocket package and kit build flows end-to-end.

---

## Prerequisites

- A working Rust toolchain (`cargo`).
- [`just`](https://github.com/casey/just) for running the helper recipes.
- `git`, `bash`, and standard Unix utilities.
- `tomlq` (`tq`), which can be installed via `just setup`.
- A container registry you can push to if you intend to test `publish`
  (for example, a local `registry:2` running on `localhost:5000`).

---

## Building twoliter

You can build the CLI manually:

```sh
cd twoliter && cargo build --release
```

You usually don't need to do this directly — the `bin/twoliter` wrapper
script will detect a missing binary and compile it for you on first use.

---

## Generating kit configuration

The `justfile` automates the two steps needed to turn an existing
Bottlerocket kit repository into a layout this PoC can build:

1. Cloning the upstream kit repository.
2. Generating the `twoliter.toml` / `edo`-compatible files for it.

By default the recipes clone from the production `bottlerocket-os`
GitHub organization. To use a fork instead, edit the first line of the
`justfile` and replace `bottlerocket-os` with your GitHub username or
org.

### 1. Clone a kit repository

```sh
just clone [kit]
```

Example:

```sh
just clone bottlerocket-kernel-kit
```

This places the upstream repository under `repos/<kit>/`.

### 2. Sync (generate config) for a kit

```sh
just sync [kit]
```

This recalculates SHA-256 digests for every remote source so that the
generated config is compatible with `edo`'s content-addressed model.
It can take several minutes on first run.

> **Note:** Neither this `justfile` nor `edo` currently support the
> Bottlerocket lookaside cache, so all sources are fetched and hashed
> directly from their origin URLs.

### Known generation quirks

Generation is best-effort and a few packages currently need manual
touch-ups after `just sync`:

- **`linux-firmware`** (in the kernel kit) does not get its remote
  sources auto-detected — you'll need to add them manually to its
  `package.toml`.
- Some **Go packages** don't get a complete `go-module` definition.
  For example, `soci-snapshotter` requires you to define both the
  repository root module (`.`) and the `cmd` module, renamed:
  `cmd@cmd.tar`.

In general, if a package fails to build right after sync, the first
place to look is the generated `package.toml` for missing or
mis-detected sources.

---

## Building packages and kits

Once the kit has been cloned and synced, you can drive builds through
the `bin/twoliter` wrapper.

### Build a single package

```sh
bin/twoliter build package [package] -a arch=[arch]
```

Example:

```sh
bin/twoliter build package kernel-6.2 -a arch=x86_64
```

### Build a full kit

```sh
bin/twoliter build kit [kit] -a arch=[arch]
```

Example:

```sh
bin/twoliter build kit bottlerocket-kernel-kit -a arch=x86_64
```

---

## Publishing a kit

After a successful kit build, you can publish the resulting OCI
artifact to a registry:

```sh
bin/twoliter publish kit [kit] [version] [full-uri]
```

- `version` is the kit version **without** a leading `v`.
- `full-uri` is the destination reference, including registry, repo,
  and tag.

Example (publishing to a local registry):

```sh
bin/twoliter publish kit bottlerocket-kernel-kit 6.2.0 \
    localhost:5000/bottlerocket-kernel-kit:v6.2.0
```

---

## Repository layout

| Path        | Purpose                                                          |
| ----------- | ---------------------------------------------------------------- |
| `twoliter/` | Source for the PoC `twoliter` CLI (Rust).                        |
| `bin/`     | Wrapper script that builds + invokes the CLI.                    |
| `justfile`  | Recipes for cloning kit repos and generating `edo` config.       |
| `repos/`    | Cloned upstream kit repositories (created by `just clone`).      |
| `kits/`     | Generated, `edo`-compatible kit configurations (from `just sync`). |

---

## Status

This is exploratory code. Interfaces, generated file layouts, and
command semantics are expected to change as the `twoliter` + `edo`
integration matures.

## Known Issues

There are a few issues that are known:

* Frequently when doing a cold fetch of all the remote sources twoliter/edo will error out on the transition from fetch -> transform build. You simply just rerun the command and it should move on from this
* If a package fails the tui may not be immediately responsive due to a bug with events overloading the tui window. Wait for a while and the prompt should work.
* Ctrl+C does not always work right now (sadly)

If you find any other issues please file a github issue on awslabs/edo if it seems to be an orchestration issue, otherwise file on this repository fo rnow.
