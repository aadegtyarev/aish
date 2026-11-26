//! In-memory history (from $HISTFILE), prefix lookup and fuzzy search.

use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Default)]
pub struct History {
    /// Oldest first, unique, most recent occurrence kept.
    entries: Vec<String>,
}

pub fn histfile() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HISTFILE").filter(|v| !v.is_empty()) {
        return Some(p.into());
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".bash_history"))
}

impl History {
    pub fn load() -> History {
        let data = histfile()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();
        Self::parse(&String::from_utf8_lossy(&data))
    }

    pub fn parse(data: &str) -> History {
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
        History { entries: rev }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn get(&self, i: usize) -> &str {
        &self.entries[i]
    }

    /// Record a submitted line (moves an older duplicate to the end).
    pub fn push(&mut self, line: &str) {
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

    /// Most recent single-line entry that strictly extends `prefix`.
    pub fn suggest(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        let p = prefix.as_bytes();
        let first = p[0];
        self.entries
            .iter()
            .rev()
            .find(|e| {
                let b = e.as_bytes();
                b.len() > p.len() && b[0] == first && b.starts_with(p) && !e.contains('\n')
            })
            .map(String::as_str)
    }

    /// Index of the nearest entry before `before` that starts with `prefix`
    /// and differs from `skip`.
    pub fn prev_matching(&self, before: usize, prefix: &str, skip: &str) -> Option<usize> {
        (0..before.min(self.entries.len()))
            .rev()
            .find(|&i| self.entries[i].starts_with(prefix) && self.entries[i] != skip)
    }

    pub fn next_matching(&self, after: usize, prefix: &str, skip: &str) -> Option<usize> {
        (after + 1..self.entries.len())
            .find(|&i| self.entries[i].starts_with(prefix) && self.entries[i] != skip)
    }

    /// Best `k` fuzzy matches of `query`, best first (indices into entries).
    pub fn search(&self, query: &str, k: usize, out: &mut Vec<usize>) {
        out.clear();
        let n = self.entries.len();
        if query.is_empty() {
            out.extend((0..n).rev().take(k));
            return;
        }
        let ci = !query.bytes().any(|b| b.is_ascii_uppercase());
        let mut top: Vec<(i32, usize)> = Vec::with_capacity(k + 1);
        for (i, e) in self.entries.iter().enumerate() {
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
        let h = History::parse("#1700000000\nls\ngit st\n#1700000001\nls\n\n  hidden\ncd /\n");
        let v: Vec<_> = (0..h.len()).map(|i| h.get(i)).collect();
        assert_eq!(v, ["git st", "ls", "cd /"]);
    }

    #[test]
    fn suggest_prefix_most_recent() {
        let mut h = History::parse("echo hello\necho help\nls\n");
        assert_eq!(h.suggest("ech"), Some("echo help"));
        assert_eq!(h.suggest("echo help"), None);
        assert_eq!(h.suggest(""), None);
        h.push("echo hello");
        assert_eq!(h.suggest("echo h"), Some("echo hello"));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn prefix_navigation() {
        let h = History::parse("git a\nls\ngit b\n");
        let i = h.prev_matching(3, "git", "git").unwrap();
        assert_eq!(h.get(i), "git b");
        let j = h.prev_matching(i, "git", "git").unwrap();
        assert_eq!(h.get(j), "git a");
        assert_eq!(h.prev_matching(j, "git", ""), None);
        assert_eq!(h.next_matching(j, "git", ""), Some(i));
    }

    #[test]
    fn fuzzy_ranking() {
        let h = History::parse("cargo build --release\nfoo bar baz\ngit commit -m x\ncat config\n");
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
        let h = History::parse(&data);
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
