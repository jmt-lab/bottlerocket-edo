# Bottlerocket BuildStream
This repository contains a setup to autogenerate buildstream definitions to build a bottlerocket kit using buildstream. It supports any kit you checkout into repos/ but has helpers in just.

_NOTE: Currently this only supports building all the packages, it currently does not have logic to build the results into an actual kit_

## Requirements
Currently this requires you to have buildstream already installed, along with jq and crane

* buildstream
* crane
* jq
* tomlq (installable using just setup)
* if you want to use the justfile just

# Example Uses
The easiest way to learn how to use this and also generate the files is to use the justfile

## Clone your fork of the core kit or kernel kit

```
just clone bottlerocket-core-kit
just clone bottlerocket-kernel-kit
```

## Generate the buildstream definitions

```
just sync bottlerocket-kernel-kit
```

_NOTE: This only has to be redone if you add a package, remove a package or change the cargo.metadata.build-package.external-files entry, otherwise you don't need to rerun it_

If you change just the sdk version in Twoliter toml run:

```
just sync-sdk bottlerocket-kernel-kit
```

To update the definition

Likewise you can update a specific package definition using:

```
just sync-package bottlerocket-kernel-kit grub
```

## Build the whole kit for an architecture

```
just build bottlerocket-kernel-kit x86_64
```

## Build only a single package

```
just build-package bottlerocket-kernel-kit grub x86_64
```

## View the contents of a package artifact

```
just view-package bottlerocket-kernel-kit grub x86_64
```

## Extract the built artifacts

```
just checkout-package bottlerocket-kernel-kit grub x86_64
```
