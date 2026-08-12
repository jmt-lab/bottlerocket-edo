# Publish: `ami`

Register AMIs in AWS from a `variant-image` artifact: upload the OS + data
images as EBS snapshots, register the AMI, copy to additional regions,
optionally grant launch permissions.

## Usage

```
twoliter publish ami register <variant>
    [--arch <x86_64|aarch64>]
    [--regions <r1,r2,…>]
    [--visibility public|private]
    [--launch-permissions <path>]
    [--ami-output <path>]
    [--allow-clobber]
    [--infra-config <path>]
```

Also includes:

```
twoliter publish ami grant <ami-input>   [--user-ids …] [--groups …] [--regions …]
twoliter publish ami revoke <ami-input>  [--user-ids …] [--groups …] [--regions …]
twoliter publish ami validate <ami-input> [--regions …] [--json]
twoliter publish ami public <ami-input>  [--regions …]
```

Old code has these as separate binaries (`pubsys ami`, `pubsys publish-ami`,
`pubsys validate-ami`) — new twoliter groups them.

**Note on CLI shape.** An earlier draft of this doc used `twoliter
publish ami <variant>` (variant as a positional). The implementation
uses `twoliter publish ami register <variant>` so the four sibling
sub-subcommands (`grant`, `revoke`, `validate`, `public`) sit at a
consistent slot.

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

## Deferred (follow-on work)

- **User-authored `amispec.toml` + handlebars templates.** The old
  pubsys let operators override any AMI-registration field via a
  handlebars-templated TOML file (`--amispec-file`). The MVP builds
  `RegisterImageInput` directly from the parsed `VariantManifest`
  (`crates/twoliter/src/publish/aws/ami/register/build.rs`). Restoring
  the template layer requires porting `crates/amispec/`,
  `crates/serde-templated/`, and `crates/serde-templated-derive/`
  from `old-twoliter/tools/`.
- **`Infra.lock` YAML support.** The MVP loads `Infra.toml` only. The
  lock-file loader ports if / when the lock-file workflow does.
- **`[repo]`, `[vmware]`, `[ssm.templates]` typed schemas.** Preserved
  as opaque `toml::Value` today so an existing operator config parses
  without error; each becomes typed as its subcommand port lands.
- **UEFI-data source for `image-features.uefi-secure-boot` variants.**
  The `variant-image` artifact does not currently carry a uefi-data
  layer, so `register` fails client-side (via a preflight in
  `register_inputs`) with a clear error pointing at this section.
  Landing this feature requires either (a) the transform emitting a
  uefi-data layer with a stable media type that `stage_variant` can
  discover, or (b) a `--uefi-data <path>` flag on the CLI to feed
  one from disk.
