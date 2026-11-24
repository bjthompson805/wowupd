mod casc;
mod cdn;
mod install;
mod jenkins;
mod launch;
mod tact;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Installs, updates and starts World of Warcraft clients from Blizzard's CDN, without the Battle.net
/// launcher. Settings come from ~/.config/wowupd/wowupd.conf; options override them.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The World of Warcraft folder (the one holding Data/ and _classic_/ etc.).
    #[arg(short, long, global = true)]
    dir: Option<PathBuf>,
    /// CDN region. Default: us.
    #[arg(long, global = true)]
    region: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct Build {
    /// Client architecture: arm64 or x86_64. Default: this machine's.
    #[arg(long)]
    arch: Option<String>,
    /// Default: enUS.
    #[arg(long)]
    locale: Option<String>,
    /// Parallel downloads.
    #[arg(short, long, default_value_t = 8)]
    jobs: usize,
    /// Check every file even when the version is current, and repair what is missing.
    #[arg(long)]
    verify: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show installed and live versions of each product.
    Status,
    /// Install or update products (wowf, wow_classic, wow_classic_era, wow_anniversary, wow).
    Update {
        #[arg(required = true)]
        products: Vec<String>,
        #[command(flatten)]
        build: Build,
        /// Only report what is missing.
        #[arg(long)]
        check: bool,
    },
    /// Update a product if needed, then start it under Wine.
    Play {
        product: String,
        #[command(flatten)]
        build: Build,
        /// Start without checking for an update.
        #[arg(long)]
        offline: bool,
    },
    /// List the products wowupd knows.
    Products,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = launch::load()?;
    let root = || cli.dir.clone().or(cfg.dir.clone()).context("no WoW folder: pass --dir or set dir in wowupd.conf");
    let region = cli.region.clone().or(cfg.region.clone()).unwrap_or_else(|| "us".to_owned());
    let options = |b: &Build, check_only: bool| install::Options {
        region: region.clone(),
        locale: b.locale.clone().or(cfg.locale.clone()).unwrap_or_else(|| "enUS".to_owned()),
        arch: b.arch.clone().or(cfg.arch.clone()).unwrap_or_else(|| if cfg!(target_arch = "aarch64") { "arm64" } else { "x86_64" }.to_owned()),
        jobs: b.jobs,
        check_only,
        verify: b.verify,
    };
    match &cli.cmd {
        Cmd::Products => {
            for p in install::PRODUCTS {
                println!("{:<16} {:<16} {}", p.code, p.folder, p.name);
            }
        }
        Cmd::Status => install::status(&root()?, &region)?,
        Cmd::Update { products, build, check } => {
            let opts = options(build, *check);
            let root = root()?;
            for code in products {
                install::update(&root, install::product(code)?, &opts)?;
            }
        }
        Cmd::Play { product, build, offline } => {
            let opts = options(build, false);
            let root = root()?;
            let p = install::product(product)?;
            if !offline {
                install::update(&root, p, &opts)?;
            }
            launch::play(&root, p, &opts.arch, &cfg)?;
        }
    }
    Ok(())
}
