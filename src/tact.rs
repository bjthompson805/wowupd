//! Blizzard's TACT formats: version-service tables, configs, BLTE, the encoding table, the install and
//! download manifests, and CDN archive indices. See wowdev.wiki/TACT.

use anyhow::{Context, Result, bail, ensure};
use flate2::read::ZlibDecoder;
use md5::{Digest, Md5};
use std::collections::{HashMap, HashSet};
use std::io::Read;

pub type Hash = [u8; 16];

pub fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Hash> {
    ensure!(s.len() == 32, "bad hash {s:?}");
    let mut h = [0u8; 16];
    for i in 0..16 {
        h[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).with_context(|| format!("bad hash {s:?}"))?;
    }
    Ok(h)
}

pub fn md5(b: &[u8]) -> Hash {
    Md5::digest(b).into()
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |v, &x| (v << 8) | x as u64)
}

/// A `Name!TYPE:n|...` table from the version service or `.build.info`.
pub fn parse_table(text: &str) -> Vec<HashMap<String, String>> {
    let mut lines = text.lines().filter(|l| !l.is_empty() && !l.starts_with('#'));
    let Some(head) = lines.next() else { return Vec::new() };
    let cols: Vec<String> = head.split('|').map(|c| c.split('!').next().unwrap().to_owned()).collect();
    lines.map(|l| cols.iter().cloned().zip(l.split('|').map(str::to_owned)).collect()).collect()
}

/// A `key = value` config (build or CDN config).
pub fn parse_config(text: &str) -> HashMap<String, Vec<String>> {
    text.lines()
        .filter_map(|l| l.split_once(" = "))
        .map(|(k, v)| (k.trim().to_owned(), v.split_whitespace().map(str::to_owned).collect()))
        .collect()
}

/// The encoding key covers the BLTE header when the blob is chunked, else the whole blob.
pub fn blte_key_matches(blob: &[u8], ekey: &Hash) -> bool {
    if blob.len() < 8 || &blob[..4] != b"BLTE" {
        return false;
    }
    let hsize = be(&blob[4..8]) as usize;
    if hsize == 0 { md5(blob) == *ekey } else { hsize <= blob.len() && md5(&blob[..hsize]) == *ekey }
}

pub fn blte_decode(blob: &[u8]) -> Result<Vec<u8>> {
    ensure!(blob.len() >= 8 && &blob[..4] == b"BLTE", "not a BLTE blob");
    let hsize = be(&blob[4..8]) as usize;
    let mut chunks = Vec::new();
    let mut pos;
    if hsize == 0 {
        chunks.push(blob.len() - 8);
        pos = 8;
    } else {
        ensure!(blob.len() >= 12 && hsize <= blob.len(), "short BLTE header");
        let n = be(&blob[9..12]) as usize;
        for i in 0..n {
            let at = 12 + 24 * i;
            chunks.push(be(&blob[at..at + 4]) as usize);
        }
        pos = hsize;
    }
    let mut out = Vec::new();
    for len in chunks {
        let c = blob.get(pos..pos + len).context("BLTE chunk past the end")?;
        pos += len;
        match c.first() {
            Some(b'N') => out.extend_from_slice(&c[1..]),
            Some(b'Z') => {
                ZlibDecoder::new(&c[1..]).read_to_end(&mut out)?;
            }
            Some(b'F') => out.extend(blte_decode(&c[1..])?),
            Some(b'E') => bail!("encrypted BLTE chunk"),
            m => bail!("unknown BLTE chunk mode {m:?}"),
        }
    }
    Ok(out)
}

/// Content key → encoding key, for the content keys in `wanted`.
pub fn encoding_lookup(data: &[u8], wanted: &HashSet<Hash>) -> Result<HashMap<Hash, Hash>> {
    ensure!(data.len() >= 22 && &data[..2] == b"EN" && data[3] == 16 && data[4] == 16, "bad encoding header");
    let cpage = be(&data[5..7]) as usize * 1024;
    let ccount = be(&data[9..13]) as usize;
    let espec = be(&data[18..22]) as usize;
    let mut pos = 22 + espec + ccount * 32;
    let mut out = HashMap::new();
    for _ in 0..ccount {
        let page = data.get(pos..pos + cpage).context("encoding page past the end")?;
        pos += cpage;
        let mut at = 0;
        while at + 22 <= page.len() {
            let n = page[at] as usize;
            if n == 0 {
                break;
            }
            let ckey: Hash = page[at + 6..at + 22].try_into().unwrap();
            if wanted.contains(&ckey) {
                out.insert(ckey, page[at + 22..at + 38].try_into().unwrap());
            }
            at += 22 + 16 * n;
        }
    }
    Ok(out)
}

pub struct Tag {
    pub name: String,
    pub kind: u16,
    pub mask: Vec<u8>,
}

impl Tag {
    fn has(&self, i: usize) -> bool {
        self.mask.get(i / 8).is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
    }
}

fn read_tags(d: &[u8], pos: &mut usize, count: usize, entries: usize) -> Result<Vec<Tag>> {
    let mlen = entries.div_ceil(8);
    let mut tags = Vec::new();
    for _ in 0..count {
        let end = *pos + d[*pos..].iter().position(|&b| b == 0).context("unterminated tag")?;
        let name = String::from_utf8_lossy(&d[*pos..end]).into_owned();
        let kind = be(&d[end + 1..end + 3]) as u16;
        let mask = d.get(end + 3..end + 3 + mlen).context("short tag mask")?.to_vec();
        *pos = end + 3 + mlen;
        tags.push(Tag { name, kind, mask });
    }
    Ok(tags)
}

/// Tags of this kind name opt-in content (HighRes, Alternate, ...): an entry carrying one is skipped
/// unless that tag is wanted.
const OPTIONAL_KIND: u16 = 0x4000;

/// Which entries the wanted tags select: for each tag kind with a wanted tag, the entry must carry one
/// of the wanted tags of that kind. Other kinds don't filter, except opt-in content.
pub fn select(tags: &[Tag], wanted: &[String], entries: usize) -> Vec<bool> {
    let is_wanted = |t: &Tag| wanted.iter().any(|w| w.eq_ignore_ascii_case(&t.name));
    let mut by_kind: HashMap<u16, Vec<&Tag>> = HashMap::new();
    for t in tags.iter().filter(|t| is_wanted(t)) {
        by_kind.entry(t.kind).or_default().push(t);
    }
    let opt_out: Vec<&Tag> = tags.iter().filter(|t| t.kind == OPTIONAL_KIND && !is_wanted(t)).collect();
    (0..entries)
        .map(|i| by_kind.values().all(|ts| ts.iter().any(|t| t.has(i))) && !opt_out.iter().any(|t| t.has(i)))
        .collect()
}

pub struct InstallFile {
    pub path: String,
    pub ckey: Hash,
}

pub fn install_manifest(d: &[u8], wanted: &[String]) -> Result<(Vec<Tag>, Vec<InstallFile>)> {
    ensure!(d.len() >= 10 && &d[..2] == b"IN" && d[3] == 16, "bad install manifest");
    let ntags = be(&d[4..6]) as usize;
    let n = be(&d[6..10]) as usize;
    let mut pos = 10;
    let tags = read_tags(d, &mut pos, ntags, n)?;
    let keep = select(&tags, wanted, n);
    let mut files = Vec::new();
    for k in keep {
        let end = pos + d[pos..].iter().position(|&b| b == 0).context("unterminated name")?;
        let path = String::from_utf8_lossy(&d[pos..end]).into_owned();
        let ckey = d.get(end + 1..end + 17).context("short install entry")?.try_into().unwrap();
        pos = end + 21;
        if k {
            files.push(InstallFile { path, ckey });
        }
    }
    Ok((tags, files))
}

pub struct DownloadFile {
    pub ekey: Hash,
}

pub fn download_manifest(d: &[u8], wanted: &[String]) -> Result<Vec<DownloadFile>> {
    ensure!(d.len() >= 11 && &d[..2] == b"DL" && d[3] == 16, "bad download manifest");
    let version = d[2];
    let has_checksum = d[4] != 0;
    let n = be(&d[5..9]) as usize;
    let ntags = be(&d[9..11]) as usize;
    let mut pos = 11;
    let mut flag_bytes = 0;
    if version >= 2 {
        flag_bytes = d[11] as usize;
        pos = 12;
    }
    if version >= 3 {
        pos += 4; // base priority and an unknown field
    }
    let entry = 16 + 5 + 1 + if has_checksum { 4 } else { 0 } + flag_bytes;
    let start = pos;
    pos += n * entry;
    let tags = read_tags(d, &mut pos, ntags, n)?;
    let keep = select(&tags, wanted, n);
    let mut files = Vec::new();
    for (i, k) in keep.into_iter().enumerate() {
        if !k {
            continue;
        }
        let e = &d[start + i * entry..start + (i + 1) * entry];
        files.push(DownloadFile { ekey: e[..16].try_into().unwrap() });
    }
    Ok(files)
}

/// A CDN archive index: (encoding key, size, offset) per blob. Loose-file indices have no offsets.
pub fn archive_index(d: &[u8]) -> Result<Vec<(Hash, u32, u32)>> {
    ensure!(d.len() >= 28, "short archive index");
    let f = &d[d.len() - 28..];
    let (block, off_bytes, size_bytes, key_bytes) = (f[11] as usize * 1024, f[12] as usize, f[13] as usize, f[14] as usize);
    let count = u32::from_le_bytes(f[16..20].try_into().unwrap()) as usize;
    ensure!(key_bytes == 16 && block > 0, "unsupported archive index layout");
    let entry = key_bytes + size_bytes + off_bytes;
    let mut out = Vec::with_capacity(count);
    let mut b = 0;
    while out.len() < count && b + block <= d.len() {
        let mut at = b;
        while at + entry <= b + block && out.len() < count {
            let e = &d[at..at + entry];
            if e[..16].iter().all(|&x| x == 0) {
                break;
            }
            let size = be(&e[16..16 + size_bytes]) as u32;
            let off = be(&e[16 + size_bytes..entry]) as u32;
            out.push((e[..16].try_into().unwrap(), size, off));
            at += entry;
        }
        b += block;
    }
    ensure!(out.len() == count, "archive index holds {} of {count} entries", out.len());
    Ok(out)
}
