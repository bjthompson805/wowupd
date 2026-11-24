//! Settings from `~/.config/wowupd/wowupd.conf`, and starting the game under Wine.

use crate::install::{Product, local_path};
use anyhow::{Context, Result, bail};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `key = value` lines; `#` starts a comment. `env` may repeat.
#[derive(Default)]
pub struct Config {
    pub dir: Option<PathBuf>,
    pub region: Option<String>,
    pub arch: Option<String>,
    pub locale: Option<String>,
    pub wine: Option<String>,
    pub wineprefix: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("wowupd").join("wowupd.conf"))
}

fn expand(v: &str) -> PathBuf {
    match (v.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(v),
    }
}

pub fn load() -> Result<Config> {
    let mut c = Config::default();
    let Some(path) = config_path() else { return Ok(c) };
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(c) };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').with_context(|| format!("{}:{}: expected key = value", path.display(), n + 1))?;
        let v = v.trim().to_owned();
        match k.trim() {
            "dir" => c.dir = Some(expand(&v)),
            "region" => c.region = Some(v),
            "arch" => c.arch = Some(v),
            "locale" => c.locale = Some(v),
            "wine" => c.wine = Some(expand(&v).to_string_lossy().into_owned()),
            "wineprefix" => c.wineprefix = Some(expand(&v)),
            "env" => {
                let (ek, ev) = v.split_once('=').with_context(|| format!("{}:{}: env needs NAME=value", path.display(), n + 1))?;
                c.env.push((ek.trim().to_owned(), ev.trim().to_owned()));
            }
            other => bail!("{}:{}: unknown setting {other:?}", path.display(), n + 1),
        }
    }
    Ok(c)
}

fn executables(arch: &str) -> &'static [&'static str] {
    match arch {
        "arm64" => &["Wow-ARM64.exe", "WowClassic-arm64.exe"],
        _ => &["Wow.exe", "WowClassic.exe"],
    }
}

/// Replaces this process with the game under Wine. Battle.net's Agent is kept from starting: the
/// client would launch it, and it isn't needed.
pub fn play(root: &Path, p: &Product, arch: &str, cfg: &Config) -> Result<()> {
    let dir = root.join(p.folder);
    let exe = executables(arch)
        .iter()
        .map(|e| local_path(&dir, e))
        .find(|e| e.is_file())
        .with_context(|| format!("no {arch} game executable in {}", dir.display()))?;
    let mut cmd = Command::new(cfg.wine.as_deref().unwrap_or("wine"));
    cmd.arg(exe.file_name().unwrap()).current_dir(&dir);
    if let Some(prefix) = &cfg.wineprefix {
        cmd.env("WINEPREFIX", prefix);
    }
    let mut overrides = std::env::var("WINEDLLOVERRIDES").unwrap_or_default();
    for (k, v) in &cfg.env {
        if k == "WINEDLLOVERRIDES" {
            overrides = v.clone();
        } else {
            cmd.env(k, v);
        }
    }
    for o in ["agent.exe=d", "battle.net.exe=d"] {
        if !overrides.to_lowercase().contains(o) {
            if !overrides.is_empty() {
                overrides.push(';');
            }
            overrides.push_str(o);
        }
    }
    cmd.env("WINEDLLOVERRIDES", overrides);
    eprintln!("starting {} in {}", exe.display(), dir.display());
    Err(cmd.exec()).context("could not start Wine")
}
