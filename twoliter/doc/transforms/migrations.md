# Transform: `migrations`

Package migration binaries (Bottlerocket's settings-migration executables) as
a single artifact that variant/repo builds can consume. Replaces
`rpm2migrations` (a 56-line shell script) with a Rust transform.

## TOML surface

```toml
# variants/aws-k8s-1.29/migrations.toml   (or under any variant)
schema-version = "3"

[migrations]
name = "aws-k8s-1.29-migrations"
version = "0.1.0"
arch = "x86_64"

# One or more RPM transforms whose contents include /usr/share/migrations/*.
packages = [
    "//packages/migration-foo",
    "//packages/migration-bar",
]
```

## Inputs

- `sdk` — SDK source (only for the RPM extraction environment; the packing
  itself is Rust).
- `packages` — RPMs containing migration binaries.

No tools-layer dependency.

## Outputs

Single-layer artifact: a tarball of migration binaries in the on-disk layout
`pubsys` expects when it assembles a TUF repo. Media type:
`application/vnd.bottlerocket.migrations.tar+gz`.

## Environment

Local farm. Migration extraction is pure IO + tar assembly — no container
needed, unlike the image-building transforms. The migration RPMs are already
built (as `rpm` transform artifacts); we're just pulling files out of them
and packing them into a tarball.

Old `rpm2migrations` ran in the container because the whole build chain
went through docker. New model doesn't need to.

## Algorithm

Ports `rpm2migrations` verbatim into `src/transform/migrations.rs`:

```rust
async fn transform(&self, ctx: Context, log: Log, storage: Storage) -> TransformStatus {
    let mut entries = Vec::new();

    for pkg_addr in &self.opts.packages {
        // Load the package's artifact (each RPM's transform output is one
        // OCI layer of *.rpm files).
        let rpms = self.list_rpms_in_artifact(pkg_addr, &storage).await?;

        for rpm_path in rpms {
            // For each migration binary in the RPM, record its:
            //   - filename (must match `migrate_from-<from-ver>-<name>` or
            //     `migrate_to-<to-ver>-<name>` naming convention)
            //   - contents (raw executable bytes)
            //   - target-version, direction, name (parsed from filename)
            let migrations = br_image::rpm_install::extract_migrations(rpm_path)?;
            entries.extend(migrations);
        }
    }

    // Deduplicate + validate: no two entries with the same (direction,
    // version, name). Ports the shell's implicit check via `set -e` +
    // `install` clobbering.
    let entries = br_image::migrations::validate(entries)?;

    // Emit a single tar.gz.
    let tarball = br_image::migrations::pack(entries)?;

    // Stage as the single output layer.
    storage.stage_artifact(...)
}
```

`br_image::rpm_install::extract_migrations(rpm_path)` uses the `rpm` crate
to read the RPM's payload (a cpio-compressed archive) and yields entries
under `/usr/share/migrations/`. No `rpm2cpio | cpio` subprocess.

`br_image::migrations::Migration` is:

```rust
pub struct Migration {
    pub name: String,
    pub from_version: semver::Version,
    pub to_version: semver::Version,
    pub direction: Direction,      // MigrateFrom | MigrateTo
    pub bytes: Vec<u8>,
}
```

`br_image::migrations::pack(entries) -> Vec<u8>` writes a deterministic
(sorted) tar.gz that matches the on-disk layout pubsys expects.

## Unique-id

```
sha256(
    "migrations",
    for pkg in packages.sorted_by_addr: pkg.artifact_id,
    node.name,
    node.version,
    node.arch,
    twoliter_binary_digest,
)
```

## Old-code references

- `old-twoliter/twoliter/embedded/rpm2migrations` — 56 lines of shell;
  full port target.
- `old-twoliter/tools/pubsys/src/repo.rs::update_manifest` — the consumer;
  it merges migration entries into the TUF manifest. Not being changed;
  we just need to keep the tarball layout compatible.
- `old-twoliter/tools/update-metadata/src/lib.rs::find_migrations` — the
  version-comparison logic the consumer applies. Referenced only to keep
  our naming conventions consistent.

## Open questions

- Should migrations be produced as a side-output of `variant-image` instead
  of its own transform? Old twoliter kept them separate because their input
  churn is decoupled from the variant image (a hotfix to a migration
  doesn't force a full variant rebuild). Keep them separate.
- Does the tarball need to include a manifest fragment (JSON listing every
  migration and its checksums)? Old `rpm2migrations` did not. Adding one
  is cheap and helps `twoliter publish repo` do integrity checks without
  re-reading the tar. Recommendation: add it.
