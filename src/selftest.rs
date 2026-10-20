//! `fastfind --selftest`: index the first NTFS volume and print a report.
//!
//! Useful for verifying MFT parsing correctness and measuring the index speed
//! without touching the GUI.

use std::fmt::Write as _;
use std::time::Instant;

use crate::index::VolumeIndex;
use crate::search::{self, DirScope, Query};
use crate::volume::Volume;

struct Report {
    text: String,
}

impl Report {
    fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    fn line(&mut self, args: std::fmt::Arguments<'_>) {
        let mut s = String::new();
        let _ = s.write_fmt(args);
        println!("{}", s);
        let _ = writeln!(self.text, "{}", s);
    }

    fn save(&self) {
        let path = std::env::temp_dir().join("fastfind-selftest.log");
        let _ = std::fs::write(&path, &self.text);
    }
}

macro_rules! say {
    ($r:expr, $($arg:tt)*) => { $r.line(format_args!($($arg)*)) };
}

pub fn run() -> i32 {    let mut r = Report::new();
    say!(r, "== FastFind selftest ==");
    say!(
        r,
        "elevated: {}",
        crate::elevate::is_elevated()
    );

    let drives = crate::volume::fixed_drives();
    for d in &drives {
        say!(r, "drive {}  fs={}  label={}", d.letter, d.fs, d.label);
    }
    let Some(cand) = drives.iter().find(|d| d.fs.eq_ignore_ascii_case("NTFS")) else {
        say!(r, "no NTFS fixed drive found");
        r.save();
        return 1;
    };

    let vol = match Volume::open(cand.letter) {
        Ok(v) => v,
        Err(e) => {
            say!(r, "open {}: failed: {:#}", cand.letter, e);
            r.save();
            return 1;
        }
    };
    let info = vol.info;
    say!(
        r,
        "geometry: cluster={} record={} mft_valid={} records={}",
        info.bytes_per_cluster,
        info.bytes_per_record,
        info.mft_valid_bytes,
        info.total_records
    );

    let t = Instant::now();
    let scan = match crate::mft::scan(&vol, &|_done| {}) {
        Ok(s) => s,
        Err(e) => {
            say!(r, "scan failed: {:#}", e);
            r.save();
            return 1;
        }
    };
    let scan_ms = t.elapsed().as_millis();
    say!(
        r,
        "scan: entries={} name_bytes={} in {} ms",
        scan.entries.len(),
        scan.names.len(),
        scan_ms
    );
    let st = scan.stats;
    say!(
        r,
        "records: total={} unused={} extension={} no_name={} fixup_fail={} not_file={}",
        st.records,
        st.unused,
        st.extension,
        st.no_name,
        st.fixup_fail,
        st.not_file_magic
    );
    say!(
        r,
        "extension names: seen={} attached={} orphan={}",
        st.ext_names_seen,
        st.ext_names_attached,
        st.ext_names_orphan
    );
    say!(r, "names seen in base records: {}", st.names_seen);
    say!(r, "names dropped beyond the 4-link cap: {}", st.names_dropped);
    let mut recs: Vec<u32> = scan.entries.iter().map(|e| e.rec).collect();
    recs.sort_unstable();
    recs.dedup();
    say!(r, "scan: distinct records with names = {}", recs.len());
    drop(vol);

    let t = Instant::now();
    let idx = VolumeIndex::build(cand.letter, scan, scan_ms as u64);
    let build_ms = t.elapsed().as_millis();
    say!(
        r,
        "build: files={} dirs={} bytes={} skipped={} in {} ms",
        idx.file_count,
        idx.dir_count,
        idx.total_bytes,
        idx.skipped,
        build_ms
    );
    let entry_size = std::mem::size_of::<crate::index::Entry>();
    say!(
        r,
        "memory: entries {:.1} MB ({} B/entry) + names {:.1} MB + paths {:.1} MB + dir_map ~{:.1} MB ({} dirs)",
        (idx.entries.len() * entry_size) as f64 / 1048576.0,
        entry_size,
        idx.names.len() as f64 / 1048576.0,
        idx.paths.len() as f64 / 1048576.0,
        (idx.dir_map.len() * 72) as f64 / 1048576.0,
        idx.dir_map.len()
    );

    // Largest entries: handy for spotting files whose $DATA size failed to parse.
    let mut biggest: Vec<usize> = (0..idx.entries.len()).collect();
    biggest.sort_unstable_by_key(|&i| std::cmp::Reverse(idx.entries[i].size));
    let mut zero_bytes = 0u64;
    for e in &idx.entries {
        if !e.is_dir && e.size == 0 {
            zero_bytes += 1;
        }
    }
    say!(r, "zero-byte files: {}", zero_bytes);
    for &i in biggest.iter().take(12) {
        let e = &idx.entries[i];
        say!(r, "  biggest {:>14}  {}", e.size, idx.full_path(e));
    }

    // Spot checks: known Windows files must resolve to the right full path.
    for target in [
        "kernel32.dll",
        "explorer.exe",
        "ntoskrnl.exe",
        "System32",
        "notepad.exe",
    ] {
        let mut hits = Vec::new();
        for e in &idx.entries {
            if idx.name(e).eq_ignore_ascii_case(target) {
                hits.push(idx.full_path(e));
                if hits.len() == 3 {
                    break;
                }
            }
        }
        say!(r, "lookup {:>14}: {} hit(s)  {:?}", target, hits.len(), hits);
    }

    for probe in ["System32"] {
        let n = idx.entries.iter().filter(|e| idx.name(e) == probe).count();
        say!(r, "probe {:?}: {} exact matches", probe, n);
    }

    // How many entries match without any result cap (drives the UI result limit).
    for pat in ["*.dll", "*.exe", "*.log"] {
        let q = Query::parse(pat, "");
        let scope = DirScope::All;
        let mut out = Vec::new();
        let t = Instant::now();
        search::search_volume(&idx, &q, &scope, &mut out, 10_000_000);
        say!(
            r,
            "total {:<8} matches={} in {:.1} ms",
            pat,
            out.len(),
            t.elapsed().as_secs_f64() * 1000.0
        );
    }

    for dir in [
        format!("{}:\\Windows\\System32", cand.letter).to_lowercase(),
        format!("{}:\\Users", cand.letter).to_lowercase(),
        format!("{}:\\Windows", cand.letter).to_lowercase(),
    ] {
        say!(
            r,
            "resolve_dir {:>28} -> {:?}",
            dir,
            idx.resolve_dir(&dir)
        );
    }

    for (pat, dir) in [
        ("*.exe", ""),
        ("*.dll", ""),
        ("*.log", ""),
        ("*kernel32*", ""),
        ("*.dll", &format!("{}:\\Windows\\System32", cand.letter)),
        ("*.dll", &format!("{}:\\Windows\\System32\\", cand.letter)),
        ("*.exe", "Windows\\System32"),
        ("*.dll", &format!("{}:\\", cand.letter)),
        ("*.dll", &format!("{}:", cand.letter)),
        ("a?c*d*.txt", ""),
        ("*.dll", r"Z:\no\such\dir"),
    ] {
        let q = Query::parse(pat, dir);
        let scope = q.scope_for(&idx);
        let err = q.validate(std::iter::once(&idx));
        let mut out = Vec::new();
        let t = Instant::now();
        let truncated = search::search_volume(&idx, &q, &scope, &mut out, search::RESULT_LIMIT);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        say!(
            r,
            "query {:<24} dir={:<28} hits={} truncated={} in {:.1} ms  err={:?}",
            pat,
            if dir.is_empty() { "<all>" } else { dir },
            out.len(),
            truncated,
            ms,
            err
        );
        if let Some(&first) = out.first() {
            if let Some(e) = idx.entries.get(first as usize) {
                say!(r, "    first: {}", idx.full_path(e));
            }
        }
    }

    // Verify ancestry filtering against a plain string prefix check.
    let dir = format!("{}:\\Windows\\System32", cand.letter);
    let q = Query::parse("*.dll", &dir);
    let scope = q.scope_for(&idx);
    let mut out = Vec::new();
    search::search_volume(&idx, &q, &scope, &mut out, search::RESULT_LIMIT);
    let wrong = out
        .iter()
        .filter(|&&i| {
            idx.entries
                .get(i as usize)
                .map(|e| !idx.full_path(e).to_lowercase().starts_with(&dir.to_lowercase()))
                .unwrap_or(true)
        })
        .count();
    say!(r, "scope check: {} hits, {} outside the requested directory", out.len(), wrong);

    r.save();
    say!(r, "report written to {}", std::env::temp_dir().join("fastfind-selftest.log").display());
    0
}
