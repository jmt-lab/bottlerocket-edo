# Source: `sdk-image`

**Status:** *Not implemented as a distinct source.* The Bottlerocket SDK is
resolved and fetched through edo-core's built-in `image` source and `image`
vendor. This document is kept so future contributors do not re-add a
redundant `sdk-image` kind.

## Why no dedicated `sdk-image`

The SDK OCI image does **not** carry the `dev.bottlerocket.kit.v3.metadata`
label. `KitImageSource::fetch` (`src/source/kit_image.rs`) requires that
label and fails with `MissingMetadataSnafu` when it is absent, so kits
cannot ride the same code path as the SDK. Old twoliter side-stepped this
with `.skip_metadata_retrieval() // SDKs don't have metadata`
(`old-twoliter/twoliter/src/project/lock/mod.rs`).

Rather than write a second image-pull source that only differs in "skip the
kit-metadata check", we point the SDK at edo-core's plain `image` source
and `image` vendor, both of which already do exactly what an `sdk-image`
kind would do:

| Capability the spec once asked for | Where it lives today                                   |
|------------------------------------|---------------------------------------------------------|
| `fetch(log, storage)`              | `edo_core::source::ImageSource::fetch` (`crates/core/src/source/oci.rs`) |
| `image_uri()` for container base   | `edo_core::source::ImageSource::uri` (via the `Source` trait) |
| `digest(arch)` for cache-keying    | `edo_core::source::ImageSource::digest` (via the `Source` trait) |
| Vendor rewriting                   | `edo_core::vendor::ImageVendor` (`crates/core/src/vendor/oci.rs`) — same `uri` shape as `KitImageVendor` |
| Tag listing (`v`-prefix fallback)  | `ImageVendor::get_options` — matches `KitImageVendor::get_options` |

No kit-metadata check runs against the SDK, which is the point.

## TOML surface

Unchanged from any earlier draft:

```toml
[sdk]
name = "bottlerocket-sdk"
vendor = "bottlerocket"
version = "0.66.0"
```

Loads into the edo node `//<sdk-name>` (e.g. `//bottlerocket-sdk`).

## Schema emission

`Twoliter::to_edo` (`src/config/twoliter.rs`) emits **two** vendor elements
per `[vendor.<name>]` in the twoliter file, over the same registry URI:

```
//vendor/<name>       kind=kit-image   uri=<registry>   name=<name>
//vendor/<name>-sdk   kind=image       uri=<registry>
```

The `-sdk` suffix keeps the vendor addresses trivially derivable from the
twoliter vendor name — edo's resolver keys vendors by their address, so
two vendor elements with different kinds may coexist over the same URI.

The SDK requirement then routes through the `-sdk` vendor:

```
//<sdk.name>   kind=image   vendor=<sdk.vendor>-sdk   at=<sdk.version>
```

Kit requirements at `//kits/<vendor>/<name>` continue to point at the
`kit-image` vendor and use `kind = "kit-image"`, so the kit-metadata check
in `KitImageSource::fetch` still runs for every kit.

If a project happens to declare both `[vendor.foo]` and `[vendor.foo-sdk]`,
the `-sdk` suffix would collide with the SDK-vendor address for `foo`. In
that case `Twoliter::to_edo` returns `error::Error::VendorNameCollision`
and asks the user to rename one of the two.

## Lockfile

The SDK contributes exactly one entry to `edo.lock.json`, with the shape
edo-core's `image` source produces:

```json
"//bottlerocket-sdk": {
  "kind": "image",
  "ref": "<sha256>",
  "uri": "public.ecr.aws/bottlerocket/bottlerocket-sdk:v0.76.0"
}
```

This is the shape the project's committed lockfile already has, from the
last successful resolution through edo-core's `image` vendor.

## Open questions

- Old twoliter had an `SDKLocked` lock state that let some targets skip
  full kit resolution. Edo's `Scheduler` naturally does this: nodes only
  load what they depend on. So there is no separate SDK-only lock — the
  CLI just runs against the addr of whatever target the user asks for.
- Should the SDK source verify a signature? None today. Out of scope for
  twoliter — if it becomes needed, the check would go into
  `crates/core`'s `image` source so every consumer benefits, not into a
  twoliter-only wrapper.

## Old-code references

- `old-twoliter/twoliter/src/project/mod.rs::Project::sdk_image` — how
  the SDK URI was composed pre-edo.
- `old-twoliter/twoliter/src/project/lock/mod.rs::LockedSDK` — SDK-only
  lockfile state. Superseded by the single edo lock.
