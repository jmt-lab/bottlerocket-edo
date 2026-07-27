# CLI: `twoliter list`

Enumerate addresses in the project graph, with optional filtering. Useful for
scripting and for discovering what `twoliter build …` targets are available.

## Usage

```
twoliter list                              # every addr
twoliter list --kind rpm                   # only //packages/*
twoliter list --kind kit                   # only //kits/*/*
twoliter list --kind variant-image
twoliter list --format json                # machine-readable
```

`src/cmd/list.rs` already contains a placeholder; this spec fills in what to
emit.

## Behavior

- Default output: one addr per line, sorted, plus its kind in a second
  column.
  ```
  //bottlerocket-sdk                   sdk-image
  //kits/bottlerocket/core-kit         kit-image
  //kits/my-project/my-kit             kit
  //packages/kernel-6.1                rpm
  //variants/aws-k8s-1.29              variant-image
  ```
- `--format json` — a JSON array of `{ addr, kind, artifact_id?, cached? }`
  objects.
- `--cached` — only list addrs whose artifacts are already in storage.

## Old-code references

- No direct equivalent in old twoliter. Cargo-make listed targets, but at
  the level of `Makefile.toml` targets, not project nodes.

## Open questions

- Should `list` also show the resolved digest for each source? Add a
  `--with-digest` flag, off by default (a `HEAD` per source is expensive on
  cold cache).
