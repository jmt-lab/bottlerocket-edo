# Source specifications

Each file here describes one edo `Source` kind that twoliter registers alongside
`register_core`'s built-in sources (`local`, `git`, `remote`, `oci`, `vendor`).

| Kind | Purpose |
|---|---|
| [`kit-image`](kit-image.md) | Fetch an OCI kit image + verify its `dev.bottlerocket.kit.v3.metadata` label. |
| [`sdk-image`](sdk-image.md) | *Not a real source kind.* The SDK is fetched through edo-core's built-in `image` source and `image` vendor; the doc explains why. |
| [`lookaside`](lookaside.md) | Fetch RPM external files (upstream tarballs), verifying SHA-512, with lookaside + upstream fallback. |

Registration lives in `src/lib.rs::register`:

```rust
registry.register_source(
    "kit-image",
    Arc::new(async |element, ctx| {
        Ok(Source::new(KitImageSource::new(&element, &ctx).await?))
    }),
);
```

Kind strings are lowercase, hyphen-separated.

## Why not just use `oci` for kits?

Edo ships two OCI-flavored source kinds out of the box: `oci` (pulls
arbitrary OCI images) and `image` (pulls OCI images via a matching `image`
vendor that also handles tag listing / semver resolution). For **kits** we
need two extra things on top of that:

1. **Kit metadata verification.** Reject any pulled image whose config does
   not carry a valid `dev.bottlerocket.kit.v<N>.metadata` label matching a
   supported version.
2. **Vendor rewriting.** Twoliter's `[vendor.<name>]` tables let the project
   redirect an image URI to a different registry. That is a twoliter concept,
   not an OCI concept; wrapping the pull in a twoliter-specific source keeps
   the metadata + rewriting logic together.

`kit-image` is that twoliter-specific wrapper.

For the **SDK** we do just use edo-core's `image` source: the SDK image
carries no kit-metadata label, so the metadata check would fail on it, and
vendor rewriting already works because `Twoliter::to_edo` emits an
`image`-kind vendor over the same twoliter `[vendor.<name>].registry` URI.
See [`sdk-image.md`](sdk-image.md) for the schema emission details.

The `lookaside` source is completely unlike an OCI pull — it's SHA-512-verified
tarball fetching, two-tier (lookaside URL first, upstream URL as fallback), and
lives in `crates/lookaside/`.
