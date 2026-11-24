//! Brings one product in a WoW folder up to its live build: game data into `Data/`, loose files into
//! the product's folder (`_forever_`, `_classic_`, ...), then `.build.info`.

use crate::casc::{Segment, Storage};
use crate::cdn::{Cdn, Live, live};
use crate::tact::{self, Hash, blte_decode, blte_key_matches, hex, md5, unhex};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub struct Product {
    pub code: &'static str,
    pub folder: &'static str,
    pub name: &'static str,
}

pub const PRODUCTS: &[Product] = &[
    Product { code: "wowf", folder: "_forever_", name: "WoW: Forever" },
    Product { code: "wow_classic", folder: "_classic_", name: "WoW Classic" },
    Product { code: "wow_classic_era", folder: "_classic_era_", name: "WoW Classic Era" },
    Product { code: "wow_anniversary", folder: "_anniversary_", name: "WoW Classic Anniversary" },
    Product { code: "wow", folder: "_retail_", name: "World of Warcraft" },
];

pub fn product(code: &str) -> Result<&'static Product> {
    PRODUCTS.iter().find(|p| p.code == code || p.folder.trim_matches('_') == code).with_context(|| {
        let known: Vec<_> = PRODUCTS.iter().map(|p| p.code).collect();
        format!("unknown product {code:?} (known: {})", known.join(", "))
    })
}

pub struct Options {
    pub region: String,
    pub locale: String,
    pub arch: String,
    pub jobs: usize,
    pub check_only: bool,
    /// Check every blob even when the version is current.
    pub verify: bool,
}

/// Reads a config from `Data/config`, or fetches it and keeps it there. Configs are named by their MD5.
fn config(cdn: &Cdn, data: &Path, hash: &str) -> Result<Vec<u8>> {
    let path = data.join("config").join(&hash[..2]).join(&hash[2..4]).join(hash);
    if let Ok(b) = fs::read(&path) {
        if hex(&md5(&b)) == hash {
            return Ok(b);
        }
    }
    let b = cdn.config(hash)?;
    if hex(&md5(&b)) != hash {
        bail!("config {hash} does not match its hash");
    }
    write_atomic(&path, &b)?;
    Ok(b)
}

/// A manifest path (`Utils\Foo.exe`) under `dir`, matching existing names without regard to case as
/// Windows does: the Agent may have created `UTILS`.
pub fn local_path(dir: &Path, manifest_path: &str) -> PathBuf {
    let mut out = dir.to_owned();
    for part in manifest_path.split(['\\', '/']).filter(|p| !p.is_empty()) {
        let exact = out.join(part);
        if exact.exists() {
            out = exact;
            continue;
        }
        let found = fs::read_dir(&out).ok().and_then(|rd| {
            rd.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().eq_ignore_ascii_case(part))
        });
        out = out.join(found.as_deref().unwrap_or(part.as_ref()));
    }
    out
}

fn write_atomic(path: &Path, b: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_file_name(format!(".{}.wowupd", path.file_name().unwrap().to_string_lossy()));
    fs::write(&tmp, b)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A blob by encoding key, from local storage if it is there.
fn blob(cdn: &Cdn, storage: &Storage, ekey: &Hash) -> Result<Vec<u8>> {
    if storage.has(ekey) {
        if let Ok(b) = storage.read(ekey) {
            return Ok(b);
        }
    }
    let b = cdn.data(ekey)?;
    if !blte_key_matches(&b, ekey) {
        bail!("{} does not match its key", hex(ekey));
    }
    Ok(b)
}

/// Where a missing blob lives on the CDN.
#[derive(Clone, Copy)]
enum Source {
    Archive { archive: usize, offset: u64, size: u64 },
    Loose,
}

struct Job {
    archive: Option<usize>,
    start: u64,
    len: u64,
    /// (ekey, offset in the job, size); a loose job has one blob of unknown size.
    blobs: Vec<(Hash, u64, u64)>,
}

/// Neighbouring blobs of one archive merge into one request when the gap between them is small.
const MERGE_GAP: u64 = 256 * 1024;
const MAX_REQUEST: u64 = 32 * 1024 * 1024;

fn requests(missing: &HashMap<Hash, Source>) -> Vec<Job> {
    let mut by_archive: HashMap<usize, Vec<(Hash, u64, u64)>> = HashMap::new();
    let mut jobs = Vec::new();
    for (k, s) in missing {
        match *s {
            Source::Archive { archive, offset, size } => by_archive.entry(archive).or_default().push((*k, offset, size)),
            Source::Loose => jobs.push(Job { archive: None, start: 0, len: 0, blobs: vec![(*k, 0, 0)] }),
        }
    }
    for (archive, mut blobs) in by_archive {
        blobs.sort_by_key(|b| b.1);
        let mut cur: Option<Job> = None;
        for (k, off, size) in blobs {
            if let Some(j) = &mut cur {
                let end = j.start + j.len;
                if off >= end && off - end <= MERGE_GAP && off + size - j.start <= MAX_REQUEST {
                    j.blobs.push((k, off - j.start, size));
                    j.len = off + size - j.start;
                    continue;
                }
                jobs.push(cur.take().unwrap());
            }
            cur = Some(Job { archive: Some(archive), start: off, len: size, blobs: vec![(k, 0, size)] });
        }
        jobs.extend(cur);
    }
    // Big requests first keeps the tail of the run short.
    jobs.sort_by_key(|j| std::cmp::Reverse(j.len));
    jobs
}

struct Progress {
    done: AtomicU64,
    total: u64,
    started: Instant,
    tty: bool,
}

impl Progress {
    fn show(&self, last: bool) {
        let done = self.done.load(Ordering::Relaxed);
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let line = format!(
            "  {:.2} / {:.2} GiB  {:.1} MiB/s",
            done as f64 / (1u64 << 30) as f64,
            self.total as f64 / (1u64 << 30) as f64,
            done as f64 / secs / (1u64 << 20) as f64
        );
        if self.tty {
            eprint!("\r{line}  ");
            if last {
                eprintln!();
            }
        } else if last {
            eprintln!("{line}");
        }
    }
}

/// Downloads `missing` into new segments of `storage`, `jobs` requests at a time.
fn download(cdn: &Cdn, storage: Storage, archives: &[String], missing: &HashMap<Hash, Source>, jobs: usize) -> Result<Storage> {
    let work = Mutex::new(requests(missing));
    let total = missing.values().map(|s| if let Source::Archive { size, .. } = s { *size } else { 0 }).sum();
    let progress = Progress { done: AtomicU64::new(0), total, started: Instant::now(), tty: std::io::stderr().is_terminal() };
    let storage = Mutex::new(storage);
    let failed = AtomicBool::new(false);
    let errors = Mutex::new(Vec::<String>::new());
    let last_shown = Mutex::new(Instant::now());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                let mut seg: Option<Segment> = None;
                let result = (|| -> Result<()> {
                    loop {
                        if failed.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        let Some(job) = work.lock().unwrap().pop() else { return Ok(()) };
                        let body = match job.archive {
                            Some(a) => cdn.fetch("data", &archives[a], "", Some((job.start, job.len)))?,
                            None => cdn.data(&job.blobs[0].0)?,
                        };
                        for &(ekey, off, size) in &job.blobs {
                            let b = if job.archive.is_some() { &body[off as usize..(off + size) as usize] } else { &body[..] };
                            if !blte_key_matches(b, &ekey) {
                                bail!("{} does not match its key", hex(&ekey));
                            }
                            if seg.as_ref().is_none_or(|s| !s.fits(b.len())) {
                                let mut st = storage.lock().unwrap();
                                if let Some(mut old) = seg.take() {
                                    st.commit(&mut old)?;
                                }
                                seg = Some(st.new_segment()?);
                            }
                            seg.as_mut().unwrap().append(&ekey, b)?;
                            if job.archive.is_none() {
                                progress.done.fetch_add(b.len() as u64, Ordering::Relaxed);
                            }
                        }
                        if job.archive.is_some() {
                            progress.done.fetch_add(job.blobs.iter().map(|b| b.2).sum(), Ordering::Relaxed);
                        }
                        storage.lock().unwrap().commit(seg.as_mut().unwrap())?;
                        let mut t = last_shown.lock().unwrap();
                        if t.elapsed().as_secs() >= 2 {
                            progress.show(false);
                            *t = Instant::now();
                        }
                    }
                })();
                if let Some(mut s) = seg {
                    let _ = storage.lock().unwrap().commit(&mut s);
                }
                if let Err(e) = result {
                    failed.store(true, Ordering::Relaxed);
                    errors.lock().unwrap().push(format!("{e:#}"));
                }
            });
        }
    });
    progress.show(true);
    let errors = errors.into_inner().unwrap();
    if let Some(e) = errors.first() {
        bail!("download failed ({} errors; finished blobs are kept, rerun to resume): {e}", errors.len());
    }
    Ok(storage.into_inner().unwrap())
}

/// Finds each missing blob in the CDN archive indices, fetching the indices into `Data/indices`.
fn locate(cdn: &Cdn, data: &Path, cdn_cfg: &HashMap<String, Vec<String>>, wanted: &HashSet<Hash>, jobs: usize) -> Result<(Vec<String>, HashMap<Hash, Source>)> {
    let archives = cdn_cfg.get("archives").cloned().unwrap_or_default();
    let dir = data.join("indices");
    fs::create_dir_all(&dir)?;
    let mut lists: Vec<String> = archives.clone();
    let file_index = cdn_cfg.get("file-index").and_then(|v| v.first()).cloned();
    lists.extend(file_index.clone());
    let found = Mutex::new(HashMap::<Hash, Source>::new());
    let next = AtomicU64::new(0);
    let fetched = AtomicU64::new(0);
    let errors = Mutex::new(Vec::<String>::new());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    let Some(name) = lists.get(i) else { break };
                    let path = dir.join(format!("{name}.index"));
                    let parsed = fs::read(&path).ok().and_then(|b| tact::archive_index(&b).ok());
                    let entries = match parsed {
                        Some(e) => e,
                        None => match cdn.fetch("data", name, ".index", None).and_then(|b| {
                            let e = tact::archive_index(&b)?;
                            write_atomic(&path, &b)?;
                            Ok(e)
                        }) {
                            Ok(e) => {
                                fetched.fetch_add(1, Ordering::Relaxed);
                                e
                            }
                            Err(e) => {
                                errors.lock().unwrap().push(format!("{name}.index: {e:#}"));
                                continue;
                            }
                        },
                    };
                    let is_loose = i >= archives.len();
                    let mut f = found.lock().unwrap();
                    for (k, size, off) in entries {
                        if wanted.contains(&k) {
                            let src = if is_loose { Source::Loose } else { Source::Archive { archive: i, offset: off as u64, size: size as u64 } };
                            // An archive copy beats a loose one.
                            if matches!(src, Source::Archive { .. }) || !f.contains_key(&k) {
                                f.insert(k, src);
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = errors.into_inner().unwrap().first() {
        bail!("{e}");
    }
    let n = fetched.load(Ordering::Relaxed);
    if n > 0 {
        eprintln!("  fetched {n} archive indices");
    }
    Ok((archives, found.into_inner().unwrap()))
}

fn region_tags(region: &str) -> String {
    match region {
        "us" => "US? acct-USA? geoip-US?".to_owned(),
        r => format!("{}?", r.to_uppercase()),
    }
}

/// Puts `live`'s row into `.build.info`, keeping other products' rows.
fn write_build_info(root: &Path, live: &Live, opts: &Options, build_key: &str) -> Result<()> {
    const HEADER: &str = "Branch!STRING:0|Active!DEC:1|Build Key!HEX:16|CDN Key!HEX:16|Install Key!HEX:16|IM Size!DEC:4|CDN Path!STRING:0|CDN Hosts!STRING:0|CDN Servers!STRING:0|Tags!STRING:0|Armadillo!STRING:0|Last Activated!STRING:0|Version!STRING:0|KeyRing!HEX:16|Product!STRING:0";
    let path = root.join(".build.info");
    let old = fs::read_to_string(&path).unwrap_or_default();
    let header = old.lines().next().filter(|h| h.contains("Product!")).unwrap_or(HEADER).to_owned();
    let cols: Vec<String> = header.split('|').map(|c| c.split('!').next().unwrap().to_owned()).collect();
    let base = format!("Windows {} {} {}", opts.arch, region_tags(&opts.region), opts.locale);
    let mine: HashMap<&str, String> = HashMap::from([
        ("Branch", live.region.clone()),
        ("Active", "1".to_owned()),
        ("Build Key", build_key.to_owned()),
        ("CDN Key", live.cdn_config.clone()),
        ("CDN Path", live.cdn_path.clone()),
        ("CDN Hosts", live.cdn_hosts.join(" ")),
        ("CDN Servers", live.cdn_servers.clone()),
        ("Tags", format!("{base} speech?:{base} text?")),
        ("Version", live.version.clone()),
        ("KeyRing", live.key_ring.clone()),
        ("Product", live.product.clone()),
    ]);
    let mut out = header.clone() + "\n";
    let mut rows: Vec<HashMap<String, String>> =
        tact::parse_table(&old).into_iter().filter(|r| r.get("Product").map(String::as_str) != Some(live.product.as_str())).collect();
    rows.push(mine.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
    for r in rows {
        let line: Vec<&str> = cols.iter().map(|c| r.get(c).map(String::as_str).unwrap_or("")).collect();
        out += &line.join("|");
        out += "\n";
    }
    write_atomic(&path, out.as_bytes())
}

fn build_info_row(root: &Path, code: &str) -> Option<HashMap<String, String>> {
    let text = fs::read_to_string(root.join(".build.info")).ok()?;
    tact::parse_table(&text).into_iter().find(|r| r.get("Product").map(String::as_str) == Some(code))
}

/// The product's version in `.build.info`, if installed.
pub fn installed_version(root: &Path, code: &str) -> Option<String> {
    build_info_row(root, code)?.get("Version").cloned()
}

fn pair(cfg: &HashMap<String, Vec<String>>, key: &str) -> Result<(Hash, Option<Hash>)> {
    let v = cfg.get(key).with_context(|| format!("build config has no {key}"))?;
    Ok((unhex(&v[0])?, v.get(1).map(|s| unhex(s)).transpose()?))
}

/// What one product's live build needs.
struct Plan {
    p: &'static Product,
    live: Live,
    cdn: Cdn,
    cdn_cfg: HashMap<String, Vec<String>>,
    /// Blobs the client reads from storage.
    needed: HashSet<Hash>,
    files: Vec<tact::InstallFile>,
    file_ekeys: HashMap<Hash, Hash>,
}

impl Plan {
    /// Loose files whose copy in the product folder is missing or stale, as encoding keys.
    fn stale_files(&self, root: &Path) -> Result<HashSet<Hash>> {
        let dir = root.join(self.p.folder);
        let mut stale = HashSet::new();
        for f in &self.files {
            if !fs::read(local_path(&dir, &f.path)).is_ok_and(|b| md5(&b) == f.ckey) {
                stale.insert(*self.file_ekeys.get(&f.ckey).with_context(|| format!("{}: not in encoding", f.path))?);
            }
        }
        Ok(stale)
    }
}

/// Reads the build's manifests (from `source` where it has them, else the CDN) and works out what it needs.
fn plan(root: &Path, p: &'static Product, live: Live, opts: &Options, source: &Storage) -> Result<Plan> {
    let cdn = Cdn::new(&live);
    let data = root.join("Data");
    let build_cfg = tact::parse_config(&String::from_utf8_lossy(&config(&cdn, &data, &live.build_config)?));
    let cdn_cfg = tact::parse_config(&String::from_utf8_lossy(&config(&cdn, &data, &live.cdn_config)?));
    if !live.key_ring.is_empty() {
        config(&cdn, &data, &live.key_ring)?;
    }
    let storage = source;

    let wanted_tags = vec!["Windows".to_owned(), opts.arch.clone(), opts.locale.clone(), opts.region.to_uppercase(), "speech".to_owned(), "text".to_owned()];
    let (_, enc_ekey) = pair(&build_cfg, "encoding")?;
    let enc_ekey = enc_ekey.context("build config has no encoding key")?;
    let encoding = blte_decode(&blob(&cdn, &storage, &enc_ekey)?).context("encoding")?;

    let (install_ckey, install_ekey) = pair(&build_cfg, "install")?;
    let (download_ckey, download_ekey) = pair(&build_cfg, "download")?;
    let (root_ckey, _) = pair(&build_cfg, "root")?;
    let first = HashSet::from([install_ckey, download_ckey, root_ckey]);
    let first = tact::encoding_lookup(&encoding, &first)?;
    let lookup = |c: &Hash, e: Option<Hash>| e.or_else(|| first.get(c).copied()).with_context(|| format!("{} not in encoding", hex(c)));
    let install_ekey = lookup(&install_ckey, install_ekey)?;
    let download_ekey = lookup(&download_ckey, download_ekey)?;
    let root_ekey = lookup(&root_ckey, None)?;

    let (_, files) = tact::install_manifest(&blte_decode(&blob(&cdn, &storage, &install_ekey)?)?, &wanted_tags)?;
    if !files.iter().any(|f| f.path.to_lowercase().ends_with(".exe") && f.path.to_lowercase().starts_with("wow")) {
        bail!("no game executable for Windows {} {}: wrong --arch or --locale?", opts.arch, opts.locale);
    }
    let file_ckeys: HashSet<Hash> = files.iter().map(|f| f.ckey).collect();
    let file_ekeys = tact::encoding_lookup(&encoding, &file_ckeys)?;
    let downloads = tact::download_manifest(&blte_decode(&blob(&cdn, &storage, &download_ekey)?)?, &wanted_tags)?;

    // Everything the client reads from storage: the build's own manifests, the game data, loose files.
    // (The Agent doesn't keep the `size` manifest locally.)
    let mut needed: HashSet<Hash> = downloads.iter().map(|d| d.ekey).collect();
    needed.extend([enc_ekey, install_ekey, download_ekey, root_ekey]);
    for (k, v) in &build_cfg {
        if k.starts_with("vfs-") && v.len() == 2 {
            needed.extend(unhex(&v[1]).ok());
        }
    }
    eprintln!("  {}: {} blobs, {} loose files", p.code, needed.len(), files.len());
    Ok(Plan { p, live, cdn, cdn_cfg, needed, files, file_ekeys })
}

/// In the WoW folder once wowupd has built `Data/data` itself. The client rejects a Battle.net store
/// with wowupd's segments mixed in ("Missing segment header ... repair needed"), but accepts one built
/// whole, so wowupd adopts an existing store by copying it into a fresh one.
const STORE_MARKER: &str = ".wowupd-store";
/// Where a Battle.net store goes once adopted, until its owner removes it.
const OLD_STORE: &str = "data.battlenet";
const NEW_STORE: &str = "data.wowupd";

fn is_current(root: &Path, p: &Product, live: &Live, opts: &Options) -> bool {
    let Some(row) = build_info_row(root, p.code) else { return false };
    row.get("Version") == Some(&live.version) && row.get("Tags").is_some_and(|t| t.starts_with(&format!("Windows {} ", opts.arch)))
}

/// Brings `products` up to their live builds; returns whether anything changed. Unless `opts.verify`,
/// a product whose `.build.info` row names the live version and architecture is taken as current.
pub fn update(root: &Path, products: &[&'static Product], opts: &Options) -> Result<bool> {
    let data = root.join("Data");
    let store_dir = data.join("data");
    let ours = root.join(STORE_MARKER).exists() || !store_dir.exists();
    let mut todo = Vec::new();
    for &p in products {
        let live = live(p.code, &opts.region)?;
        let installed = installed_version(root, p.code);
        eprintln!("{} ({}): live {}, installed {}", p.name, p.code, live.version, installed.as_deref().unwrap_or("none"));
        if ours && is_current(root, p, &live, opts) && !opts.verify {
            eprintln!("  up to date");
            continue;
        }
        todo.push((p, live));
    }
    if todo.is_empty() {
        return Ok(false);
    }
    if !ours {
        if data.join(OLD_STORE).exists() {
            bail!("{} already exists; remove it (an earlier adopted Battle.net store) first", data.join(OLD_STORE).display());
        }
        // The new store must hold every product the folder has, not just the ones asked for.
        for p in PRODUCTS {
            if !todo.iter().any(|(t, _)| t.code == p.code) && installed_version(root, p.code).is_some() {
                eprintln!("{} ({}): installed, so it moves into the new store too", p.name, p.code);
                todo.push((p, live(p.code, &opts.region)?));
            }
        }
    }
    let source = Storage::open(&store_dir)?;
    eprintln!("storage: {} entries{}", source.entries.len(), if ours { "" } else { " (Battle.net's; wowupd will build its own)" });
    let plans = todo.into_iter().map(|(p, live)| plan(root, p, live, opts, &source)).collect::<Result<Vec<_>>>()?;
    let mut wanted: HashSet<Hash> = HashSet::new();
    for pl in &plans {
        wanted.extend(pl.needed.iter().copied());
        wanted.extend(pl.stale_files(root)?);
    }

    let store = if ours {
        let missing: HashSet<Hash> = wanted.iter().filter(|k| !source.has(k)).copied().collect();
        eprintln!("{} blobs to fetch", missing.len());
        let Some(store) = fetch(root, source, &plans, &missing, opts)? else { return Ok(false) };
        store
    } else {
        let mut fresh = Storage::open(&data.join(NEW_STORE))?;
        let copy: Vec<Hash> = wanted.iter().filter(|k| !fresh.has(k) && source.has(k)).copied().collect();
        let missing: HashSet<Hash> = wanted.iter().filter(|k| !fresh.has(k) && !source.has(k)).copied().collect();
        eprintln!("new store: {} blobs to copy from the old one, {} to fetch", copy.len(), missing.len());
        if !opts.check_only {
            copy_blobs(&source, &mut fresh, &copy)?;
        }
        let Some(mut fresh) = fetch(root, fresh, &plans, &missing, opts)? else { return Ok(false) };
        fresh.finish()?;
        drop(source);
        fs::rename(&store_dir, data.join(OLD_STORE))?;
        fs::rename(data.join(NEW_STORE), &store_dir)?;
        fs::write(root.join(STORE_MARKER), "Data/data was built by wowupd\n")?;
        eprintln!("the Battle.net store is now {}; delete it when everything works", data.join(OLD_STORE).display());
        Storage::open(&store_dir)?
    };

    for pl in &plans {
        install_files(root, pl, &store, opts)?;
    }
    Ok(true)
}

/// Downloads `missing` into `store` and writes its indices; None in check mode.
fn fetch(root: &Path, mut store: Storage, plans: &[Plan], missing: &HashSet<Hash>, opts: &Options) -> Result<Option<Storage>> {
    let data = root.join("Data");
    let mut left = missing.clone();
    // Each build's CDN config lists its own archives; look in each in turn.
    let mut found = Vec::new();
    for pl in plans {
        if left.is_empty() {
            break;
        }
        let (archives, sources) = locate(&pl.cdn, &data, &pl.cdn_cfg, &left, opts.jobs)?;
        left.retain(|k| !sources.contains_key(k));
        found.push((pl, archives, sources));
    }
    if let (Some(first), false) = (plans.first(), left.is_empty()) {
        // Not in any archive: try them as loose CDN files.
        found.push((first, Vec::new(), left.iter().map(|k| (*k, Source::Loose)).collect()));
    }
    let bytes: u64 = found.iter().flat_map(|f| f.2.values()).map(|s| if let Source::Archive { size, .. } = s { *size } else { 0 }).sum();
    let loose = found.iter().flat_map(|f| f.2.values()).filter(|s| matches!(s, Source::Loose)).count();
    if !missing.is_empty() {
        eprintln!("download: {:.2} GiB from archives, plus {loose} loose blobs", bytes as f64 / (1u64 << 30) as f64);
    }
    if opts.check_only {
        return Ok(None);
    }
    for (pl, archives, sources) in found {
        store = download(&pl.cdn, store, &archives, &sources, opts.jobs)?;
    }
    store.finish()?;
    Ok(Some(store))
}

/// Copies blobs between stores, keeping their bytes.
fn copy_blobs(from: &Storage, to: &mut Storage, keys: &[Hash]) -> Result<()> {
    let progress = Progress { done: AtomicU64::new(0), total: 0, started: Instant::now(), tty: std::io::stderr().is_terminal() };
    let mut seg = to.new_segment()?;
    let mut last = Instant::now();
    for (i, k) in keys.iter().enumerate() {
        let b = from.read(k)?;
        if !seg.fits(b.len()) {
            to.commit(&mut seg)?;
            seg = to.new_segment()?;
        }
        seg.append(k, &b)?;
        progress.done.fetch_add(b.len() as u64, Ordering::Relaxed);
        if i % 20000 == 19999 {
            to.commit(&mut seg)?;
        }
        if last.elapsed().as_secs() >= 2 {
            eprint!("\r  copied {} of {} blobs, {:.2} GiB  ", i + 1, keys.len(), progress.done.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64);
            last = Instant::now();
        }
    }
    to.commit(&mut seg)?;
    if !keys.is_empty() {
        eprintln!("\r  copied {} blobs, {:.2} GiB          ", keys.len(), progress.done.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64);
    }
    Ok(())
}

/// Writes a product's loose files, `.flavor.info` and `.build.info` row.
fn install_files(root: &Path, pl: &Plan, store: &Storage, opts: &Options) -> Result<()> {
    let dir = root.join(pl.p.folder);
    let mut written = 0;
    for f in &pl.files {
        let dest = local_path(&dir, &f.path);
        if fs::read(&dest).is_ok_and(|b| md5(&b) == f.ckey) {
            continue;
        }
        let ekey = pl.file_ekeys.get(&f.ckey).with_context(|| format!("{}: not in encoding", f.path))?;
        let content = blte_decode(&store.read(ekey)?).with_context(|| f.path.clone())?;
        if md5(&content) != f.ckey {
            bail!("{}: content does not match its key", f.path);
        }
        write_atomic(&dest, &content)?;
        written += 1;
    }
    if written > 0 {
        eprintln!("  wrote {written} loose files to {}", dir.display());
    }
    write_atomic(&dir.join(".flavor.info"), format!("Product Flavor!STRING:0\n{}\n", pl.p.code).as_bytes())?;
    write_build_info(root, &pl.live, opts, &pl.live.build_config)?;
    eprintln!("{} is at {}", pl.p.name, pl.live.version);
    Ok(())
}

pub fn status(root: &Path, region: &str) -> Result<()> {
    for p in PRODUCTS {
        let installed = installed_version(root, p.code);
        let folder = root.join(p.folder).is_dir();
        let live = live(p.code, region).map(|l| l.version).unwrap_or_else(|e| format!("? ({e})"));
        let state = match &installed {
            Some(v) if *v == live => "up to date",
            Some(_) => "update available",
            None if folder => "folder present, not in .build.info",
            None => "not installed",
        };
        println!("{:<24} {:<16} installed {:<16} live {:<16} {state}", p.name, p.code, installed.as_deref().unwrap_or("-"), live);
    }
    Ok(())
}
