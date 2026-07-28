use crate::source::{KitImageSource, KitImageVendor};
use crate::transform::{KitTransform, PackageBuildTransform, VariantImageTransform};
use edo::{
    context::{Context, FromElement},
    source::{Source, Vendor},
    transform::Transform,
};
use std::sync::Arc;

pub mod config;
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
