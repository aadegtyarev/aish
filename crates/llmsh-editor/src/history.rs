//! History abstraction, an in-memory implementation and the fuzzy scorer.

use std::borrow::Cow;
use std::collections::HashSet;

/// Source of past command lines, oldest first. Only `len`, `get` and `add`
/// are required; lookups have default implementations (reverse scan with early
/// exit) that an implementation may override with an index.
pub trait History: Send + Sync {
    /// Number of entries.
    fn len(&self) -> usize;
    /// Entry `i` (0 = oldest). Owned or borrowed, so databases can implement it.
    fn get(&self, i: usize) -> Cow<'_, str>;
    /// Record a submitted line.
    fn add(&mut self, line: &str);

    /// True when there are no entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most recent single-line entry that strictly extends `prefix`.
    fn suggest(&self, prefix: &str) -> Option<Cow<'_, str>> {
        self.suggest_before(prefix, self.len()).map(|i| self.get(i))
    }

    /// Index of the most recent single-line entry older than `before` that
    /// strictly extends `prefix` (scans backwards with early exit).
    fn suggest_before(&self, prefix: &str, before: usize) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let p = prefix.as_bytes();
        (0..before.min(self.len())).rev().find(|&i| {
            let e = self.get(i);
            let b = e.as_bytes();
            b.len() > p.len() && b[0] == p[0] && b.starts_with(p) && !e.contains('\n')
        })
    }

    /// Nearest entry before index `before` starting with `prefix` and != `skip`.
    fn prev_matching(&self, before: usize, prefix: &str, skip: &str) -> Option<usize> {
        (0..before.min(self.len()))
            .rev()
            .find(|&i| self.get(i).starts_with(prefix) && self.get(i) != skip)
    }

    /// Nearest entry after index `after` starting with `prefix` and != `skip`.
    fn next_matching(&self, after: usize, prefix: &str, skip: &str) -> Option<usize> {
        (after + 1..self.len()).find(|&i| self.get(i).starts_with(prefix) && self.get(i) != skip)
    }

    /// Best `k` fuzzy matches of `query`, best first (indices).
    fn search(&self, query: &str, k: usize, out: &mut Vec<usize>) {
        out.clear();
        let n = self.len();
        if query.is_empty() {
            out.extend((0..n).rev().take(k));
            return;
        }
        let ci = !query.bytes().any(|b| b.is_ascii_uppercase());
        let mut top: Vec<(i32, usize)> = Vec::with_capacity(k + 1);
        for i in 0..n {
            let e = self.get(i);
            if e.contains('\n') {
                continue;
            }
            if let Some(m) = fuzzy(query.as_bytes(), e.as_bytes(), ci, None) {
                // Recency: up to +24 for the newest entry.
                let s = m + (i * 24 / n.max(1)) as i32;
                if top.len() == k && (s, i) <= *top.last().unwrap_or(&(i32::MIN, 0)) {
                    continue;
                }
                let at = top.partition_point(|&(ts, ti)| (ts, ti) > (s, i));
                top.insert(at, (s, i));
                top.truncate(k);
            }
        }
        out.extend(top.iter().map(|&(_, i)| i));
    }
}

/// Unique entries, most recent occurrence kept.
#[derive(Default)]
pub struct MemHistory {
    entries: Vec<String>,
}

impl MemHistory {
    /// Parse a bash history file: skips `#<timestamp>` lines, blank lines and
    /// lines starting with a space; dedupes keeping the most recent.
    pub fn parse(data: &str) -> MemHistory {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut rev: Vec<String> = Vec::new();
        for line in data.lines().rev() {
            if line.trim().is_empty() || is_timestamp(line) || line.starts_with(' ') {
                continue;
            }
            if seen.insert(line) {
                rev.push(line.to_owned());
            }
        }
        rev.reverse();
        MemHistory { entries: rev }
    }
}

impl History for MemHistory {
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn get(&self, i: usize) -> Cow<'_, str> {
        Cow::Borrowed(&self.entries[i])
    }
    fn add(&mut self, line: &str) {
        if line.trim().is_empty() || line.starts_with(' ') {
            return;
        }
        if let Some(p) = self.entries.iter().rposition(|e| e == line) {
            let e = self.entries.remove(p);
            self.entries.push(e);
        } else {
            self.entries.push(line.to_owned());
        }
    }
    // Faster than the default: direct slice access.
    fn suggest_before(&self, prefix: &str, before: usize) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let p = prefix.as_bytes();
        self.entries[..before.min(self.entries.len())]
            .iter()
            .rposition(|e| {
                let b = e.as_bytes();
                b.len() > p.len() && b[0] == p[0] && b.starts_with(p) && !e.contains('\n')
            })
    }
}

fn is_timestamp(l: &str) -> bool {
    l.strip_prefix('#')
        .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
}

fn eq(a: u8, b: u8, ci: bool) -> bool {
    if ci {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// Subsequence match with bonuses for contiguous runs and word starts.
/// Byte based (smart-case); `positions` receives matched byte offsets.
pub fn fuzzy(q: &[u8], c: &[u8], ci: bool, positions: Option<&mut Vec<usize>>) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let mut qi = 0;
    let mut end = 0;
    for (i, &b) in c.iter().enumerate() {
        if eq(b, q[qi], ci) {
            qi += 1;
            if qi == q.len() {
                end = i;
                break;
            }
        }
    }
    if qi < q.len() {
        return None;
    }
    // Tighten: walk back from the end to find the latest start.
    let mut start = 0;
    let mut qi = q.len();
    for i in (0..=end).rev() {
        if eq(c[i], q[qi - 1], ci) {
            qi -= 1;
            if qi == 0 {
                start = i;
                break;
            }
        }
    }
    let mut score = 0i32;
    let mut prev: Option<usize> = None;
    let mut qi = 0;
    let mut pos = positions;
    if let Some(p) = pos.as_deref_mut() {
        p.clear();
    }
    for (i, &b) in c.iter().enumerate().take(end + 1).skip(start) {
        if qi < q.len() && eq(b, q[qi], ci) {
            score += 16;
            if prev.is_some_and(|p| p + 1 == i) {
                score += 20;
            }
            if i == 0 || !c[i - 1].is_ascii_alphanumeric() {
                score += 14;
            }
            if let Some(p) = pos.as_deref_mut() {
                p.push(i);
            }
            prev = Some(i);
            qi += 1;
        }
    }
    score -= (start as i32).min(12);
    score -= ((end - start + 1 - q.len()) as i32).min(30);
    score -= (c.len() / 16) as i32;
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dedupes_keeps_recent_skips_timestamps() {
        let h = MemHistory::parse("#1700000000\nls\ngit st\n#1700000001\nls\n\n  hidden\ncd /\n");
        let v: Vec<_> = (0..h.len()).map(|i| h.get(i)).collect();
        assert_eq!(v, ["git st", "ls", "cd /"]);
    }

    #[test]
    fn suggest_prefix_most_recent() {
        let mut h = MemHistory::parse("echo hello\necho help\nls\n");
        assert_eq!(h.suggest("ech").as_deref(), Some("echo help"));
        assert_eq!(h.suggest("echo help"), None);
        assert_eq!(h.suggest(""), None);
        h.add("echo hello");
        assert_eq!(h.suggest("echo h").as_deref(), Some("echo hello"));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn prefix_navigation() {
        let h = MemHistory::parse("git a\nls\ngit b\n");
        let i = h.prev_matching(3, "git", "git").unwrap();
        assert_eq!(h.get(i), "git b");
        let j = h.prev_matching(i, "git", "git").unwrap();
        assert_eq!(h.get(j), "git a");
        assert_eq!(h.prev_matching(j, "git", ""), None);
        assert_eq!(h.next_matching(j, "git", ""), Some(i));
    }

    #[test]
    fn fuzzy_ranking() {
        let h =
            MemHistory::parse("cargo build --release\nfoo bar baz\ngit commit -m x\ncat config\n");
        let mut out = vec![];
        h.search("gc", 8, &mut out);
        assert_eq!(h.get(out[0]), "git commit -m x");
        h.search("zzz", 8, &mut out);
        assert!(out.is_empty());
        h.search("", 2, &mut out);
        assert_eq!(h.get(out[0]), "cat config");
        let mut pos = vec![];
        assert!(fuzzy(b"cb", b"cargo build", true, Some(&mut pos)).is_some());
        assert_eq!(pos, vec![0, 6]);
        // Smart case: uppercase query is case sensitive.
        assert!(fuzzy(b"G", b"git", false, None).is_none());
        assert!(fuzzy(b"g", b"Git", true, None).is_some());
        // contiguous beats scattered
        let a = fuzzy(b"abc", b"xabcx", true, None).unwrap();
        let b = fuzzy(b"abc", b"xaxbxc", true, None).unwrap();
        assert!(a > b);
    }

    #[test]
    fn suggest_speed_100k() {
        let mut data = String::new();
        for i in 0..100_000 {
            data.push_str(&format!("command-number {i} --flag value{}\n", i % 97));
        }
        let h = MemHistory::parse(&data);
        let t = std::time::Instant::now();
        let mut hits = 0;
        for q in ["zzz", "command-number 9999", "q", "command-number 1"] {
            hits += h.suggest(q).is_some() as u32;
        }
        let us = t.elapsed().as_micros() / 4;
        eprintln!("suggest avg {us} us over 100k entries ({hits} hits)");
        assert!(us < 5000);
    }
}
