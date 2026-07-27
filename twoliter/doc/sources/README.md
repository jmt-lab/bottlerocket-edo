# Source specifications

Each file here describes one edo `Source` kind that twoliter registers alongside
`register_core`'s built-in sources (`local`, `git`, `remote`, `oci`, `vendor`).

| Kind | Purpose |
|---|---|
| [`kit-image`](kit-image.md) | Fetch an OCI kit image + verify its `dev.bottlerocket.kit.v3.metadata` label. |
| [`sdk-image`](sdk-image.md) | Fetch the Bottlerocket SDK OCI image. |
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

## Why not just use `oci`?

Edo's built-in `oci` source pulls arbitrary OCI images. For kits and SDKs we
need two extra things:

1. **Kit metadata verification.** Reject any pulled image whose config does
   not carry a valid `dev.bottlerocket.kit.v<N>.metadata` label matching a
   supported version.
2. **Vendor rewriting.** Twoliter's `[vendor.<name>]` tables let the project
   redirect an image URI to a different registry. That is a twoliter concept,
   not an OCI concept; wrapping the pull in a twoliter-specific source keeps
   the metadata + rewriting logic together.

`sdk-image` is a thinner wrapper (no kit-metadata check), but the same
vendor-rewriting benefit applies.

The `lookaside` source is completely unlike an OCI pull — it's SHA-512-verified
tarball fetching, two-tier (lookaside URL first, upstream URL as fallback), and
lives in `crates/lookaside/`.
