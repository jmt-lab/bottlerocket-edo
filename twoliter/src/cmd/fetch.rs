use std::collections::HashMap;

use crate::Args;
use crate::Result;
use clap::Parser;
use edo::context::Addr;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Fetch sources for a target without executing transforms", long_about = None)]
pub struct Fetch {
    addr: String,
    #[clap(long = "arch")]
    arch: Option<String>,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl Fetch {
    pub async fn run(&self, args: Args) -> Result<()> {
        let mut variables: HashMap<String, String> = self
            .args
            .clone()
            .map(HashMap::from_iter)
            .unwrap_or_default();
        if let Some(arch) = self.arch.as_ref() {
            variables.insert("arch".to_string(), arch.clone());
        }
        let ctx = super::create_context(&args, &self.addr, variables, true, true).await?;
        let addr = Addr::parse(self.addr.as_str())?;
        ctx.fetch(&addr).await?;
        Ok(())
    }
}
