mod common;
mod diagnose;
mod fetch;
mod inspect;
mod replay;
mod state;

use anyhow::{bail, Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fetch") => fetch::fetch(),
        Some("inspect") => inspect::inspect(),
        Some("replay") => replay::replay_all(),
        Some("diagnose") => diagnose::diagnose(args.get(2).context("usage: diagnose <tx hash or its start>")?),
        _ => bail!("usage: soroban-rehearse fetch | inspect | replay | diagnose <tx hash>"),
    }
}
