# Publish: `ami`

Register AMIs in AWS from a `variant-image` artifact: upload the OS + data
images as EBS snapshots, register the AMI, copy to additional regions,
optionally grant launch permissions.

## Usage

```
twoliter publish ami <variant>
    [--arch <x86_64|aarch64>]
    [--regions <r1,r2,…>]
    [--visibility public|private]
    [--launch-permissions <path>]
    [--ami-output <path>]
    [--allow-clobber]
```

Also includes:

```
twoliter publish ami grant <ami-input>   [--user-ids …] [--groups …] [--regions …]
twoliter publish ami revoke <ami-input>  [--user-ids …] [--groups …] [--regions …]
twoliter publish ami validate <ami-input> …
twoliter publish ami public <ami-input>  …
```

Old code has these as separate binaries (`pubsys ami`, `pubsys publish-ami`,
`pubsys validate-ami`) — new twoliter groups them.

## Behavior

Ports `old-twoliter/tools/pubsys/src/aws/ami/mod.rs`:

1. Load the variant's `manifest.json` (produced by the `variant-image`
   transform) and the `os.img` + optional `data.img` layers from storage.
2. For each region:
   1. Upload OS/data via EBS snapshot API (`ebs.rs` in old pubsys).
   2. Wait for snapshot completion.
   3. Register AMI via `mk_amispec.rs` (builds a `RegisterImageInput` from the
      variant manifest, using `crates/amispec/`).
   4. Wait for AMI availability.
   5. Copy to any additional regions.
3. Grant launch permissions per `--launch-permissions` or `Infra.toml`.
4. Write `<ami-output>` with `{ region: { id, arch, name, … }, … }` — the
   format `publish ssm` and `promote-ssm` consume.

## Environment

Local farm. Talks to `ec2`, `ebs`, `sts` via `aws-sdk-*`.

## Configuration

`Infra.toml` `[aws]` section:
- `regions` — default set of regions to publish to.
- `profile` — AWS profile.
- `role` — optional cross-account role.

## Old-code references

- `old-twoliter/tools/pubsys/src/aws/ami/mod.rs` (main).
- `old-twoliter/tools/pubsys/src/aws/publish_ami/mod.rs` (grant/revoke).
- `old-twoliter/tools/pubsys/src/aws/validate_ami/mod.rs` (verify AMIs
  match spec).
- `old-twoliter/tools/pubsys/src/aws/ami/register/mk_amispec.rs` (build the
  register-image call from a variant manifest — nontrivial, ~1000 lines).
- `old-twoliter/tools/pubsys/src/aws/ami/wait.rs` (polling).
- `old-twoliter/tools/pubsys/src/aws/ami/snapshot.rs` (EBS upload).
- `old-twoliter/tools/pubsys/src/aws/ami/launch_permissions.rs`.
- `old-twoliter/tools/amispec/` (register-image spec builder — keep as
  `crates/amispec/`).

## Open questions

- Old pubsys wrote AMI output as a single JSON file. Do we also want to
  update `edo.lock.json` with published AMI IDs? Recommendation: no, keep
  them in a separate `<project>/ami.<variant>.<arch>.json` file. AMIs
  aren't project inputs; they're project outputs.
