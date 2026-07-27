# Lookaside cache (S3 source cache)

> **Not a source kind.** These are implementation notes for how the
> old-buildsys "lookaside" behavior is expressed in twoliter using edo's
> existing primitives. `src/lib.rs::register` does *not* register a
> `lookaside` source kind, and there is no `crates/lookaside/`.

The old-buildsys `LookasideCache` (see
`old-twoliter/tools/buildsys/src/cache.rs`) fetched external tarballs
through a two-tier lookup: a lookaside mirror first, upstream URL as
fallback, SHA-512 verified. Edo already offers every piece needed to
recover this behavior without a bespoke source kind:

- **`remote` source** (`edo-core/src/source/remote.rs`) fetches a URL,
  verifies a SHA-256 content ref, and stores the blob content-addressed.
  If the blob is already local it short-circuits without a network fetch.
- **Source caches** (`//edo-source-cache/<name>`, priority-ordered)
  transparently short-circuit `Source::fetch` when the artifact `Id` is
  already present in a cache backend. On cache miss edo *automatically*
  falls through to `Source::fetch`, which for a `remote` source pulls
  from the upstream URL.
- **`s3` storage backend** (`edo-core/src/storage/s3/mod.rs`) is already
  registered under kind `"s3"` and can act as a source cache directly.

Composing these three gives exactly the old lookaside behavior.

## External files → `remote` sources

External files are declared per package as `[[package.external-files]]`
in `package.toml`. In the current schema these are represented as the
existing `[[source]]` table with `kind = "url"` — see
`src/config/package.rs::Source::Url`. `Package::to_edo` emits each entry
as an edo element of kind `remote`:

```toml
# package.toml
[[package.external-files]]
url = "https://cdn.example.com/kernel-6.1.42.tar.xz"
ref = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
out = "kernel-6.1.42.tar.xz"
```

Notes on the schema:

- `ref` is a **SHA-256** hex digest (optionally `sha256:`-prefixed, per
  `RemoteSource::blob_digest`). SHA-512 is not accepted — edo hashes and
  stores content by SHA-256.
- `out` is the on-disk filename downstream transforms (e.g. `rpm`) see
  under `SOURCES/`.
- There is no `force-upstream` flag. Upstream fallback is always on:
  when the source cache misses, edo calls `RemoteSource::fetch` against
  `url` automatically.
- There is no mtime normalization. Edo hashes content, not mtimes; the
  mtime drift that motivated the old-buildsys workaround does not exist
  in this pipeline.

`Package::to_edo`'s existing `Source::Url` branch already emits the
`kind = "remote"` element; no code change is needed to accept external
files, only the doc alignment above.

## `[lookaside]` in `Twoliter.toml`

```toml
# Twoliter.toml
[lookaside]
bucket = "bottlerocket-lookaside-v2"
prefix = "cache"          # optional
```

`Twoliter::to_edo` emits this as a `[cache.source.lookaside]` element
of kind `s3` at address `//edo-source-cache/lookaside`, using
`Schema::add_source_cache`. The two fields map 1:1 to
`edo-core`'s `S3BackendOptions`: `bucket` is required, `prefix` is an
optional key prefix.

Region and credentials are taken from the ambient AWS config
(`aws_config::load_defaults`), matching edo-core's S3 backend behavior.
Anonymous / public-read access is not exposed today (see "Open items").

When the `[lookaside]` table is absent, twoliter does *not* seed a
`"lookaside"` source-cache entry. This leaves the slot free for an
`edo.toml` overlay to define one, and `Schema::add_source_cache` is
or-insert so an overlay can also override the twoliter-emitted entry.

## Behavior at build time

For each `[[package.external-files]]` entry:

1. `Storage::fetch_source` checks every registered source cache for the
   artifact `Id` derived from the `remote` element. The `lookaside`
   cache (if configured) checks S3.
2. **Hit:** the blob is returned from S3. No upstream network fetch.
3. **Miss:** edo falls through to `RemoteSource::fetch`, which pulls
   `url`, verifies against `ref`, and stores the blob into edo storage.
   The next build with the same `Twoliter.toml` and `[lookaside]`
   populated will find the blob on the S3 side once it has been
   promoted there.

## Old-code references

- `old-twoliter/tools/buildsys/src/manifest.rs::ExternalFile` — the
  schema for a single entry. The `url` and hash fields survive; the
  runtime does not.

The rest of `old-twoliter/tools/buildsys/src/cache.rs::LookasideCache`,
including `force-upstream`, `upstream-source-fallback`, the composed
`${lookaside}/<name>/<sha512>/<name>` URL layout, and the mtime
normalization, is intentionally not ported. Rebuilding the S3 bucket
from upstream sources is the migration path.

## Open items

- **Region / endpoint override.** `S3BackendOptions` deserializes only
  `bucket` and `prefix`; it uses `aws_config::load_defaults` for auth
  and region. That is fine for the AWS-hosted bottlerocket cache but
  may need extending upstream if a fork wants to host lookaside on
  MinIO or a non-default region.
- **Public read-only access.** The old lookaside was an anonymous HTTPS
  mirror. `aws-sdk-s3` clients can be configured for anonymous reads,
  but `S3BackendOptions` doesn't expose that toggle today. If the new
  bucket is public and the goal is "no AWS credentials required to
  build," that needs a small edo-side change.
