//! The client's local storage in `Data/data`: `data.NNN` segments holding BLTE blobs, each behind a 30-byte
//! header, and sixteen `.idx` files that map the first 9 bytes of each encoding key to its place.
//! Layouts follow wowdev.wiki/CASC, CascLib, and files written by Blizzard's Agent.

use crate::jenkins::{hashlittle, hashlittle2};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const HEADER_SIZE: u64 = 0x1e;
pub const KEY_BYTES: usize = 9;
const ENTRY_SIZE: usize = 18;
pub const SEGMENT_BITS: u32 = 30;
pub const SEGMENT_SIZE: u64 = 1 << SEGMENT_BITS;
pub const MAX_SEGMENTS: u32 = 1024;
const HEADER_SEED: u32 = 0x3d6be971;
/// The Agent's table for mixing a header's position into its second checksum.
const OFFSET_TABLE: [u32; 16] = [
    0x049396b8, 0x72a82a9b, 0xee626cca, 0x9917754f, 0x15de40b1, 0xf5a8a9b6, 0x421eac7e, 0xa9d55c9a,
    0x317fd40c, 0x04faf80d, 0x3d6be971, 0x52933cfd, 0x27f64b7d, 0xc6f5c11b, 0xd5757e3a, 0x6c388745,
];
/// The client appends updates after the sorted entries, from the next 64 KiB boundary on, in pages
/// of 0x200 bytes holding 24-byte slots (a guard hash, then the entry).
const UPDATE_ALIGN: usize = 0x10000;
const UPDATE_SECTION: usize = 0x10000;
const UPDATE_PAGE: usize = 0x200;
const UPDATE_SLOT: usize = 24;

pub type Key9 = [u8; KEY_BYTES];

pub fn key9(ekey: &[u8; 16]) -> Key9 {
    ekey[..KEY_BYTES].try_into().unwrap()
}

#[derive(Clone, Copy, Debug)]
pub struct Loc {
    pub segment: u32,
    pub offset: u64,
    /// Counts the 30-byte header.
    pub size: u32,
}

pub fn bucket(key: &[u8]) -> u8 {
    let h = key[..KEY_BYTES].iter().fold(0u8, |h, b| h ^ b);
    (h & 0xf) ^ (h >> 4)
}

/// The header in front of a blob of `blte_size` bytes whose header sits at `offset` in its segment.
pub fn entry_header(ekey: &[u8; 16], blte_size: u32, offset: u64) -> [u8; HEADER_SIZE as usize] {
    // The key, reversed; like the index, the Agent keeps only its first 9 bytes and zeroes the rest.
    let mut h = [0u8; HEADER_SIZE as usize];
    for i in 0..KEY_BYTES {
        h[15 - i] = ekey[i];
    }
    h[0x10..0x14].copy_from_slice(&(blte_size + HEADER_SIZE as u32).to_le_bytes());
    let check_a = hashlittle(&h[..0x16], HEADER_SEED);
    h[0x16..0x1a].copy_from_slice(&check_a.to_le_bytes());
    let mut folded = [0u8; 4];
    for i in 0..0x1a {
        folded[i & 3] ^= h[i];
    }
    let pos = (offset + HEADER_SIZE) as u32;
    let mixed = OFFSET_TABLE[(pos & 0xf) as usize] ^ pos;
    for j in 0..4 {
        let k = (0x1a + j) & 3;
        h[0x1a + j] = folded[k] ^ (mixed >> (8 * k)) as u8;
    }
    h
}

fn read_u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn entries_hash(body: &[u8]) -> u32 {
    let (mut pc, mut pb) = (0, 0);
    for e in body.chunks(ENTRY_SIZE) {
        (pc, pb) = hashlittle2(e, pc, pb);
    }
    pc
}

fn decode_entry(e: &[u8]) -> (Key9, Loc) {
    let key: Key9 = e[..KEY_BYTES].try_into().unwrap();
    let packed = ((e[9] as u64) << 32) | u32::from_be_bytes(e[10..14].try_into().unwrap()) as u64;
    let loc = Loc {
        segment: (packed >> SEGMENT_BITS) as u32,
        offset: packed & (SEGMENT_SIZE - 1),
        size: read_u32le(e, 14),
    };
    (key, loc)
}

/// Reads one index file: the sorted entries, then whatever the client appended to the update section.
pub fn read_index(data: &[u8], bucket_no: u8) -> Result<Vec<(Key9, Loc)>> {
    if data.len() < 0x28 || read_u32le(data, 0) != 16 || read_u32le(data, 4) != hashlittle(&data[8..24], 0) {
        bail!("bad index header");
    }
    let h = &data[8..24];
    if h[0] != 7 || h[2] != bucket_no || h[4] != 4 || h[5] != 5 || h[6] != KEY_BYTES as u8 || h[7] != SEGMENT_BITS as u8 {
        bail!("unsupported index layout {:02x?}", h);
    }
    let size = read_u32le(data, 0x20) as usize;
    let end = 0x28 + size;
    if end > data.len() || read_u32le(data, 0x24) != entries_hash(&data[0x28..end]) {
        bail!("index entries fail their hash");
    }
    let mut out: Vec<_> = data[0x28..end].chunks(ENTRY_SIZE).map(decode_entry).collect();
    let mut page = end.div_ceil(UPDATE_ALIGN) * UPDATE_ALIGN;
    'pages: while page + UPDATE_PAGE <= data.len() {
        let mut slot = page;
        while slot + UPDATE_SLOT <= page + UPDATE_PAGE {
            let guard = read_u32le(data, slot);
            if guard == 0 {
                break 'pages;
            }
            let e = &data[slot + 4..slot + 4 + ENTRY_SIZE + 1];
            if hashlittle(e, 0) | 0x8000_0000 != guard {
                break 'pages;
            }
            out.push(decode_entry(e));
            slot += UPDATE_SLOT;
        }
        page += UPDATE_PAGE;
    }
    Ok(out)
}

/// Writes a sorted version-7 index with an empty update section.
fn write_index(path: &Path, bucket_no: u8, entries: &[(Key9, Loc)]) -> Result<()> {
    let mut header = [0u8; 16];
    header[..8].copy_from_slice(&[7, 0, bucket_no, 0, 4, 5, KEY_BYTES as u8, SEGMENT_BITS as u8]);
    header[8..].copy_from_slice(&(SEGMENT_SIZE * 256).to_le_bytes());
    let mut body = Vec::with_capacity(entries.len() * ENTRY_SIZE);
    for (key, loc) in entries {
        body.extend_from_slice(key);
        let packed = ((loc.segment as u64) << SEGMENT_BITS) | loc.offset;
        body.push((packed >> 32) as u8);
        body.extend_from_slice(&(packed as u32).to_be_bytes());
        body.extend_from_slice(&loc.size.to_le_bytes());
    }
    let mut out = Vec::new();
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&hashlittle(&header, 0).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&entries_hash(&body).to_le_bytes());
    out.extend_from_slice(&body);
    out.resize(out.len().div_ceil(UPDATE_ALIGN) * UPDATE_ALIGN + UPDATE_SECTION, 0);
    let tmp = path.with_extension("idx.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&out)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn index_name(bucket_no: u8, version: u32) -> String {
    format!("{bucket_no:02x}{version:08x}.idx")
}

/// The newest index file of each bucket in `dir`, as (bucket, version).
fn current_indices(dir: &Path) -> Result<HashMap<u8, u32>> {
    let mut found = HashMap::new();
    if !dir.is_dir() {
        return Ok(found);
    }
    for e in fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if name.len() != 14 || !name.ends_with(".idx") {
            continue;
        }
        let (Ok(b), Ok(v)) = (u8::from_str_radix(&name[..2], 16), u32::from_str_radix(&name[2..10], 16)) else { continue };
        if b < 16 && found.get(&b).is_none_or(|&old| v > old) {
            found.insert(b, v);
        }
    }
    Ok(found)
}

fn segment_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("data.{n:03}"))
}

/// The open storage: every indexed entry, plus segments being appended to.
pub struct Storage {
    pub dir: PathBuf,
    pub entries: HashMap<Key9, Loc>,
    versions: HashMap<u8, u32>,
    /// Open once this run has written a blob, or when an earlier run left one.
    journal: Option<File>,
    next_segment: u32,
}

const JOURNAL: &str = "wowupd.journal";
const JOURNAL_RECORD: usize = 16 + 4 + 8 + 4;

impl Storage {
    /// Opens `Data/data`, reading the indices and replaying the journal of an interrupted run.
    pub fn open(dir: &Path) -> Result<Storage> {
        fs::create_dir_all(dir)?;
        let versions = current_indices(dir)?;
        let mut entries = HashMap::new();
        for (&b, &v) in &versions {
            let path = dir.join(index_name(b, v));
            let data = fs::read(&path)?;
            for (k, loc) in read_index(&data, b).with_context(|| path.display().to_string())? {
                entries.insert(k, loc);
            }
        }
        let mut max_segment = None::<u32>;
        for e in fs::read_dir(dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_prefix("data.").and_then(|n| n.parse::<u32>().ok()) {
                max_segment = max_segment.max(Some(n));
            }
        }
        let journal_path = dir.join(JOURNAL);
        let bytes = fs::read(&journal_path).unwrap_or_default();
        let journal = if journal_path.exists() { Some(OpenOptions::new().append(true).open(&journal_path)?) } else { None };
        let whole = bytes.len() / JOURNAL_RECORD * JOURNAL_RECORD;
        // Our segments always start after every existing one; journaled ends show how much of each is good.
        let mut ends: HashMap<u32, u64> = HashMap::new();
        for r in bytes[..whole].chunks(JOURNAL_RECORD) {
            let ekey: [u8; 16] = r[..16].try_into().unwrap();
            let segment = read_u32le(r, 16);
            let offset = u64::from_le_bytes(r[20..28].try_into().unwrap());
            let size = read_u32le(r, 28);
            entries.insert(key9(&ekey), Loc { segment, offset, size });
            let end = ends.entry(segment).or_default();
            *end = (*end).max(offset + size as u64);
        }
        if let Some(j) = journal.as_ref().filter(|_| whole != bytes.len()) {
            j.set_len(whole as u64)?;
        }
        let journaled_first = ends.keys().min().copied();
        if let (Some(first), Some(max)) = (journaled_first, max_segment) {
            // Segments at or past our first one hold only journaled data; cut off whatever came after.
            for n in first..=max {
                let p = segment_path(dir, n);
                match ends.get(&n) {
                    Some(&end) => OpenOptions::new().write(true).open(&p)?.set_len(end)?,
                    None if p.exists() => fs::remove_file(&p)?,
                    None => {}
                }
            }
        }
        let next_segment = max_segment.map_or(0, |m| m + 1).max(ends.keys().max().map_or(0, |m| m + 1));
        Ok(Storage { dir: dir.to_owned(), entries, versions, journal, next_segment })
    }

    /// Where the BLTE blob of an entry starts and how long it is. Most entries sit behind a 30-byte header
    /// naming their key; some (written by the game client itself) are the bare blob, and their size
    /// counts no header.
    fn blob_span(&self, ekey: &[u8; 16], f: &mut File, loc: &Loc) -> Option<(u64, usize)> {
        let mut h = [0u8; HEADER_SIZE as usize];
        f.seek(SeekFrom::Start(loc.offset)).ok()?;
        f.read_exact(&mut h).ok()?;
        if &h[..4] == b"BLTE" {
            return Some((loc.offset, loc.size as usize));
        }
        let named = (0..KEY_BYTES).all(|i| h[15 - i] == ekey[i]) && read_u32le(&h, 0x10) == loc.size;
        named.then_some((loc.offset + HEADER_SIZE, (loc.size as u64).checked_sub(HEADER_SIZE)? as usize))
    }

    /// Checks that the blob indexed for `ekey` is there. A headed entry must name the key; a bare one must
    /// hash to it. Catches entries indexed but never written, and stale indices.
    pub fn has(&self, ekey: &[u8; 16]) -> bool {
        let Some(loc) = self.entries.get(&key9(ekey)) else { return false };
        let Ok(mut f) = File::open(segment_path(&self.dir, loc.segment)) else { return false };
        let Some((start, len)) = self.blob_span(ekey, &mut f, loc) else { return false };
        if start != loc.offset {
            return true;
        }
        // A bare blob: its key is the MD5 of its BLTE header, or of all of it when it has no chunk table.
        let mut head = [0u8; 8];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut head).is_err() {
            return false;
        }
        let hsize = u32::from_be_bytes(head[4..8].try_into().unwrap()) as usize;
        let n = if hsize == 0 { len } else { hsize.min(len) };
        let mut b = vec![0u8; n];
        f.seek(SeekFrom::Start(start)).is_ok() && f.read_exact(&mut b).is_ok() && crate::tact::blte_key_matches(&b, ekey)
    }

    pub fn read(&self, ekey: &[u8; 16]) -> Result<Vec<u8>> {
        let loc = self.entries.get(&key9(ekey)).context("not in storage")?;
        let mut f = File::open(segment_path(&self.dir, loc.segment))?;
        let (start, len) = self.blob_span(ekey, &mut f, loc).context("entry does not hold this key")?;
        f.seek(SeekFrom::Start(start))?;
        let mut b = vec![0u8; len];
        f.read_exact(&mut b)?;
        Ok(b)
    }

    pub fn new_segment(&mut self) -> Result<Segment> {
        if self.next_segment >= MAX_SEGMENTS {
            bail!("storage is full ({MAX_SEGMENTS} segments)");
        }
        let n = self.next_segment;
        self.next_segment += 1;
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(segment_path(&self.dir, n))?;
        Ok(Segment { number: n, file, end: 0, pending: Vec::new() })
    }

    /// Syncs a segment and journals the blobs it took since the last commit.
    pub fn commit(&mut self, seg: &mut Segment) -> Result<()> {
        if seg.pending.is_empty() {
            return Ok(());
        }
        seg.file.sync_data()?;
        let mut rec = Vec::with_capacity(seg.pending.len() * JOURNAL_RECORD);
        for (ekey, offset, size) in seg.pending.drain(..) {
            rec.extend_from_slice(&ekey);
            rec.extend_from_slice(&seg.number.to_le_bytes());
            rec.extend_from_slice(&offset.to_le_bytes());
            rec.extend_from_slice(&size.to_le_bytes());
            self.entries.insert(key9(&ekey), Loc { segment: seg.number, offset, size });
        }
        if self.journal.is_none() {
            self.journal = Some(OpenOptions::new().append(true).create(true).open(self.dir.join(JOURNAL))?);
        }
        let j = self.journal.as_mut().unwrap();
        j.write_all(&rec)?;
        j.sync_data()?;
        Ok(())
    }

    /// Writes every bucket as a new index version, removes the old ones and the journal. The client's
    /// `shmem` caches index versions and free space, so it goes too; the client rebuilds it.
    /// Does nothing when no blob was added.
    pub fn finish(&mut self) -> Result<()> {
        if self.journal.is_none() {
            return Ok(());
        }
        let mut buckets: Vec<Vec<(Key9, Loc)>> = vec![Vec::new(); 16];
        for (k, loc) in &self.entries {
            buckets[bucket(k) as usize].push((*k, *loc));
        }
        for (b, mut list) in buckets.into_iter().enumerate() {
            let b = b as u8;
            list.sort_by(|x, y| x.0.cmp(&y.0));
            let old = self.versions.get(&b).copied();
            let version = old.map_or(1, |v| v + 1);
            write_index(&self.dir.join(index_name(b, version)), b, &list)?;
            if let Some(v) = old {
                fs::remove_file(self.dir.join(index_name(b, v)))?;
            }
            self.versions.insert(b, version);
        }
        let shmem = self.dir.join("shmem");
        if shmem.exists() {
            fs::remove_file(shmem)?;
        }
        self.journal = None;
        fs::remove_file(self.dir.join(JOURNAL))?;
        Ok(())
    }
}

pub struct Segment {
    pub number: u32,
    file: File,
    end: u64,
    pending: Vec<([u8; 16], u64, u32)>,
}

impl Segment {
    pub fn fits(&self, blte_size: usize) -> bool {
        self.end + HEADER_SIZE + blte_size as u64 <= SEGMENT_SIZE
    }

    pub fn append(&mut self, ekey: &[u8; 16], blte: &[u8]) -> Result<()> {
        let offset = self.end;
        self.file.write_all(&entry_header(ekey, blte.len() as u32, offset))?;
        self.file.write_all(blte)?;
        self.end += HEADER_SIZE + blte.len() as u64;
        self.pending.push((*ekey, offset, (HEADER_SIZE as usize + blte.len()) as u32));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    // Headers written by Blizzard's Agent: (key, entry size, offset, header).
    #[test]
    fn agent_headers() {
        for (key, size, offset, want) in [
            ("00005353f628445600", 9235, 691909257, "0000000000000000564428f653530000132400000000c94bf30821800bd3"),
            ("0001247736dba21f64", 7916, 85714509, "00000000000000641fa2db3677240100ec1e0000000036e675997b222b89"),
            ("000180d18404adcac0", 7097, 460179241, "00000000000000c0caad0484d1800100b91b000000008d5625c1bacf1c4f"),
        ] {
            let mut ekey = [0u8; 16];
            ekey[..9].copy_from_slice(&unhex(key));
            // The last 4 bytes come from a newer Agent than CascLib's formula (table and position mix);
            // they differ, and the client doesn't seem to check them.
            assert_eq!(entry_header(&ekey, size - HEADER_SIZE as u32, offset)[..0x1a], unhex(want)[..0x1a]);
        }
    }

    #[test]
    fn index_round_trip() {
        let dir = std::env::temp_dir().join(format!("wowupd-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut entries = Vec::new();
        for i in 0..1000u32 {
            let mut k = [0u8; 9];
            k[..4].copy_from_slice(&i.to_be_bytes());
            k[4] = bucket(&[0; 9]);
            if bucket(&k) == 3 {
                entries.push((k, Loc { segment: i % 1024, offset: i as u64 * 1000, size: i + 30 }));
            }
        }
        entries.sort_by(|x, y| x.0.cmp(&y.0));
        let p = dir.join("x.idx");
        write_index(&p, 3, &entries).unwrap();
        let back = read_index(&fs::read(&p).unwrap(), 3).unwrap();
        assert_eq!(back.len(), entries.len());
        for (a, b) in back.iter().zip(&entries) {
            assert_eq!(a.0, b.0);
            assert_eq!((a.1.segment, a.1.offset, a.1.size), (b.1.segment, b.1.offset, b.1.size));
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
