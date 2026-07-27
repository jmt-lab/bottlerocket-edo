# Source: `lookaside`

Fetch a single upstream file (typically a source tarball) via a two-tier
lookup: try the project-configured lookaside cache first, then fall back to
the upstream URL if permitted. Verify SHA-512 before returning.

Ports `old-twoliter/tools/buildsys/src/cache.rs::LookasideCache`.

## TOML surface

External files are declared per package in
`[[package.metadata.build-package.external-files]]` (or in `package.toml`'s
`[[package.external-files]]` table in the new schema):

```toml
[[package.external-files]]
url = "https://cdn.example.com/kernel-6.1.42.tar.xz"
sha512 = "e3b0c44…"
# Optional: rename on disk.
path = "kernel-6.1.42.tar.xz"
# Optional: skip the lookaside and go straight upstream.
force-upstream = false
# Optional: `[BundleModule]` — go|rust, triggers cargo-vendor/go-vendor.
bundle-modules = ["go"]
```

Each entry produces an edo node with kind `lookaside`, addressed as
`//packages/<pkg>/external/<hash-slug>`.

## Behavior

Implements `SourceImpl`:

- `fetch(log, storage)`:
  1. If the SHA-512 verified copy is already present at
     `//edo-source-cache/lookaside/<sha512>/<filename>`, return it.
  2. Compose the lookaside URL: `${lookaside-cache}/<name>/<sha512>/<name>`.
     `${lookaside-cache}` comes from project config (defaults to
     `https://cache.bottlerocket.aws`).
  3. Fetch the lookaside URL. Verify SHA-512. On success, store and return.
  4. On failure:
     - If `force-upstream` or the project-wide `upstream-source-fallback` is
       set, fetch `url`, verify, store, return.
     - Otherwise fail with a clear error naming the missing hash.
  5. Set the file's mtime to the project's canonical mtime so subsequent
     builds don't spuriously invalidate.

## Cache slot

`//edo-source-cache/lookaside/<sha512>/<filename>`. Content-addressed by
SHA-512 so multiple packages that pull the same file share the cache.

## Global configuration

```toml
# Twoliter.toml
[lookaside]
url = "https://cache.bottlerocket.aws"
upstream-source-fallback = false
```

The URL is passed to the source at construction. `upstream-source-fallback`
is a per-project fallback flag; individual entries can still opt in via
`force-upstream`.

## Environment / permissions

Local farm — HTTP client only. Uses the same `User-Agent` string old
buildsys did (`"Bottlerocket buildsys <version> (…)"`).

## Old-code references

- `old-twoliter/tools/buildsys/src/cache.rs::LookasideCache::fetch` — the
  reference implementation, port verbatim into `crates/lookaside/`.
- `old-twoliter/tools/buildsys/src/manifest.rs::ExternalFile` — the schema
  for a single entry.
- `old-twoliter/tools/buildsys/src/gomod.rs`, `rustmod.rs`, `vendormod.rs` —
  the vendored-modules hook. In new twoliter these are separate edo
  transforms (`cargo-vendor`, `go-vendor`), and the `lookaside` source only
  fetches the raw tarball.

## Open questions

- Does the source expose the file's on-disk path to consumers, or a
  `Reader`? Both are useful — the `rpm` transform wants a path (to stage
  into a `SOURCES/` dir); `cargo-vendor`/`go-vendor` want a stream. Follow
  edo's convention: `Source::fetch` writes to a path in storage, the
  transform reads from that path.
- `mtime` normalization was a buildsys correctness fix (mtime drift caused
  cargo to rebuild). Edo transforms hash contents, not mtimes, so the mtime
  normalization is only relevant for tools running inside the container
  (cargo/make). Keep it.
