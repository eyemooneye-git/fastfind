//! In-memory index: names + parent links, with full paths rebuilt on demand.
//!
//! Storing only the file name plus a parent record number is what keeps a
//! 2 million file volume at ~150 MB and makes indexing mostly a memory copy
//! problem. Directory prefix paths are memoised once per directory record, so
//! even the memo table is cheap.

use std::collections::HashMap;

use crate::mft::{RawEntry, ScanOut, ROOT_RECORD};
use crate::winutil::ascii_lower_str;

/// Sentinel for "not present" in the record indexed tables.
pub const SENT: u32 = u32::MAX;

#[derive(Clone, Copy, Default)]
pub struct Entry {
    pub parent: u32,
    pub name_off: u32,
    pub prefix_off: u32,
    pub name_len: u16,
    pub prefix_len: u16,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}

pub struct VolumeIndex {
    pub drive: char,
    /// UTF-8 arena of file names (original case).
    pub names: Vec<u8>,
    /// UTF-8 arena of directory prefix paths, without a trailing separator.
    /// The first entry is the volume prefix, e.g. `C:`.
    pub paths: Vec<u8>,
    pub entries: Vec<Entry>,
    /// record number -> parent record number (directories).
    pub rec_parent: Vec<u32>,
    /// lowercase full directory path -> record number.
    pub dir_map: HashMap<String, u32>,
    pub scanned_ms: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub total_bytes: u64,
    pub skipped: u64,
}

impl VolumeIndex {
    pub fn build(drive: char, scan: ScanOut, scanned_ms: u64) -> Self {
        let ScanOut {
            names,
            entries: raw,
            ..
        } = scan;

        let rec_count = raw.iter().map(|e| e.rec as usize + 1).max().unwrap_or(0);
        let mut rec_parent = vec![SENT; rec_count];
        let mut rec_entry = vec![SENT; rec_count];
        for (i, e) in raw.iter().enumerate() {
            let r = e.rec as usize;
            if r < rec_count && e.is_dir && rec_entry[r] == SENT {
                rec_entry[r] = i as u32;
                rec_parent[r] = e.parent;
            }
        }

        let root_prefix = format!("{}:", drive);
        let mut paths: Vec<u8> = Vec::with_capacity(rec_count * 8 + root_prefix.len());
        let root_off = 0u32;
        let root_len = root_prefix.len() as u32;
        paths.extend_from_slice(root_prefix.as_bytes());
        let root_range = (root_off, root_len);

        // (offset, len) per record of that directory's own path. SENT = unknown.
        let mut memo = vec![(SENT, 0u32); rec_count];
        if (ROOT_RECORD as usize) < rec_count {
            memo[ROOT_RECORD as usize] = root_range;
        }

        // Resolve every directory path. Walk up until a known ancestor is hit, then
        // build the chain downwards, memoising each step.
        let mut chain: Vec<u32> = Vec::with_capacity(32);
        let mut skipped = 0u64;
        for i in 0..raw.len() {
            if !raw[i].is_dir {
                continue;
            }
            let rec = raw[i].rec as usize;
            if rec == ROOT_RECORD as usize || rec >= rec_count {
                continue;
            }
            if memo[rec].0 != SENT {
                continue;
            }
            chain.clear();
            let mut base: Option<(u32, u32)> = None;
            let mut cur = i;
            loop {
                let rc = raw[cur].rec as usize;
                if rc < rec_count && memo[rc].0 != SENT {
                    base = Some(memo[rc]);
                    break;
                }
                if rc == ROOT_RECORD as usize {
                    base = Some(root_range);
                    break;
                }
                if chain.len() >= 256 {
                    break;
                }
                chain.push(cur as u32);
                let p = raw[cur].parent as usize;
                if p == ROOT_RECORD as usize {
                    base = Some(root_range);
                    break;
                }
                if p >= rec_count {
                    break;
                }
                let pe = rec_entry[p];
                if pe == SENT {
                    break;
                }
                cur = pe as usize;
            }
            let Some(base) = base else {
                skipped += 1;
                continue;
            };
            let mut sbuf = String::new();
            sbuf.push_str(slice_str(&paths, base));
            for ci in chain.iter().rev() {
                let e = raw[*ci as usize];
                sbuf.push('\\');
                sbuf.push_str(name_of(&names, &e));
                let off = paths.len() as u32;
                paths.extend_from_slice(sbuf.as_bytes());
                let r = e.rec as usize;
                if r < rec_count {
                    memo[r] = (off, sbuf.len() as u32);
                }
            }
        }

        let mut entries: Vec<Entry> = Vec::with_capacity(raw.len());
        let mut dir_map: HashMap<String, u32> = HashMap::new();
        let mut file_count = 0u64;
        let mut dir_count = 0u64;
        let mut total_bytes = 0u64;
        let mut dropped = 0u64;
        for e in raw.iter().copied() {
            if e.rec == ROOT_RECORD {
                continue;
            }
            let pr = e.parent as usize;
            let prefix = if pr == ROOT_RECORD as usize {
                root_range
            } else if pr < rec_count {
                memo[pr]
            } else {
                (SENT, 0)
            };
            if prefix.0 == SENT {
                dropped += 1;
                continue;
            }
            if e.is_dir {
                let r = e.rec as usize;
                if r < rec_count && memo[r].0 != SENT {
                    dir_map.insert(ascii_lower_str(slice_str(&paths, memo[r])), r as u32);
                }
                dir_count += 1;
            } else {
                file_count += 1;
                total_bytes += e.size;
            }
            entries.push(Entry {
                parent: e.parent,
                name_off: e.name_off,
                prefix_off: prefix.0,
                name_len: e.name_len,
                prefix_len: prefix.1 as u16,
                is_dir: e.is_dir,
                size: e.size,
                mtime: e.mtime,
            });
        }

        Self {
            drive,
            names,
            paths,
            entries,
            rec_parent,
            dir_map,
            scanned_ms,
            file_count,
            dir_count,
            total_bytes,
            skipped: skipped + dropped,
        }
    }

    #[inline]
    pub fn name(&self, e: &Entry) -> &str {
        let s = e.name_off as usize;
        slice_str(&self.names, (s as u32, e.name_len as u32))
    }

    /// Raw UTF-8 bytes of the name - avoids re-validating UTF-8 in the search loop.
    #[inline]
    pub fn name_bytes(&self, e: &Entry) -> &[u8] {
        let start = e.name_off as usize;
        self.names
            .get(start..start + e.name_len as usize)
            .unwrap_or(&[])
    }

    /// Raw UTF-8 bytes of the directory prefix (no trailing separator).
    #[inline]
    pub fn prefix_bytes(&self, e: &Entry) -> &[u8] {
        let start = e.prefix_off as usize;
        self.paths
            .get(start..start + e.prefix_len as usize)
            .unwrap_or(&[])
    }

    #[inline]
    pub fn prefix(&self, e: &Entry) -> &str {
        slice_str(&self.paths, (e.prefix_off, e.prefix_len as u32))
    }

    /// `C:\Windows\System32\kernel32.dll`
    pub fn full_path(&self, e: &Entry) -> String {
        let mut s = String::with_capacity(e.prefix_len as usize + e.name_len as usize + 1);
        s.push_str(self.prefix(e));
        s.push('\\');
        s.push_str(self.name(e));
        s
    }

    /// All of `e`'s parent directories (not including `e` itself).
    pub fn ancestor_records(&self, e: &Entry) -> impl Iterator<Item = u32> + '_ {
        let mut cur = e.parent;
        std::iter::from_fn(move || {
            if cur == ROOT_RECORD || cur == 0 {
                return None;
            }
            let next = self.rec_parent.get(cur as usize).copied().unwrap_or(SENT);
            let out = cur;
            if next == SENT || next == cur {
                cur = 0;
            } else {
                cur = next;
            }
            Some(out)
        })
    }

    /// Is `e` located inside the directory with record number `target`?
    pub fn is_under(&self, e: &Entry, target: u32) -> bool {
        let mut depth = 0;
        for rec in self.ancestor_records(e) {
            if rec == target {
                return true;
            }
            depth += 1;
            if depth > 512 {
                return false;
            }
        }
        false
    }

    /// Resolve a lowercase absolute directory path (`c:\windows`) to a record.
    pub fn resolve_dir(&self, lower_path: &str) -> Option<u32> {
        if lower_path == format!("{}:", self.drive).to_lowercase() {
            return Some(ROOT_RECORD);
        }
        self.dir_map.get(lower_path).copied()
    }
}

#[inline]
fn slice_str(arena: &[u8], range: (u32, u32)) -> &str {
    let start = range.0 as usize;
    let end = start + range.1 as usize;
    match arena.get(start..end) {
        Some(b) => std::str::from_utf8(b).unwrap_or(""),
        None => "",
    }
}

#[inline]
fn name_of<'a>(arena: &'a [u8], e: &RawEntry) -> &'a str {
    slice_str(arena, (e.name_off, e.name_len as u32))
}
