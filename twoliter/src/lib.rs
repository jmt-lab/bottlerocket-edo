///! Twoliter bottlerocket build tool library
///
/// * [`config`]: Project configuration schematic and conversion to edo elements
/// * [`kit`]: Kit related sources, transforms and construction logic
/// * [`model`]: Shared structures used across twoliter
/// * [`publish`]: Logic for publishing kits, amis and ssm parameters (formerly pubsys)
/// * [`rpm`]: RPM Package build logic
/// * [`variant`]: Variant related sources, transforms and construction logic
/// * [`util`]: Shared helpers and utilities
pub mod config;
pub mod kit;
pub mod model;
pub mod rpm;
pub mod util;
pub mod variant;

use edo::context::{Context, FromElement};
use edo::source::{Source, Vendor};
use edo::transform::Transform;
use std::sync::Arc;

use kit::{KitImageSource, KitImageVendor, KitTransform};
use rpm::PackageBuildTransform;
use variant::{RPM2ImageTransform, RepoTransform};

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
        "variant-repo",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(RepoTransform::new(&element, &ctx).await?))
        }),
    );
    registry.register_transform(
        "variant-image",
        Arc::new(async |element, ctx| {
            Ok(Transform::new(
                RPM2ImageTransform::new(&element, &ctx).await?,
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
}
