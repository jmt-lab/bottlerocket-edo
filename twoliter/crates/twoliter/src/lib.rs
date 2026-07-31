use crate::source::{KitImageSource, KitImageVendor};
use crate::transform::{
    KitTransform, PackageBuildTransform, VariantDiskTransform, VariantGrubTransform,
    VariantImageTransform, VariantRootfsTransform,
};
use edo::{
    context::{Context, FromElement},
    source::{Source, Vendor},
    transform::Transform,
};
use std::sync::Arc;

pub mod config;
pub mod image;
pub mod source;
pub mod transform;

pub fn register(ctx: &Context) {
    let registry = ctx.registry();
    registry.register_transform(
        "rpm",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                PackageBuildTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "kit",
        Arc::new(async |element, ctx| Ok(Transform::new(KitTransform::new(&element, &ctx).await?))),
    );
    // The `variant-*` transform family: four independently-cached
    // stages that together produce the same eight-layer artifact the
    // pre-decomposition `variant-image` monolith did. See
    // `crates/twoliter/src/transform/variant/mod.rs` for the
    // dependency chain and hash-input decomposition.
    registry.register_transform(
        "variant-rootfs",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                VariantRootfsTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "variant-disk",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                VariantDiskTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "variant-grub",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                VariantGrubTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_transform(
        "variant-image",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                VariantImageTransform::new(&element, &ctx).await?,
            ))
        }),
    );
    registry.register_source(
        "kit-image",
        Arc::new(async |element, ctx| Ok(Source::new(KitImageSource::new(&element, &ctx).await?))),
    );
    registry.register_vendor(
        "kit-image",
        Arc::new(async |element, ctx| Ok(Vendor::new(KitImageVendor::new(&element, &ctx).await?))),
    );
    // registry.register_transform("advisory-check", Arc::new(async |addr, node, ctx| {
    //     Ok(Transform::new(AdvisoryCheckTransform::new(&addr, &node, &ctx).await?))
    // }));
}
