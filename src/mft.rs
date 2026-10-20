//! Direct `$MFT` scanning: the fast path that makes indexing a multi-million
//! file volume take a couple of seconds instead of minutes.
//!
//! How it works:
//!  1. read MFT record 0 and pull the run list out of its unnamed `$DATA`
//!     attribute, so a *fragmented* MFT is handled correctly;
//!  2. stream the MFT in 16 MB chunks (one positioned read per chunk);
//!  3. parse every 1 KB `FILE` record in parallel with rayon:
//!     undo the update sequence fixups, then walk the attribute list for
//!     `$STANDARD_INFORMATION` (timestamps) and `$FILE_NAME` (parent + name);
//!  4. only *names* and parent references are kept - full paths are rebuilt
//!     later from the parent chain, which is where the memory and speed win is.

use anyhow::{bail, Result};
use rayon::prelude::*;

use crate::volume::Volume;
use crate::winutil::push_utf16le;

pub const ROOT_RECORD: u32 = 5;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// NTFS update sequence arrays always work in 512 byte units.
const FIXUP_UNIT: usize = 512;
const MAX_RECORD_SIZE: usize = 4096;
/// Read granularity. Big enough to keep the disk queue busy, small enough to stay
/// cache friendly.
const CHUNK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub struct RawEntry {
    /// MFT record number of the file/directory itself.
    pub rec: u32,
    /// MFT record number of the parent directory.
    pub parent: u32,
    /// Offset into [`ScanOut::names`].
    pub name_off: u32,
    pub name_len: u16,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Default)]
pub struct ScanOut {
    /// Arena of UTF-8 file names.
    pub names: Vec<u8>,
    pub entries: Vec<RawEntry>,
    /// `$FILE_NAME` attributes found in attribute-list extension records,
    /// attached to their base record after the scan.
    ext_names: Vec<ExtName>,
    /// Parse statistics, for diagnostics only.
    pub stats: ScanStats,
}

/// A name that lives in an extension record and belongs to `base`.
#[derive(Clone, Copy)]
struct ExtName {
    base: u32,
    parent: u32,
    name_off: u32,
    name_len: u16,
}

#[derive(Default, Clone, Copy)]
pub struct ScanStats {
    pub records: u64,
    pub not_file_magic: u64,
    pub fixup_fail: u64,
    pub unused: u64,
    pub extension: u64,
    pub no_name: u64,
    pub names_seen: u64,
    pub names_dropped: u64,
    pub ext_names_seen: u64,
    pub ext_names_attached: u64,
    pub ext_names_orphan: u64,
}

impl ScanOut {
    fn absorb(&mut self, other: ScanOut) {
        let base = self.names.len() as u32;
        self.names.extend_from_slice(&other.names);
        self.entries.reserve(other.entries.len());
        for mut e in other.entries {
            e.name_off += base;
            self.entries.push(e);
        }
        self.ext_names.reserve(other.ext_names.len());
        for mut x in other.ext_names {
            x.name_off += base;
            self.ext_names.push(x);
        }
        self.stats.records += other.stats.records;
        self.stats.not_file_magic += other.stats.not_file_magic;
        self.stats.fixup_fail += other.stats.fixup_fail;
        self.stats.unused += other.stats.unused;
        self.stats.extension += other.stats.extension;
        self.stats.no_name += other.stats.no_name;
        self.stats.names_seen += other.stats.names_seen;
        self.stats.ext_names_seen += other.stats.ext_names_seen;
    }
}

#[inline]
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[inline]
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline]
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

/// Decode an NTFS data run list into `(start_lcn, cluster_count)` pairs.
fn decode_runs(bytes: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut prev: i64 = 0;
    while i < bytes.len() {
        let header = bytes[i];
        i += 1;
        if header == 0 {
            break;
        }
        let len_bytes = (header & 0x0F) as usize;
        let off_bytes = (header >> 4) as usize;
        if len_bytes == 0 || off_bytes == 0 || i + len_bytes + off_bytes > bytes.len() {
            break;
        }
        let mut len = 0u64;
        for k in 0..len_bytes {
            len |= (bytes[i + k] as u64) << (8 * k);
        }
        i += len_bytes;
        let mut raw = 0i64;
        for k in 0..off_bytes {
            raw |= (bytes[i + k] as i64) << (8 * k);
        }
        let shift = 64 - 8 * off_bytes;
        raw = (raw << shift) >> shift; // sign extend
        i += off_bytes;
        prev += raw;
        if len == 0 {
            break;
        }
        out.push((prev as u64, len));
    }
    out
}

/// Apply the update sequence array fixups into `dst`, returning false when the
/// record is corrupt or unused.
fn fixup(src: &[u8], dst: &mut [u8]) -> bool {
    let n = src.len();
    let usa_off = u16le(src, 4) as usize;
    let usa_count = u16le(src, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > n {
        return false;
    }
    dst[..n].copy_from_slice(src);
    let magic = u16le(src, usa_off);
    for i in 1..usa_count {
        let pos = i * FIXUP_UNIT;
        if pos < 2 || pos > n {
            return false;
        }
        let pos = pos - 2;
        if u16le(dst, pos) != magic {
            return false;
        }
        let fixed = u16le(src, usa_off + i * 2);
        dst[pos..pos + 2].copy_from_slice(&fixed.to_le_bytes());
    }
    true
}

/// Parse one MFT record, appending any file names found to `out`.
fn parse_record(raw: &[u8], rec_index: u32, out: &mut ScanOut) {
    out.stats.records += 1;
    if raw.len() < 0x30 || &raw[0..4] != b"FILE" {
        out.stats.not_file_magic += 1;
        return;
    }
    let mut scratch = [0u8; MAX_RECORD_SIZE];
    if raw.len() > MAX_RECORD_SIZE {
        return;
    }
    let n = raw.len();
    if !fixup(raw, &mut scratch[..n]) {
        out.stats.fixup_fail += 1;
        return;
    }
    let rec = &scratch[..n];

    let flags = u16le(rec, 0x16);
    if flags & 0x0001 == 0 {
        out.stats.unused += 1;
        return; // deleted / unused record
    }
    let attrs_off = u16le(rec, 0x14) as usize;
    if attrs_off < 0x18 || attrs_off >= n {
        return;
    }

    // Extension record: its `$FILE_NAME` attributes are extra hard links of the
    // base file. They are collected here and attached after the scan, because the
    // base record may be parsed in a different chunk (and by another thread).
    let base_ref = u64le(rec, 0x20) & 0x0000_FFFF_FFFF_FFFF;
    if base_ref != 0 {
        out.stats.extension += 1;
        collect_ext_names(rec, attrs_off, base_ref as u32, out);
        return;
    }
    let is_dir = flags & 0x0002 != 0;

    // Up to 4 hard links, plus a DOS 8.3 alias fallback.
    let mut names: [(u32, u32, u16); 4] = [(0, 0, 0); 4];
    let mut name_count = 0usize;
    let mut dos_name: Option<(u32, u32, u16)> = None;
    let mut size: u64 = 0;
    let mut fn_size: u64 = 0;
    let mut mtime: u64 = 0;
    let mut name_mtime: u64 = 0;

    let mut off = attrs_off;
    while off + 16 <= n {
        let ty = u32le(rec, off);
        if ty == ATTR_END {
            break;
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 16 || off + len > n {
            break;
        }
        let non_resident = rec[off + 8] != 0;
        let attr_name_len = rec[off + 9] as usize;
        let end = off + len;

        if !non_resident {
            let value = off + u16le(rec, off + 0x14) as usize;
            match ty {
                ATTR_STANDARD_INFORMATION => {
                    if value + 16 <= end {
                        mtime = u64le(rec, value + 8);
                    }
                }
                ATTR_FILE_NAME if attr_name_len == 0 => {
                    if value + 0x42 <= end {
                        let parent_ref = u64le(rec, value) & 0x0000_FFFF_FFFF_FFFF;
                        let name_chars = rec[value + 0x40] as usize;
                        let namespace = rec[value + 0x41];
                        let name_start = value + 0x42;
                        let name_end = name_start + name_chars * 2;
                        if name_end <= end && name_end <= n {
                            if name_mtime == 0 {
                                name_mtime = u64le(rec, value + 16);
                            }
                            if fn_size == 0 {
                                // $FILE_NAME RealSize: the last fallback when the
                                // unnamed $DATA lives in an attribute-list extension.
                                fn_size = u64le(rec, value + 0x30);
                            }
                            let parent = parent_ref as u32;
                            if parent_ref > u32::MAX as u64 {
                                off += len;
                                continue;
                            }
                            let start = out.names.len() as u32;
                            push_utf16le(&mut out.names, &rec[name_start..name_end]);
                            let stored = (out.names.len() as u32 - start) as u16;
                            let slot = (parent, start, stored);
                            out.stats.names_seen += 1;
                            match namespace {
                                2 => {
                                    if dos_name.is_none() {
                                        dos_name = Some(slot);
                                    }
                                }
                                _ => {
                                    if name_count < names.len() {
                                        names[name_count] = slot;
                                        name_count += 1;
                                    } else {
                                        out.stats.names_dropped += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                ATTR_DATA if attr_name_len == 0 && !is_dir => {
                    // Resident data: the value length *is* the file size.
                    if off + 0x14 <= end {
                        size = u32le(rec, off + 0x10) as u64;
                    }
                }
                _ => {}
            }
        } else if ty == ATTR_DATA && attr_name_len == 0 && !is_dir {
            // Non resident $DATA: RealSize at +0x30.
            if off + 0x38 <= end {
                size = u64le(rec, off + 0x30);
            }
        }
        off += len;
    }

    if name_count == 0 {
        if let Some(slot) = dos_name {
            names[0] = slot;
            name_count = 1;
        }
    }
    if name_count == 0 {
        out.stats.no_name += 1;
        return;
    }

    let mtime = if mtime != 0 { mtime } else { name_mtime };
    if !is_dir && size == 0 {
        size = fn_size;
    }
    for slot in names.iter().take(name_count) {
        out.entries.push(RawEntry {
            rec: rec_index,
            parent: slot.0,
            name_off: slot.1,
            name_len: slot.2,
            is_dir,
            size: if is_dir { 0 } else { size },
            mtime,
        });
    }
}

fn collect_ext_names(rec: &[u8], attrs_off: usize, base: u32, out: &mut ScanOut) {
    let n = rec.len();
    let mut off = attrs_off;
    while off + 16 <= n {
        let ty = u32le(rec, off);
        if ty == ATTR_END {
            break;
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 16 || off + len > n {
            break;
        }
        let non_resident = rec[off + 8] != 0;
        let attr_name_len = rec[off + 9] as usize;
        let end = off + len;
        if ty == ATTR_FILE_NAME && !non_resident && attr_name_len == 0 {
            let value = off + u16le(rec, off + 0x14) as usize;
            if value + 0x42 <= end {
                let parent_ref = u64le(rec, value) & 0x0000_FFFF_FFFF_FFFF;
                let name_chars = rec[value + 0x40] as usize;
                let namespace = rec[value + 0x41];
                let name_start = value + 0x42;
                let name_end = name_start + name_chars * 2;
                // Namespace 2 is the 8.3 alias - the Win32 name is already listed.
                if namespace != 2
                    && name_end <= end
                    && name_end <= n
                    && parent_ref <= u32::MAX as u64
                {
                    let start = out.names.len() as u32;
                    push_utf16le(&mut out.names, &rec[name_start..name_end]);
                    let stored = (out.names.len() as u32 - start) as u16;
                    out.ext_names.push(ExtName {
                        base,
                        parent: parent_ref as u32,
                        name_off: start,
                        name_len: stored,
                    });
                    out.stats.ext_names_seen += 1;
                }
            }
        }
        off += len;
    }
}

/// Give extension-record names a real entry, cloning the base entry's metadata.
fn attach_ext_names(out: &mut ScanOut) {
    if out.ext_names.is_empty() {
        return;
    }
    out.stats.ext_names_orphan = 0;
    let mut by_rec: Vec<(u32, u32)> = out
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.rec, i as u32))
        .collect();
    by_rec.sort_unstable_by_key(|&(r, _)| r);

    let mut extra: Vec<RawEntry> = Vec::with_capacity(out.ext_names.len());
    for x in &out.ext_names {
        let Ok(pos) = by_rec.binary_search_by_key(&x.base, |&(r, _)| r) else {
            out.stats.ext_names_orphan += 1;
            continue;
        };
        let base = out.entries[by_rec[pos].1 as usize];
        let candidate = &out.names[x.name_off as usize..x.name_off as usize + x.name_len as usize];
        let duplicate = out
            .entries
            .iter()
            .skip(by_rec[pos].1 as usize)
            .take_while(|e| e.rec == base.rec)
            .any(|e| {
                e.parent == x.parent
                    && &out.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
                        == candidate
            });
        if duplicate {
            continue;
        }
        extra.push(RawEntry {
            rec: base.rec,
            parent: x.parent,
            name_off: x.name_off,
            name_len: x.name_len,
            is_dir: base.is_dir,
            size: base.size,
            mtime: base.mtime,
        });
    }
    out.stats.ext_names_attached = extra.len() as u64;
    out.entries.extend(extra);
}

fn parse_chunk(buf: &[u8], rec_size: usize, base_rec: u32) -> ScanOut {
    buf.par_chunks_exact(rec_size)
        .enumerate()
        .fold(ScanOut::default, |mut acc, (i, rec)| {
            parse_record(rec, base_rec + i as u32, &mut acc);
            acc
        })
        .reduce(ScanOut::default, |mut a, b| {
            a.absorb(b);
            a
        })
}

/// Read the run list of `$MFT` itself out of MFT record 0.
fn mft_runs(vol: &Volume, rec_size: usize) -> Result<Vec<(u64, u64)>> {
    let mut buf = vec![0u8; rec_size];
    vol.read_at(vol.info.mft_start_byte, &mut buf)?;
    let mut scratch = [0u8; MAX_RECORD_SIZE];
    if buf.len() > MAX_RECORD_SIZE || !fixup(&buf, &mut scratch[..buf.len()]) {
        bail!("无法解析 $MFT 记录 0");
    }
    let rec = &scratch[..buf.len()];
    let attrs_off = u16le(rec, 0x14) as usize;
    let n = rec.len();
    let mut off = attrs_off;
    while off + 16 <= n {
        let ty = u32le(rec, off);
        if ty == ATTR_END {
            break;
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 16 || off + len > n {
            break;
        }
        if ty == ATTR_DATA && rec[off + 8] != 0 && rec[off + 9] == 0 {
            let run_off = u16le(rec, off + 0x20) as usize;
            if off + run_off < off + len {
                let runs = decode_runs(&rec[off + run_off..off + len]);
                if !runs.is_empty() {
                    return Ok(runs);
                }
            }
        }
        off += len;
    }
    // Fall back to the contiguous layout reported by the volume.
    let clusters = vol.info.mft_valid_bytes / vol.info.bytes_per_cluster;
    if clusters == 0 {
        bail!("$MFT 数据属性为空");
    }
    Ok(vec![(
        vol.info.mft_start_byte / vol.info.bytes_per_cluster,
        clusters,
    )])
}

/// Scan the whole volume MFT. `progress` is called with the number of MFT bytes
/// processed so far.
pub fn scan(vol: &Volume, progress: &dyn Fn(u64)) -> Result<ScanOut> {
    let info = vol.info;
    let rec_size = info.bytes_per_record as usize;
    if !(512..=MAX_RECORD_SIZE).contains(&rec_size) || !rec_size.is_power_of_two() {
        bail!("不支持的 MFT 记录大小: {}", rec_size);
    }
    if info.total_records == 0 {
        bail!("卷 {} 的 MFT 为空", vol.letter);
    }

    let runs = mft_runs(vol, rec_size)?;
    let cluster = info.bytes_per_cluster;
    let total_bytes = info.mft_valid_bytes;

    let mut out = ScanOut::default();
    out.entries.reserve(info.total_records as usize);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut stream_off: u64 = 0;

    'outer: for (lcn, clusters) in runs {
        let run_bytes = clusters.saturating_mul(cluster);
        let mut run_off: u64 = 0;
        while run_off < run_bytes {
            if stream_off >= total_bytes {
                break 'outer;
            }
            let remaining_run = run_bytes - run_off;
            let remaining_data = total_bytes - stream_off;
            let mut take = (CHUNK_SIZE as u64).min(remaining_run).min(remaining_data);
            take -= take % rec_size as u64;
            if take == 0 {
                break;
            }
            let vol_off = lcn * cluster + run_off;
            vol.read_at(vol_off, &mut buf[..take as usize])?;
            let base_rec = (stream_off / rec_size as u64) as u32;
            let part = parse_chunk(&buf[..take as usize], rec_size, base_rec);
            out.absorb(part);
            run_off += take;
            stream_off += take;
            progress(stream_off);
        }
    }
    attach_ext_names(&mut out);
    Ok(out)
}
