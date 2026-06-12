mod checkout;
mod list;
mod prune;
mod run;
mod update;
mod util;

use std::collections::{BTreeMap, HashMap};

pub use checkout::*;
use edo::context::{ConsoleConfig, Element};
use edo::context::{Addr, Context, LogVerbosity};
use edo_core::register_core;
pub use list::*;
pub use prune::*;
pub use run::*;
pub use update::*;

use crate::Args;
use crate::Result;

pub async fn create_context(
    args: &Args,
    variables: HashMap<String, String>,
    locked: bool,
) -> Result<Context> {
    let verbosity = if args.trace {
        LogVerbosity::Trace
    } else if args.debug {
        LogVerbosity::Debug
    } else {
        LogVerbosity::Info
    };
    let ctx = Context::init(
        args.storage.clone(),
        args.config.clone(),
        variables,
        verbosity,
        ConsoleConfig {
            mode: edo::console::ConsoleMode::Full,
            ..Default::default()
        }
    )
    .await?;
    // Register all core component handlers
    register_core(&ctx);
    // Register all twoliter handlers
    twoliter::register(&ctx);

    // Register a local farm in the project directory
    ctx.add_farm(&Element::builder()
        .addr(Addr::parse("//default")?)
        .kind("local")
        .config(BTreeMap::default())
        .build()
    ).await?;
    // Now load the current project
    ctx.load_project(locked).await?;
    Ok(ctx)
}
