# Transform: `advisory-check`

Validate the project's declared advisories against the actual RPMs that would
ship: every advisory's `fixed-in` version must be `>=` the current RPM version,
and every advisory must reference a CVE ID that matches the CVE regex.

This is the `advisory-checker` binary in old twoliter, converted to a
transform so it participates in the DAG and only re-runs when advisories or
package versions change.

## TOML surface

```toml
# advisory-check.toml (project-root or under a specific kit)
schema-version = "3"

[advisory-check]
name = "advisories"

# Directory containing per-package advisories/*.toml files. Optional; defaults
# to `advisories/` at project root.
advisories-dir = "advisories"

# Packages to check. Any RPM transform address, or the special string "all"
# to include every rpm transform in the project.
packages = "all"
```

## Inputs

- `packages` — the set of RPM transforms whose specfiles determine the current
  versions. Resolved at prepare time.
- `advisories-dir` — a `local` source pointing at the advisories tree.

## Outputs

Either:

- Empty artifact (no layers), signaling success. The transform's presence in
  the DAG is what enforces the check.
- A single `advisory-report.json` layer listing every mismatch, with the
  transform marked as `TransformStatus::Failed` if there are any.

Recommendation: produce the report as a layer even on success, so consumers
can display "how many advisories were checked" without redoing the work.

## Environment

Local farm — this is pure Rust that reads RPM headers and TOML files, no
containerization needed.

## Algorithm

Ported straight from `old-twoliter/tools/advisory-checker/src/main.rs`:

1. `find_spec_file(package_dir)` for each declared package: locate its
   `<name>.spec`.
2. `get_rpm_metadata(spec_path)` — shell out to `rpm --define ... -q ...` to
   extract the current version (`EV`).
3. `collect_advisories(advisories_dir)` — walk `*.toml`.
4. For each advisory, check that `fixed-in` `>=` `EV` using `rpm_evr_compare`
   (the `rpm` crate's ordering).
5. `validate_advisories` emits `AdvisoryValidationErr` per mismatch.

## Unique-id

```
sha256(
    "advisory-check",
    for pkg in packages.sorted_by_addr: pkg.artifact_id,
    advisories_dir.tree_hash,
)
```

## Old-code references

- `old-twoliter/tools/advisory-checker/src/main.rs` (the full binary).
- `old-twoliter/tools/advisory-checker/src/models.rs` (`Advisory`, `CveId`,
  `PackageVersionManifest`).
- `LAST_UNTRACKED_PACKAGE_REMOVAL_DATE` constant — carry over.

## Open questions

- Do we want the check to gate `variant-image` builds by default, or should
  it be an opt-in transform that CI depends on? Recommendation: opt-in.
  Advisory checks are policy, not correctness — the variant builds fine
  without the check, and gating adds friction to local dev.
- `rpm --queryformat` was the old approach. If we already parse RPM headers
  natively (we depend on `rpm = "0.18"` in the root `Cargo.toml`), skip the
  shell-out.
