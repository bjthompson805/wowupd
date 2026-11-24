use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use wowupd::install::{self, State};
use wowupd::launch;
use wowupd::report::{Event, Reporter};

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

/// Notes as lines; progress as one line rewritten in place on a terminal.
fn terminal() -> Reporter {
    let tty = std::io::stderr().is_terminal();
    Arc::new(move |e| match e {
        Event::Note(s) => eprintln!("{s}"),
        Event::Progress(p) => {
            let line = format!("  {:?}: {}", p.phase, p.describe());
            if tty {
                eprint!("\r{line}\x1b[K");
                if p.finished {
                    eprintln!();
                }
                let _ = std::io::stderr().flush();
            } else if p.finished {
                eprintln!("{line}");
            }
        }
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = launch::load()?;
    if cli.dir.is_some() {
        cfg.dir = cli.dir.clone();
    }
    if cli.region.is_some() {
        cfg.region = cli.region.clone();
    }
    let root = || cfg.dir.clone().context("no WoW folder: pass --dir or set dir in wowupd.conf");
    let options = |b: &Build, check_only: bool| install::Options {
        region: cfg.region(),
        locale: b.locale.clone().unwrap_or_else(|| cfg.locale()),
        arch: b.arch.clone().unwrap_or_else(|| cfg.arch()),
        jobs: b.jobs,
        check_only,
        verify: b.verify,
        report: terminal(),
    };
    match &cli.cmd {
        Cmd::Products => {
            for p in install::PRODUCTS {
                println!("{:<16} {:<16} {}", p.code, p.folder, p.name);
            }
        }
        Cmd::Status => {
            let root = root()?;
            for p in install::PRODUCTS {
                let s = install::product_status(&root, p, &cfg.region(), &cfg.arch());
                let state = match &s.state {
                    State::UpToDate => "up to date".to_owned(),
                    State::UpdateAvailable => "update available".to_owned(),
                    State::NotInstalled => "not installed".to_owned(),
                    State::Unknown(e) => format!("live version unknown: {e}"),
                };
                let (installed, live) = (s.installed.as_deref().unwrap_or("-"), s.live.as_deref().unwrap_or("?"));
                println!("{:<24} {:<16} installed {installed:<16} live {live:<16} {state}", p.name, p.code);
            }
        }
        Cmd::Update { products, build, check } => {
            let opts = options(build, *check);
            let products = products.iter().map(|c| install::product(c)).collect::<Result<Vec<_>>>()?;
            install::update(&root()?, &products, &opts)?;
        }
        Cmd::Play { product, build, offline } => {
            let opts = options(build, false);
            let root = root()?;
            let p = install::product(product)?;
            if !offline {
                install::update(&root, &[p], &opts)?;
            }
            eprintln!("starting {}", p.name);
            launch::play(&root, p, &opts.arch, &cfg)?;
        }
    }
    Ok(())
}
