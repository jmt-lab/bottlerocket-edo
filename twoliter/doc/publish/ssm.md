# Publish: `ssm`

Write SSM Parameter Store entries pointing at the AMIs produced by `publish
ami`, optionally promoting from one channel to another.

## Usage

```
twoliter publish ssm <variant>
    [--ami-input <path>]
    [--parameter-map <path>]
    [--regions <r1,r2,…>]
    [--check-public]
```

Sub-subcommands:

```
twoliter publish ssm promote  --from-channel <c> --to-channel <c> --variant <v>
twoliter publish ssm validate --parameters <path>
```

## Behavior

Ports `old-twoliter/tools/pubsys/src/aws/ssm/mod.rs`:

1. Read AMI ID map (from `publish ami --ami-output`).
2. Read template file (`Infra.toml` `[ssm.templates]` or a passed
   `--parameter-map`).
3. Render each template into an `SsmKey` + value.
4. Diff against currently-published SSM values.
5. Rate-limited (`DESCRIBE_IMAGES_RATE_LIMIT`) apply the diff.
6. Optional public-AMI namespace check: refuse to publish under a
   `/aws/service/bottlerocket/…` prefix unless the target AMI is public.
7. Emit a report of created/updated/skipped parameters.

## Environment

Local. Talks to `ssm`, `ec2`.

## Old-code references

- `old-twoliter/tools/pubsys/src/aws/ssm/mod.rs` (main).
- `old-twoliter/tools/pubsys/src/aws/ssm/ssm.rs` (API calls).
- `old-twoliter/tools/pubsys/src/aws/ssm/template.rs` (templating).
- `old-twoliter/tools/pubsys/src/aws/promote_ssm/mod.rs`.
- `old-twoliter/tools/pubsys/src/aws/validate_ssm/mod.rs`.
- `old-twoliter/tools/pubsys/policies/ssm/defaults.toml` — default templates.
  Move to `crates/twoliter/data/ssm-defaults.toml`.

## Open questions

- Template variables include `{{arch}}`, `{{variant}}`, `{{image-id}}`, etc.
  Old pubsys expanded them via `handlebars` (also what edo's ScriptTransform
  uses). Keep the same variable set for backward compat with existing
  operator templates.
