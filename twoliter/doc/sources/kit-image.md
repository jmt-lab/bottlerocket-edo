# Source: `kit-image`

Fetch a kit's OCI image (multi-arch manifest list), verify its metadata
label, and expose the archive + a parsed `KitMetadata` struct.

## TOML surface

Kits are declared at project level in `Twoliter.toml`:

```toml
[[kit]]
name = "core-kit"
vendor = "bottlerocket"
version = "1.42.0"
```

They can also appear per-variant in `variant.toml`:

```toml
[[variant.kit]]
name = "extra-3-kit"
vendor = "my-vendor"
version = "0.1.0"
```

Each entry loads into an edo node at `//kits/<vendor>/<name>` with the
`kit-image` source kind. The full image URI is derived as
`<vendor.registry>/<name>:v<version>` — matching
`old-twoliter/twoliter/src/project/mod.rs::ProjectImage::project_image_uri`.

## Behavior

Implements `SourceImpl`:

- `fetch(log, storage)` — pull the OCI archive to
  `//edo-source-cache/kit-image/<vendor>/<name>@<digest>/`. Idempotent: uses
  the digest as its cache key.
- `cache(log, storage)` — default `Source::cache` implementation is fine.

Additionally exposes:

- `metadata() -> KitMetadata` — decode the base64 label
  `dev.bottlerocket.kit.v3.metadata` from the pulled config blob. Fail with a
  clear error if the label is missing, malformed, or version-mismatched.
- `arches() -> Vec<Arch>` — the arches present in the manifest list.
- `packages(arch: Arch) -> Vec<PackageRef>` — for the requested arch, the
  layer that holds each package's RPMs and its content-addressed digest.

Consumers (`variant-image`, `kit`, `repack-variant`) hash the kit's digest
into their unique-ids. Because the digest fully identifies the pulled image,
this is enough for edo's cache to invalidate correctly on kit updates.

## Vendor rewriting

At load time, the source reads `Twoliter.toml`'s `[vendor.<vendor-name>]` to
find the registry. When the project has an override (see
`doc/config/overrides.md`), the override registry wins. This is exactly
`ArtifactVendor::Overridden` from
`old-twoliter/twoliter/src/project/vendor.rs`.

## Lockfile

Every `kit-image` source contributes an entry to `edo.lock.json`:

```json
{
  "//kits/bottlerocket/core-kit": {
    "kind": "kit-image",
    "digest": "sha256:…",
    "manifest_list_digest": "sha256:…",
    "resolved_uri": "public.ecr.aws/bottlerocket/core-kit:v1.42.0"
  }
}
```

`twoliter update` refreshes these.

## Environment / permissions

The pull runs in the caller's environment (usually the host); `ocilot` handles
auth (docker credential helpers, `.docker/config.json`).

## Old-code references

- `old-twoliter/tools/oci-cli-wrapper/src/lib.rs::ImageTool` — old pull path,
  replaced by `ocilot`.
- `old-twoliter/twoliter/src/project/lock/image.rs::ImageResolver` — the old
  digest-resolution logic, kept in shape for the new source.
- `old-twoliter/twoliter/src/project/lock/archive.rs::OCIArchive` — the old
  disk layout for a pulled archive; new source uses edo's storage layer
  layout.

## Open questions

- Should this source expose a `.checkout(dest)` operation to unpack the RPMs
  into a directory (for `twoliter checkout //kits/…`)? Or is that a job for a
  separate `kit-checkout` transform? Recommendation: this source. Old
  twoliter's `checkout` was a first-class subcommand; making the source
  expose an unpack op keeps the CLI trivial.
- Multi-arch handling: today `variant-image` cares about one arch at a time,
  but `publish kit` cares about the whole manifest list. Both need to work
  from the same cached data.
