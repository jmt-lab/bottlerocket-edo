# `Infra.toml` schema

Configuration file consumed by every `twoliter publish` subcommand that
talks to AWS, VMware, or a TUF repo. Ported verbatim from
`old-twoliter/tools/pubsys-config/src/lib.rs::InfraConfig`.

## Structure

```toml
[aws]
regions = ["us-west-2", "us-east-1", "eu-west-1"]
profile = "bottlerocket"
role = "arn:aws:iam::123456789012:role/BottlerocketPublish"

# Per-region overrides for accounts / roles.
[aws.region.us-east-1]
profile = "bottlerocket-us-east-1"

# Per-role SSM prefixes.
[aws.ssm_prefix]
public = "/aws/service/bottlerocket"

# Optional: launch-permission grants.
[aws.publish]
regions = ["us-west-2"]
user_ids = ["123456789012"]
groups = ["all"]

# ---- Repo ----

[repo.my-project]
metadata_base_url = "https://updates.example.com/{variant}"
targets_url = "https://updates.example.com/{variant}/targets"
root_role_path = "Infra/root.json"

[repo.my-project.signing_keys.root]
type = "local"
path = "Infra/keys/root.pem"

[repo.my-project.signing_keys.targets]
type = "kms"
key_id = "alias/bottlerocket-targets"
region = "us-west-2"

[repo.my-project.expiration_policy]
path = "Infra/repo-expiration-policy.toml"

# ---- VMware ----

[vmware.my-dc]
url = "https://vcenter.example.com/sdk"
datacenter = "DC1"
datastore = "DS1"
folder = "Bottlerocket"
network = "VM Network"
insecure = false

# ---- SSM templates ----

[ssm.templates]
# handlebars-templated key/value pairs; see doc/publish/ssm.md.
"/aws/service/bottlerocket/{{variant}}/{{arch}}/latest/image_id" = "{{image-id}}"
```

The full struct definitions are in
`old-twoliter/tools/pubsys-config/src/lib.rs` (AWS, Repo, KMSKeyConfig,
SigningKeyConfig, RepoExpirationPolicy) and
`old-twoliter/tools/pubsys-config/src/vmware.rs`.

## Loading

`pubsys-config`'s `InfraConfig::from_path` is the canonical loader. Move the
crate to `crates/pubsys-config/`, add a re-export in
`crates/twoliter/src/publish/infra.rs`.

Every publishing subcommand accepts `--infra-config <path>`; when absent,
defaults to `<project>/Infra.toml`.

## Open questions

- Old twoliter re-used `Infra.toml` for both `pubsys` (build-side) and
  runtime tests. New twoliter drops the test path entirely (testsys is
  deprecated), so `Infra.toml` becomes strictly a publishing config.
- Should any of `Infra.toml` merge into `Twoliter.toml`? Recommendation:
  no — publishing config is a shared secret / auth-heavy artifact; keeping
  it in its own file makes it easier to manage separately from the
  buildable project.
