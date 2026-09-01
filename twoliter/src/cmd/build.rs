use std::collections::HashMap;

use crate::Args;
use crate::Result;
use clap::Parser;
use edo::context::Addr;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "build a package,kit or variant", long_about = None)]
pub struct Build {
    #[clap(subcommand)]
    command: BuildCommands,
}

impl Build {
    pub async fn run(&self, args: &Args) -> Result<()> {
        match &self.command {
            BuildCommands::Package(package) => package.run(args).await,
            BuildCommands::Kit(kit) => kit.run(args).await,
            BuildCommands::Variant(variant) => variant.run(args).await,
        }
    }
}

#[derive(Parser, Debug, Clone)]
enum BuildCommands {
    Package(BuildPackage),
    Kit(BuildKit),
    Variant(BuildVariant),
}

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "build a package in the project", long_about = None)]
pub struct BuildPackage {
    name: String,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl BuildPackage {
    pub async fn run(&self, args: &Args) -> Result<()> {
        let addr = Addr::parse(&format!("//packages/{}/rpm", self.name))?;
        let ctx = super::create_context(
            &args,
            &addr.to_string(),
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
            true,
        )
        .await?;
        ctx.run(&addr).await?;
        Ok(())
    }
}

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "build a kit in the project", long_about = None)]
pub struct BuildKit {
    name: String,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl BuildKit {
    pub async fn run(&self, args: &Args) -> Result<()> {
        let addr = Addr::parse(&format!("//kits/{}/build", self.name))?;
        let ctx = super::create_context(
            &args,
            &addr.to_string(),
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
            true,
        )
        .await?;
        ctx.run(&addr).await?;
        Ok(())
    }
}

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "build a variant in the project", long_about = None)]
pub struct BuildVariant {
    name: String,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl BuildVariant {
    pub async fn run(&self, args: &Args) -> Result<()> {
        let addr = Addr::parse(&format!("//variants/{}", self.name))?;
        let ctx = super::create_context(
            &args,
            &addr.to_string(),
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
            true,
        )
        .await?;
        ctx.run(&addr).await?;
        Ok(())
    }
}
