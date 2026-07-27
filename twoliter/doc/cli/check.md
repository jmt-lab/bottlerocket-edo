# CLI: `twoliter check`

Static and semi-static checks that don't produce build artifacts. This is the
home for old cargo-make targets like `check-fmt`, `check-lints`,
`check-clippy`, `check-shell`, `check-golangci-lint`, `check-migrations`,
`check-licenses`, `check-cargo-version`, `check-repo-expirations`,
`validate-boot-config`, `validate-kits`.

## Subcommands

```
twoliter check advisories                  # cross-check advisories vs current RPMs
twoliter check lockfile                    # verify edo.lock.json is up to date
twoliter check kits                        # verify locked kits pull + validate metadata
twoliter check licenses                    # cargo-deny wrapper
twoliter check migrations                  # migrations spec validation
twoliter check boot-config                 # bootconfig parser test
twoliter check repo-expirations            # TUF repo expiration lookahead
twoliter check fmt | lints | clippy | shell | golangci-lint
                                           # dev-loop static checks
```

## Grouping rationale

The old build tool bundled every static check into cargo-make. In edo we can
express most of them as `advisory-check`-style transforms (they participate
in the DAG, cache their pass/fail state, and only re-run on input changes).
But some are host-side dev tools (fmt, clippy) that don't fit the DAG model
and belong as thin subcommands that shell out.

### DAG-backed checks (each has a transform)

- `advisories` → `advisory-check` transform (see
  `doc/transforms/advisory-check.md`).
- `lockfile` → runs `edo.lock.json` refresh in dry-run mode; fails if
  anything would change.
- `kits` → for every `kit-image`, pull the image, decode metadata, verify
  vendor/name/version.

### Host-side checks (each shells out to a linter)

- `licenses` → `cargo deny check`.
- `fmt` → `cargo fmt --check`.
- `lints` → `cargo clippy -- -D warnings` + custom lint rules.
- `clippy` → alias for `lints`.
- `shell` → `shellcheck` over `crates/twoliter/scripts/*.sh`.
- `golangci-lint` → `golangci-lint run` over any Go source under
  `packages/**/`.

### Config-only checks

- `migrations` → walk `manifest.json` under every declared variant, verify
  each declared migration binary has a matching artifact.
- `boot-config` → parse `boot-config` fragments in variant manifests.
- `repo-expirations` → for `Infra.toml`'s repo config, warn if any expiration
  is within `<n>` days.

## Old-code references

- `old-twoliter/tools/advisory-checker/` — the whole binary.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.check-*]`,
  `[tasks.validate-*]`.
- `old-twoliter/tools/pubsys/src/repo/check_expirations/mod.rs` — repo
  expiration checker.

## Open questions

- Should `twoliter check` accept `all` to run every check? Yes, and it should
  run them in parallel where safe.
- Should CI plumbing (JUnit XML output) be built-in? Add `--format junit`
  later; not a v1 requirement.
