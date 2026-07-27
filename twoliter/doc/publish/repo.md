# Publish: `repo`

Assemble a signed TUF update repo from a `variant-image` artifact +
optionally a `migrations` artifact. Sign with either a local key or a
KMS-backed key.

## Usage

```
twoliter publish repo <variant>
    [--repo <name>]
    [--variant <v>]
    [--arch <arch>]
    [--release-start-time <ts>]
    [--wave-policy-path <path>]
    [--repo-expiration-policy-path <path>]
    [--kms-key-id <id>]
    [--allow-expired-repo]
    [--outdir <path>]
```

Sub-subcommands:

```
twoliter publish repo refresh          <repo>
twoliter publish repo validate         <repo> [--outdir]
twoliter publish repo check-expirations <repo>
twoliter publish repo fetch-variant    <repo>       # pull previously published bits
```

## Behavior

Ports `old-twoliter/tools/pubsys/src/repo.rs::run`:

1. Load `Infra.toml` `[repo.<name>]`: `metadata-base-url`, `targets-url`,
   signing key config, expiration policy.
2. Load the current repo from disk if present; else initialize a fresh one.
3. Add targets:
   - `os.img`, `data.img`, `boot.ext4.lz4`, `root.ext4.lz4`, `verity.lz4`,
     the boot config, migrations (from the `migrations` artifact if any),
     and the variant manifest.
4. Update `manifest.json` (`update-metadata`'s `Manifest` type) — insert
   an `Update` entry with variant, version, arch, digests, waves.
5. Compute wave times from the wave-policy TOML
   (`twoliter/embedded/waves/default-waves.toml` etc.). Ported to
   `crates/twoliter/data/waves/`.
6. Set expirations (`root.json`, `targets.json`, `snapshot.json`,
   `timestamp.json`).
7. Sign with the configured key (local file or AWS KMS).
8. Write out the updated `metadata/` and `targets/` trees.

## Environment

Local. Uses `tough`, `tough-kms`, `tough-ssm` for signing.

## Configuration

`Infra.toml` `[repo.<name>]`:

- `root_role_path` — path to `root.json`.
- `signing_keys` — `[repo.<name>.signing_keys.<role>]` per role (`root`,
  `targets`, `snapshot`, `timestamp`), each either a local file or a KMS
  key config.
- `expiration_policy` — reference to a policy file.
- `metadata_base_url`, `targets_url` — where the repo will be served.

## Old-code references

- `old-twoliter/tools/pubsys/src/repo.rs` (main).
- `old-twoliter/tools/pubsys/src/repo/{check_expirations,refresh_repo,
  validate_repo,fetch_variant}/mod.rs`.
- `old-twoliter/tools/pubsys/src/repo/env_key_source.rs` — env-var key
  source; keep.
- `old-twoliter/tools/update-metadata/` — the `Manifest`/`Update` schema.
  Keep as `crates/update-metadata/`.
- `old-twoliter/twoliter/embedded/waves/*.toml` — wave policies. Move to
  `crates/twoliter/data/waves/`.
- `old-twoliter/tools/pubsys/policies/repo-expiration/2w-2w-1w.toml` —
  default expiration policy. Move to `crates/twoliter/data/repo-expiration/`.

## Open questions

- Should `publish repo` also embed the SBOM as a target? Old pubsys did not;
  new pubsys probably should, for supply-chain auditability. Optional flag.
- Do we still need `fetch-variant`? Its purpose was to bootstrap a repo from
  a previously-published one during hotfixes. Recommendation: yes, keep.
