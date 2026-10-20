//! Query compilation and the background search worker.
//!
//! Searching is a linear scan over the in-memory entry array: a name match first
//! (cheap, rejects almost everything), then the directory scope check only for
//! the survivors.

use std::cmp::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};

use crate::index::VolumeIndex;
use crate::winutil::{ascii_lower, ascii_lower_str};
use crate::wildcard::Wildcard;

/// How many hits the UI is willing to keep. Everything beyond this is reported
/// as "truncated" - it keeps sorting and rendering bounded.
pub const RESULT_LIMIT: usize = 100_000;

#[derive(Clone, Debug)]
pub enum DirScope {
    All,
    None,
    Under(u32),
    Contains(Vec<u8>),
}

pub struct Query {
    pub wc: Wildcard,
    pub dir: Option<(Option<char>, String)>,
}

/// `"D:\Games\"` -> `(Some('d'), "d:\games")`, `"windows"` -> `(None, "windows")`.
fn normalize_dir(dir: &str) -> Option<(Option<char>, String)> {
    let t = dir.trim();
    if t.is_empty() {
        return None;
    }
    let mut s: String = t.replace('/', "\\");
    while s.ends_with('\\') {
        s.pop();
    }
    // A bare separator (or several) means "everywhere".
    if s.is_empty() {
        return None;
    }
    // "C:\" and "C:" both mean the whole drive.
    if s.len() == 2 && s.as_bytes()[1] == b':' {
        let s = s.to_ascii_lowercase();
        return Some((s.chars().next(), s));
    }
    let bytes = s.as_bytes();
    let drive = if bytes.len() >= 2 && bytes[1] == b':' {
        Some(bytes[0].to_ascii_lowercase() as char)
    } else {
        None
    };
    Some((drive, ascii_lower_str(&s)))
}

impl Query {
    pub fn parse(pattern: &str, dir: &str) -> Self {
        Self {
            wc: Wildcard::new(pattern),
            dir: normalize_dir(dir),
        }
    }

    /// Human readable description of the directory filter, for the status bar.
    pub fn dir_label(&self) -> String {
        match &self.dir {
            None => crate::i18n::all_drives().to_owned(),
            Some((_, p)) => p.clone(),
        }
    }

    /// Returns an error message when the requested directory cannot be resolved.
    pub fn validate<'a>(&self, volumes: impl Iterator<Item = &'a VolumeIndex>) -> Option<String> {
        let (drive, path) = self.dir.clone()?;
        let drive = drive?;
        let vols: Vec<&VolumeIndex> = volumes.collect();
        let Some(vol) = vols.iter().find(|v| v.drive.eq_ignore_ascii_case(&drive)) else {
            return Some(crate::i18n::drive_not_indexed(drive.to_ascii_uppercase()));
        };
        if path.len() <= 2 {
            return None;
        }
        if vol.resolve_dir(&path).is_none() {
            return Some(crate::i18n::dir_not_in_index(&path));
        }
        None
    }

    pub fn scope_for(&self, vol: &VolumeIndex) -> DirScope {
        match &self.dir {
            None => DirScope::All,
            Some((drive, path)) => match drive {
                Some(d) => {
                    if !d.eq_ignore_ascii_case(&vol.drive) {
                        DirScope::None
                    } else if path.len() <= 2 {
                        DirScope::All
                    } else {
                        match vol.resolve_dir(path) {
                            Some(rec) => DirScope::Under(rec),
                            None => DirScope::None,
                        }
                    }
                }
                None => DirScope::Contains(path.as_bytes().to_vec()),
            },
        }
    }
}

/// Scan one volume. Returns true when `limit` was reached.
pub fn search_volume(
    vol: &VolumeIndex,
    query: &Query,
    scope: &DirScope,
    out: &mut Vec<u32>,
    limit: usize,
) -> bool {
    if matches!(scope, DirScope::None) {
        return false;
    }
    let mut scratch: Vec<u8> = Vec::new();
    for (i, e) in vol.entries.iter().enumerate() {
        let name = vol.name_bytes(e);
        if !query.wc.matches(name) {
            continue;
        }
        match scope {
            DirScope::All => {}
            DirScope::None => continue,
            DirScope::Under(target) => {
                if !vol.is_under(e, *target) {
                    continue;
                }
            }
            DirScope::Contains(needle) => {
                scratch.clear();
                scratch.extend(vol.prefix_bytes(e).iter().map(|&b| ascii_lower(b)));
                scratch.push(b'\\');
                scratch.extend(name.iter().map(|&b| ascii_lower(b)));
                if memchr::memmem::find(&scratch, needle).is_none() {
                    continue;
                }
            }
        }
        out.push(i as u32);
        if out.len() >= limit {
            return true;
        }
    }
    false
}

pub struct Request {
    pub gen: u64,
    pub pattern: String,
    pub dir: String,
}

pub struct Response {
    pub gen: u64,
    pub hits: Arc<Vec<(char, u32)>>,
    pub truncated: bool,
    pub ms: u128,
    pub error: Option<String>,
    pub dir_label: String,
}

/// Compares two names case insensitively (ASCII) without allocating.
pub fn cmp_name(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let n = ab.len().min(bb.len());
    for i in 0..n {
        let c = ascii_lower(ab[i]).cmp(&ascii_lower(bb[i]));
        if c != Ordering::Equal {
            return c;
        }
    }
    ab.len().cmp(&bb.len())
}

fn sort_hits(vols: &[Arc<VolumeIndex>], hits: &mut [(char, u32)]) {
    fn entry_of<'a>(
        vols: &'a [Arc<VolumeIndex>],
        hit: (char, u32),
    ) -> Option<(&'a VolumeIndex, &'a crate::index::Entry)> {
        vols.iter()
            .find(|v| v.drive == hit.0)
            .and_then(|v| v.entries.get(hit.1 as usize).map(|e| (v.as_ref(), e)))
    }
    // Sort on borrowed names/prefixes: no allocation per hit, which keeps a
    // 70k-row result set in the low milliseconds.
    hits.sort_unstable_by(|a, b| match (entry_of(vols, *a), entry_of(vols, *b)) {
        (Some((va, ea)), Some((vb, eb))) => cmp_name(va.name(ea), vb.name(eb))
            .then_with(|| cmp_name(va.prefix(ea), vb.prefix(eb)))
            .then_with(|| va.drive.cmp(&vb.drive)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
}

pub type VolumeSet = Arc<RwLock<Vec<Arc<VolumeIndex>>>>;

/// Spawns the search worker thread; returns the request sender and the response
/// receiver.
pub fn spawn_worker(volumes: VolumeSet) -> (Sender<Request>, Receiver<Response>) {
    let (req_tx, req_rx) = channel::<Request>();
    let (res_tx, res_rx) = channel::<Response>();
    std::thread::Builder::new()
        .name("fastfind-search".to_owned())
        .spawn(move || {
            while let Ok(first) = req_rx.recv() {
                // Coalesce: if more requests are already queued, jump to the newest.
                let mut req = first;
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let started = std::time::Instant::now();
                let query = Query::parse(&req.pattern, &req.dir);
                let vols: Vec<Arc<VolumeIndex>> =
                    volumes.read().map(|g| g.clone()).unwrap_or_default();

                if req.pattern.trim().is_empty() {
                    let _ = res_tx.send(Response {
                        gen: req.gen,
                        hits: Arc::new(Vec::new()),
                        truncated: false,
                        ms: 0,
                        error: None,
                        dir_label: query.dir_label(),
                    });
                    continue;
                }

                let error = query.validate(vols.iter().map(|v| v.as_ref()));
                let mut hits: Vec<(char, u32)> = Vec::new();
                let mut truncated = false;
                if error.is_none() {
                    for vol in &vols {
                        let scope = query.scope_for(vol);
                        let mut local = Vec::new();
                        let full = search_volume(vol, &query, &scope, &mut local, RESULT_LIMIT);
                        truncated |= full;
                        hits.reserve(local.len());
                        hits.extend(local.into_iter().map(|i| (vol.drive, i)));
                        if hits.len() >= RESULT_LIMIT {
                            truncated = true;
                            hits.truncate(RESULT_LIMIT);
                            break;
                        }
                    }
                    sort_hits(&vols, &mut hits);
                }
                let _ = res_tx.send(Response {
                    gen: req.gen,
                    hits: Arc::new(hits),
                    truncated,
                    ms: started.elapsed().as_millis(),
                    error,
                    dir_label: query.dir_label(),
                });
            }
        })
        .expect("spawn search worker");
    (req_tx, res_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert!(normalize_dir("  ").is_none());
        assert!(normalize_dir("\\").is_none());
        assert_eq!(
            normalize_dir("D:\\Games\\"),
            Some((Some('d'), "d:\\games".to_owned()))
        );
        assert_eq!(
            normalize_dir("D:/Games//"),
            Some((Some('d'), "d:\\games".to_owned()))
        );
        assert_eq!(normalize_dir("d:"), Some((Some('d'), "d:".to_owned())));
        assert_eq!(normalize_dir("D:\\"), Some((Some('d'), "d:".to_owned())));
        assert_eq!(
            normalize_dir("Windows/System32"),
            Some((None, "windows\\system32".to_owned()))
        );
    }
}
