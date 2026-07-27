# Publish: `ova`

Upload an OVA (Open Virtualization Format) to a VMware endpoint (vSphere,
vCenter). Optional convenience: also create a VM template from the uploaded
OVA.

## Usage

```
twoliter publish ova <variant>
    [--datacenter <dc>]
    [--datastore <ds>]
    [--folder <folder>]
    [--network <net>]
    [--as-template]
```

## Behavior

Ports `old-twoliter/tools/pubsys/src/vmware/upload_ova/mod.rs`:

1. Load the variant artifact; locate the `os.img.vmdk` layer (only present
   when the variant's `image-format = "vmdk"` produced an OVF).
2. Read `Infra.toml` `[vmware.<datacenter>]` for auth + endpoints.
3. Call `govc` (a Go-based vSphere CLI, external tool) via subprocess to do
   the upload — old pubsys does the same.
4. Optionally mark the uploaded VM as a template.

## Environment

Local. Requires `govc` on `PATH`.

## Old-code references

- `old-twoliter/tools/pubsys/src/vmware/upload_ova/mod.rs`.
- `old-twoliter/tools/pubsys/src/vmware/govc.rs`.
- `old-twoliter/tools/pubsys-config/src/vmware.rs` — config schema.
- `old-twoliter/twoliter/embedded/Makefile.toml` `[tasks.upload-ova]`,
  `[tasks.vmware-template]`.

## Open questions

- The old `_upload-ova-base` and `vmware-template` cargo-make targets split
  upload from template creation. New CLI can keep them as one flag or two
  subcommands; the `--as-template` flag is simpler.
