//! Wildcard matching for file names: `*` (any run of characters) and `?` (one character).
//!
//! Case-insensitive like Windows itself (ASCII folding - CJK has no case).
//!
//! Common shapes are compiled into specialised matchers because they dominate
//! real usage and turn a 1M entry scan into a handful of tight loops:
//!   `*.log`   -> suffix compare
//!   `report*` -> prefix compare
//!   `*tmp*`   -> substring search
//! Everything else falls back to a backtracking matcher: a byte level one when
//! the pattern has no `?`, and a char level one so `?` means one character
//! (important for CJK names).

use crate::winutil::ascii_lower;

const MAX_NAME_CHARS: usize = 400;

enum Kind {
    All,
    Exact,
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Vec<u8>),
    General,
}

pub struct Wildcard {
    /// Lowercased pattern bytes.
    pat: Vec<u8>,
    /// Lowercased pattern chars (only when `?` is present).
    pat_chars: Vec<char>,
    /// Literal prefix before the first wildcard - used to reject quickly.
    fast_prefix: Vec<u8>,
    has_qmark: bool,
    kind: Kind,
}

impl Wildcard {
    pub fn new(pattern: &str) -> Self {
        let lowered: Vec<u8> = pattern.bytes().map(ascii_lower).collect();
        let has_qmark = pattern.contains('?');
        let stars = pattern.bytes().filter(|&b| b == b'*').count();
        let fast_prefix: Vec<u8> = lowered
            .iter()
            .copied()
            .take_while(|&b| b != b'*' && b != b'?')
            .collect();
        let pat_chars = if has_qmark {
            pattern
                .chars()
                .map(|c| c.to_ascii_lowercase())
                .collect::<Vec<char>>()
        } else {
            Vec::new()
        };

        let kind = if !has_qmark {
            if stars == 0 {
                Kind::Exact
            } else if stars == 1 {
                let starts = pattern.starts_with('*');
                let ends = pattern.ends_with('*');
                match (starts, ends) {
                    (true, true) if pattern.len() == 1 => Kind::All,
                    (true, true) => Kind::Contains(lowered[1..lowered.len() - 1].to_vec()),
                    (true, false) => Kind::Suffix(lowered[1..].to_vec()),
                    (false, true) => Kind::Prefix(lowered[..lowered.len() - 1].to_vec()),
                    (false, false) => Kind::General,
                }
            } else {
                Kind::General
            }
        } else {
            Kind::General
        };

        Self {
            pat: lowered,
            pat_chars,
            fast_prefix,
            has_qmark,
            kind,
        }
    }

    /// Does `name` (UTF-8 bytes) match?
    pub fn matches(&self, name: &[u8]) -> bool {
        match &self.kind {
            Kind::All => true,
            Kind::Exact => name.len() == self.pat.len() && fold_eq(&self.pat, name),
            Kind::Prefix(lit) => name.len() >= lit.len() && fold_eq(lit, &name[..lit.len()]),
            Kind::Suffix(lit) => {
                name.len() >= lit.len() && fold_eq(lit, &name[name.len() - lit.len()..])
            }
            Kind::Contains(lit) => contains_fold(name, lit),
            Kind::General => {
                if !starts_with_fold(name, &self.fast_prefix) {
                    return false;
                }
                if self.has_qmark {
                    self.match_chars(name)
                } else {
                    match_bytes(&self.pat, name)
                }
            }
        }
    }

    fn match_chars(&self, name: &[u8]) -> bool {
        match std::str::from_utf8(name) {
            Ok(s) => {
                let mut buf = ['\0'; MAX_NAME_CHARS];
                let mut n = 0;
                let mut overflow = false;
                for c in s.chars() {
                    if n == MAX_NAME_CHARS {
                        overflow = true;
                        break;
                    }
                    buf[n] = c.to_ascii_lowercase();
                    n += 1;
                }
                if overflow {
                    let chars: Vec<char> = s.chars().map(|c| c.to_ascii_lowercase()).collect();
                    return match_chars(&self.pat_chars, &chars);
                }
                match_chars(&self.pat_chars, &buf[..n])
            }
            Err(_) => match_bytes(&self.pat, name),
        }
    }
}

#[inline]
fn fold_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(&x, &y)| ascii_lower(x) == ascii_lower(y))
}

#[inline]
fn starts_with_fold(name: &[u8], prefix: &[u8]) -> bool {
    name.len() >= prefix.len() && fold_eq(prefix, &name[..prefix.len()])
}

/// Case insensitive substring search. Uses memchr to skip ahead, so it runs at
/// memory bandwidth for typical file names.
fn contains_fold(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    let needle_len = needle.len();
    let first = needle[0];
    let mut start = 0usize;
    while start + needle_len <= hay.len() {
        let Some(off) = memchr::memchr2(first, first.to_ascii_uppercase(), &hay[start..]) else {
            return false;
        };
        let pos = start + off;
        if pos + needle_len > hay.len() {
            return false;
        }
        if fold_eq(needle, &hay[pos..pos + needle_len]) {
            return true;
        }
        start = pos + 1;
    }
    false
}

/// Byte level matcher (`*` and literals only).
fn match_bytes(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star_p: Option<usize> = None;
    let mut star_n = 0usize;
    while n < name.len() {
        if p < pat.len() && pat[p] != b'*' && pat[p] == ascii_lower(name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = Some(p);
            star_n = n;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_n += 1;
            n = star_n;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Char level matcher (`*`, `?` and literals).
fn match_chars(pat: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star_p: Option<usize> = None;
    let mut star_n = 0usize;
    while n < name.len() {
        if p < pat.len() && pat[p] != '*' && (pat[p] == '?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star_p = Some(p);
            star_n = n;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_n += 1;
            n = star_n;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        let w = Wildcard::new("Report.docx");
        assert!(w.matches(b"report.DOCX"));
        assert!(!w.matches(b"report.docx2"));
    }

    #[test]
    fn suffixes() {
        let w = Wildcard::new("*.log");
        assert!(w.matches(b"a.log"));
        assert!(w.matches(b".log"));
        assert!(!w.matches(b"a.logx"));
        assert!(!w.matches(b"log"));
    }

    #[test]
    fn prefixes() {
        let w = Wildcard::new("report*");
        assert!(w.matches(b"Report2024.docx"));
        assert!(!w.matches(b"my-report.docx"));
    }

    #[test]
    fn contains() {
        let w = Wildcard::new("*tmp*");
        assert!(w.matches(b"myTMPfile"));
        assert!(!w.matches(b"nothere"));
        assert!(w.matches(b"tmp"));
        let w = Wildcard::new("*");
        assert!(w.matches(b""));
        assert!(w.matches(b"anything"));
    }

    #[test]
    fn stars() {
        let w = Wildcard::new("a*b*c");
        assert!(w.matches(b"axxbyyc"));
        assert!(!w.matches(b"axxbyy"));
        let w = Wildcard::new("*.tar.*");
        assert!(w.matches(b"x.tar.gz"));
        assert!(!w.matches(b"x.tar"));
    }

    #[test]
    fn questions() {
        let w = Wildcard::new("a?c");
        assert!(w.matches(b"abc"));
        assert!(!w.matches(b"ac"));
        // `?` matches exactly one multi-byte character
        let w = Wildcard::new("?国.txt");
        assert!(w.matches("中国.txt".as_bytes()));
        assert!(!w.matches("国.txt".as_bytes()));
    }

    #[test]
    fn chinese() {
        let w = Wildcard::new("*报告*.docx");
        assert!(w.matches("年度报告2024.docx".as_bytes()));
        assert!(!w.matches("年度报纸2024.docx".as_bytes()));
    }
}
