# Source: `sdk-image`

Fetch the Bottlerocket SDK OCI image, expose its digest for cache-keying, and
optionally unpack its filesystem layers as a container base image.

## TOML surface

Declared once per project in `Twoliter.toml`:

```toml
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "0.66.0"
```

Loads into the edo node `//<sdk-name>` with kind `sdk-image` (or the project
can use `//bottlerocket-sdk` explicitly). Same vendor-rewriting rules as
`kit-image`.

## Behavior

Implements `SourceImpl`:

- `fetch(log, storage)` — pull the OCI archive to
  `//edo-source-cache/kit-image/<vendor>/<name>@<digest>/`. Reuses the same
  cache slot as `kit-image` because SDKs are OCI images too.

Exposes:

- `image_uri() -> ImageUri` — for consumers that need to run the SDK as a
  container base image.
- `digest(arch) -> String` — for hashing into transform unique-ids.

## Environment / permissions

Same as `kit-image`.

## Lockfile

Contributes one entry to `edo.lock.json`, keyed by SDK addr, with the same
shape as a kit entry.

## Old-code references

- `old-twoliter/twoliter/src/project/mod.rs::Project::sdk_image` — how the
  SDK URI is composed today.
- `old-twoliter/twoliter/src/project/lock/mod.rs::LockedSDK` — SDK-only
  lockfile state (still needed for `twoliter make` targets that don't need
  full kit resolution; new twoliter drops this notion in favor of a single
  edo lock).

## Open questions

- Old twoliter had an `SDKLocked` lock state that let some targets skip full
  kit resolution. Edo's `Scheduler` naturally does this: nodes only load
  what they depend on. So there's no separate SDK-only lock — the CLI just
  runs against the addr of whatever target the user asks for.
- Should the SDK source verify a signature? None today. Out of scope.
