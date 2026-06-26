use edo::{context::{Context, FromElement}, transform::Transform};
use std::sync::Arc;
use crate::transform::{KitTransform, PackageBuildTransform};

pub mod config;
pub mod transform;

pub fn register(ctx: &Context) {
    let registry = ctx.registry();
    registry.register_transform("rpm", Arc::new(async |element, ctx| {
        Ok(Transform::new(PackageBuildTransform::new(&element, &ctx).await?))
    }));
    registry.register_transform("kit", Arc::new(async |element, ctx| {
        Ok(Transform::new(KitTransform::new(&element, &ctx).await?))
    }));
    // registry.register_transform("advisory-check", Arc::new(async |addr, node, ctx| {
    //     Ok(Transform::new(AdvisoryCheckTransform::new(&addr, &node, &ctx).await?))
    // }));
}
