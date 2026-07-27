# Publish specifications

Each file here describes an out-of-DAG publishing path. They all follow the
same pattern:

1. Look up an artifact by addr in edo's storage.
2. Read the artifact's layers.
3. Drive an external service (registry, AWS, VMware, TUF signer).
4. Emit a report.

None of them re-enter the scheduler. They can be run at any time as long as
the artifact they consume exists in storage.

| Spec | What it drives |
|---|---|
| [`publish-kit.md`](publish-kit.md) | Push a kit OCI image to a registry. |
| [`ami.md`](ami.md) | Register + publish AMIs from a variant image. |
| [`ssm.md`](ssm.md) | Write SSM parameters pointing at AMIs. |
| [`repo.md`](repo.md) | Assemble + sign a TUF update repo. |
| [`ova.md`](ova.md) | Upload an OVA to a VMware endpoint. |
| [`setup-tuf.md`](setup-tuf.md) | Bootstrap TUF signing keys and root.json. |
| [`infra-toml.md`](infra-toml.md) | Schema for `Infra.toml` (shared by all AWS + VMware + TUF subcommands). |

## Configuration

All AWS-side publishing reads `Infra.toml` for regions, accounts, key configs.
Schema is unchanged from
`old-twoliter/tools/pubsys-config/src/lib.rs::InfraConfig`. Ported to
`crates/twoliter/src/publish/infra.rs`.

`--infra-config <path>` is a global flag on every `publish` subcommand.

## Crates involved

Most of these live under `crates/` after the port:

- `crates/pubsys-config/` — schema for `Infra.toml`.
- `crates/update-metadata/` — TUF manifest schema for `publish repo`.
- `crates/amispec/` — AMI registration spec for `publish ami`.
- `crates/parse-datetime/` — expiration date parsing for `publish repo`.
- `crates/bottlerocket-variant/` — variant name parsing for `publish ami/ssm`.
- `crates/error-utils/` — AWS SDK error wrappers.
