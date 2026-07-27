# Publish: `setup-tuf`

Bootstrap the TUF signing infrastructure: fetch/verify a `root.json`,
optionally generate signing keys, wire them into `Infra.toml`.

## Usage

```
twoliter publish setup-tuf
    [--repo <name>]
    [--root-role-path <path>]
    [--root-role-url <url>]
    [--root-role-sha512 <hash>]
    [--allow-expired-repo]
```

## Behavior

Ports `old-twoliter/tools/pubsys-setup/src/main.rs`:

1. Read `Infra.toml` `[repo.<name>]`.
2. Locate `root.json`:
   - If `root_role_path` is set locally and exists, use it.
   - Else if `--root-role-url` + `--root-role-sha512` are provided,
     download and verify.
   - Else fetch from `metadata_base_url` (unauthenticated) — logs a warning
     because this defeats TUF's trust model.
3. If any signing key config is missing:
   - For `local` key type, generate a fresh RSA 2048 key with `tuftool` and
     write to disk.
   - For `kms` type, expect the user to have already created the key.
4. Update `Infra.toml` in place (or write a fragment) with the resolved key
   paths.

## Environment

Local. Shells out to `tuftool` for key generation and root verification.

## Old-code references

- `old-twoliter/tools/pubsys-setup/src/main.rs`.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.publish-setup]`,
  `[tasks.publish-setup-without-key]`.

## Open questions

- `tuftool` is currently a separately-installed binary (Bottlerocket's own
  tool). New twoliter can either shell out (simple, matches old behavior) or
  link `tough` directly (more robust). Recommendation: shell out. `tuftool`
  is battle-tested and matches what operators already run manually.
