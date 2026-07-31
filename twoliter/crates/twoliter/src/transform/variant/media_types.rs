//! Media-type strings for every artifact produced by the `variant-*`
//! transform family.
//!
//! Final (finalizer-emitted) media types are the eight from
//! `doc/transforms/variant-image.md::Outputs`. Intermediate media
//! types (rootfs tar + metadata JSON, disk partial + partition
//! scratches + disk metadata JSON, plus grub pass-throughs) are
//! specific to this refactor: each stage's outputs carry a media type
//! that describes exactly what the layer is so the next stage can
//! locate the right bytes by media type rather than filename.

/// Final artifact media types (finalizer outputs). Their layer order
/// and file names are fixed by the doc and consumed by downstream
/// `twoliter publish` tooling.
pub mod final_ {
    pub const OS_IMG_LZ4: &str = "application/vnd.bottlerocket.image.os.lz4";
    pub const DATA_IMG_LZ4: &str = "application/vnd.bottlerocket.image.data.lz4";
    pub const BOOT_EXT4_LZ4: &str = "application/vnd.bottlerocket.image.boot.ext4.lz4";
    pub const ROOT_EXT4_LZ4: &str = "application/vnd.bottlerocket.image.root.ext4.lz4";
    pub const VERITY_LZ4: &str = "application/vnd.bottlerocket.image.verity.lz4";
    pub const BOOTCONFIG: &str = "application/vnd.bottlerocket.image.bootconfig";
    pub const MANIFEST_JSON: &str = "application/vnd.bottlerocket.image.manifest+json";
    pub const SBOM_SPDX: &str = "application/spdx+json";
}

/// Intermediate media types for the `variant-rootfs` transform.
pub mod rootfs {
    /// Layer 0: tar of the fully-populated `root-mount/` directory.
    pub const ROOTFS_TAR: &str = "application/vnd.bottlerocket.variant.rootfs.tar";
    /// Layer 1: JSON metadata (SELinux file_contexts path, packaged
    /// RPM names).
    pub const METADATA_JSON: &str = "application/vnd.bottlerocket.variant.rootfs.metadata+json";
    /// Layer 2: SBOM (SPDX JSON) generated over the same
    /// `deps/rpms/` directory that fed `stage_root`. Emitted here
    /// rather than in the finalizer so the finalizer doesn't need to
    /// re-stage every RPM. Consumers restage the file verbatim as
    /// the `sbom.spdx.json` layer of the finalizer's artifact.
    pub const SBOM_SPDX: &str = "application/spdx+json";
}

/// Intermediate media types for the `variant-disk` transform.
pub mod disk {
    /// Layer 0: `os.img` with root + hash + boot + data partitions
    /// populated, but *without* grub/ESP contents yet.
    pub const OS_IMG_PARTIAL: &str = "application/vnd.bottlerocket.variant.disk.partial";
    /// Layer 1: raw erofs root partition scratch (input to verity
    /// generation on any downstream consumer that wants to rebuild).
    pub const ROOT_EROFS_RAW: &str = "application/vnd.bottlerocket.variant.disk.root.erofs";
    /// Layer 2: raw verity hash-tree scratch.
    pub const VERITY_RAW: &str = "application/vnd.bottlerocket.variant.disk.verity.raw";
    /// Layer 3: raw ext4 boot partition scratch.
    pub const BOOT_EXT4_RAW: &str = "application/vnd.bottlerocket.variant.disk.boot.ext4";
    /// Layer 4 (optional): raw data partition scratch.
    pub const DATA_RAW: &str = "application/vnd.bottlerocket.variant.disk.data.raw";
    /// Layer N+1: JSON metadata (serialized [`DiskLayout`],
    /// resolved [`ImageLayout`], selected feature bits).
    pub const METADATA_JSON: &str = "application/vnd.bottlerocket.variant.disk.metadata+json";
}

/// Intermediate media types for the `variant-grub` transform. It
/// passes through the disk stage's raw partition scratches plus the
/// disk metadata layer, and adds a finalised `os.img` (with grub +
/// ESP contents populated), boot-config file, and boot ext4 scratch.
pub mod grub {
    /// Layer 0: finalised `os.img` (raw disk with grub + ESP
    /// populated).
    pub const OS_IMG_RAW: &str = "application/vnd.bottlerocket.variant.disk.raw";
    /// Layer 1: `boot-config` file.
    pub const BOOTCONFIG: &str = "application/vnd.bottlerocket.image.bootconfig";
}
