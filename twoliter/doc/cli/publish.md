# CLI: `twoliter publish`

Publish subcommands operate on artifacts that the DAG has already produced.
They read from edo's storage and drive an external service (registry, AWS,
TUF repo). None of them re-enter the scheduler.

The detailed algorithms live under [`doc/publish/`](../publish/); this file
documents the CLI shape and how it dispatches.

## Subcommands

```
twoliter publish kit <addr> <vendor>       [--kit-repo <alt-repo>]
twoliter publish ami <variant>             [--regions …] [--visibility public|private] …
twoliter publish ssm <variant>             [--regions …] …
twoliter publish repo <variant>            …
twoliter publish ova <variant>             …
twoliter publish setup-tuf                 …
```

### `twoliter publish kit`

Currently exists in stub form as `src/cmd/publish.rs::PublishKit`. Reads the
`kit` artifact (or `kit-image` source) at `<addr>` and pushes its OCI
manifest list to `<vendor>`'s registry. Detail in
[`doc/publish/publish-kit.md`](../publish/publish-kit.md).

Old code: `old-twoliter/tools/pubsys/src/kit/publish_kit/mod.rs` and
`old-twoliter/twoliter/src/cmd/publish_kit.rs`.

### `twoliter publish ami`

Registers AMIs from a `variant-image` artifact into one or more AWS regions.
Detail in [`doc/publish/ami.md`](../publish/ami.md).

Old code: `old-twoliter/tools/pubsys/src/aws/ami/mod.rs`,
`aws/publish_ami/mod.rs`, `aws/validate_ami/mod.rs`,
`aws/ami/register/mk_amispec.rs`.

### `twoliter publish ssm`

Writes SSM parameters pointing at the AMIs. Includes `promote-ssm` and
`validate-ssm` as sub-subcommands. Detail in
[`doc/publish/ssm.md`](../publish/ssm.md).

Old code: `old-twoliter/tools/pubsys/src/aws/ssm/`,
`aws/promote_ssm/mod.rs`, `aws/validate_ssm/mod.rs`.

### `twoliter publish repo`

Assembles a TUF-signed update repo from the `variant-image` + `migrations`
artifacts. Detail in [`doc/publish/repo.md`](../publish/repo.md).

Old code: `old-twoliter/tools/pubsys/src/repo.rs` plus
`repo/{check_expirations,fetch_variant,refresh_repo,validate_repo}/mod.rs`.

### `twoliter publish ova`

Uploads an OVA to VMware. Detail in [`doc/publish/ova.md`](../publish/ova.md).

Old code: `old-twoliter/tools/pubsys/src/vmware/upload_ova/mod.rs`.

### `twoliter publish setup-tuf`

Bootstraps TUF signing keys and root. Detail in
[`doc/publish/setup-tuf.md`](../publish/setup-tuf.md).

Old code: `old-twoliter/tools/pubsys-setup/src/main.rs`.

## Arguments common to publishing

- `--infra-config <path>` — path to `Infra.toml` (the pubsys config file).
  Required for AWS/VMware subcommands, optional for `publish kit`.
- `--dry-run` — for AWS-side subcommands, describe changes but don't apply.

## Old-code references

- `old-twoliter/tools/pubsys/src/main.rs` — top-level pubsys command
  structure. Each subcommand becomes a `twoliter publish <name>` subcommand.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.ami]`, `[tasks.ssm]`,
  `[tasks.repo]`, `[tasks.upload-ova]`, `[tasks.publish-kit]` — target
  wiring.

## Open questions

- Should `twoliter publish` accept an addr (`//variants/aws-k8s-1.29`) or a
  human-readable name (`aws-k8s-1.29`)? Recommendation: both, with the addr
  form being canonical. Old twoliter used names; new twoliter should accept
  either.
- Should publishing walk the lockfile? Yes: publishing kit `v1.42.0` and
  publishing kit `v1.42.1` must resolve different digests, and the resolved
  digest is what edo's lock records.
