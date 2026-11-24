//! Blizzard's version service and CDN.

use crate::tact::{Hash, hex, parse_table};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

const VERSION_SERVICE: &str = "https://us.version.battle.net/v2/products";
const TRIES: usize = 4;

/// One product's live build in one region.
pub struct Live {
    pub product: String,
    pub region: String,
    pub version: String,
    pub build_config: String,
    pub cdn_config: String,
    pub key_ring: String,
    pub cdn_path: String,
    pub cdn_hosts: Vec<String>,
    pub cdn_servers: String,
}

pub struct Cdn {
    agent: ureq::Agent,
    pub path: String,
    hosts: Vec<String>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(20)).timeout_read(Duration::from_secs(60)).build()
}

fn read_body(r: ureq::Response) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    r.into_reader().read_to_end(&mut b)?;
    Ok(b)
}

fn row<'a>(rows: &'a [HashMap<String, String>], col: &str, value: &str) -> Option<&'a HashMap<String, String>> {
    rows.iter().find(|r| r.get(col).is_some_and(|v| v.eq_ignore_ascii_case(value)))
}

pub fn live(product: &str, region: &str) -> Result<Live> {
    let a = agent();
    let get = |what: &str| -> Result<Vec<HashMap<String, String>>> {
        let url = format!("{VERSION_SERVICE}/{product}/{what}");
        let body = read_body(a.get(&url).call().with_context(|| url.clone())?)?;
        Ok(parse_table(&String::from_utf8_lossy(&body)))
    };
    let versions = get("versions")?;
    let cdns = get("cdns")?;
    let v = row(&versions, "Region", region).with_context(|| format!("{product} has no {region} build"))?;
    let c = row(&cdns, "Name", region).with_context(|| format!("{product} has no {region} CDN"))?;
    let f = |r: &HashMap<String, String>, k: &str| r.get(k).cloned().unwrap_or_default();
    Ok(Live {
        product: product.to_owned(),
        region: region.to_owned(),
        version: f(v, "VersionsName"),
        build_config: f(v, "BuildConfig"),
        cdn_config: f(v, "CDNConfig"),
        key_ring: f(v, "KeyRing"),
        cdn_path: f(c, "Path"),
        cdn_hosts: f(c, "Hosts").split_whitespace().map(str::to_owned).collect(),
        cdn_servers: f(c, "Servers"),
    })
}

impl Cdn {
    pub fn new(live: &Live) -> Cdn {
        Cdn { agent: agent(), path: live.cdn_path.clone(), hosts: live.cdn_hosts.clone() }
    }

    /// Fetches `<kind>/ab/cd/<hash><suffix>`, optionally a byte range, trying each host in turn.
    pub fn fetch(&self, kind: &str, hash: &str, suffix: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        let rel = format!("{}/{kind}/{}/{}/{hash}{suffix}", self.path, &hash[..2], &hash[2..4]);
        let mut last = None;
        for attempt in 0..TRIES {
            for host in &self.hosts {
                let url = format!("http://{host}/{rel}");
                let mut req = self.agent.get(&url);
                if let Some((start, len)) = range {
                    req = req.set("Range", &format!("bytes={start}-{}", start + len - 1));
                }
                match req.call().map_err(anyhow::Error::from).and_then(read_body) {
                    Ok(b) if range.is_none_or(|(_, len)| b.len() as u64 == len) => return Ok(b),
                    Ok(b) => last = Some(anyhow::anyhow!("{url}: got {} bytes", b.len())),
                    // A missing file stays missing on every host.
                    Err(e) if matches!(e.downcast_ref::<ureq::Error>(), Some(ureq::Error::Status(404, _))) && attempt > 0 => {
                        return Err(e.context(url));
                    }
                    Err(e) => last = Some(e.context(url)),
                }
            }
            std::thread::sleep(Duration::from_secs(1 << attempt));
        }
        match last {
            Some(e) => Err(e),
            None => bail!("no CDN hosts"),
        }
    }

    pub fn config(&self, hash: &str) -> Result<Vec<u8>> {
        self.fetch("config", hash, "", None)
    }

    pub fn data(&self, ekey: &Hash) -> Result<Vec<u8>> {
        self.fetch("data", &hex(ekey), "", None)
    }
}
