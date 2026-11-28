//! Streaming renderer for model answers: a dim `│ ` bar on every row, light
//! markdown, wrapping, and the command handoff (the last ```bash block is held
//! back, never printed). Output uses `\r\n` before every row (raw terminal)
//! and never ends with one, so the shell's own newline closes the answer.

use lineedit_wip::Theme;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Sty {
    bold: bool,
    code: bool,
}

struct Fence {
    cmd: bool,
    lines: Vec<String>,
}

pub struct Renderer {
    color: bool,
    accent: String,
    dim: String,
    /// Text columns available after the bar.
    cols: usize,
    line: String,
    fence: Option<Fence>,
    /// A closed bash block waiting to learn whether it is the last one.
    held: Option<Vec<String>>,
    /// Rows that arrived after `held` (printed in order if `held` is printed).
    deferred: Vec<u8>,
    pending_blank: bool,
    wrote: bool,
}

fn is_cmd_lang(l: &str) -> bool {
    matches!(
        l.trim().to_ascii_lowercase().as_str(),
        "bash" | "sh" | "shell" | "zsh"
    )
}

/// Control (C0/C1), bidi and zero-width characters: never shown or typed
/// for text that comes from a server.
fn is_bad(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}')
}

/// Strip everything that could act as a terminal escape or hide text.
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|&c| !is_bad(c)).collect()
}

/// One line cut to at most `cols - 1` columns (the last one stays free, so the
/// terminal never wraps it), ending in `…` when something was cut.
pub fn fit_line(s: &str, cols: usize) -> String {
    let room = cols.saturating_sub(1).max(1);
    if s.chars().map(width).sum::<usize>() <= room {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        if used + width(c) + 1 > room {
            break;
        }
        used += width(c);
        out.push(c);
    }
    out.push('…');
    out
}

pub(crate) fn unsafe_cmd(cmd: &str) -> bool {
    cmd.chars().any(|c| is_bad(c) && c != '\n' && c != '\t')
}

/// The command as the user must see it: newlines as `⏎`, leading, trailing
/// and repeated blanks (and blanks next to a newline) as `·` or `·×N`.
pub fn show_cmd(cmd: &str) -> String {
    let cs: Vec<char> = cmd.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\n' || c == '\r' {
            out.push('⏎');
            i += 1;
        } else if c == ' ' || c == '\t' {
            let start = i;
            while i < cs.len() && (cs[i] == ' ' || cs[i] == '\t') {
                i += 1;
            }
            let n = i - start;
            let edge = start == 0
                || i == cs.len()
                || matches!(cs[start - 1], '\n' | '\r')
                || matches!(cs[i], '\n' | '\r');
            if n == 1 && !edge && c == ' ' {
                out.push(' ');
            } else if n <= 3 {
                out.extend(std::iter::repeat_n('·', n));
            } else {
                out.push_str(&format!("·×{n}"));
            }
        } else if is_bad(c) {
            out.push('�');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn wrap_run(display: &str, w: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut cur = String::new();
    let mut cw = 0;
    for c in display.chars() {
        if cw + width(c) > w {
            rows.push(std::mem::take(&mut cur));
            cw = 0;
        }
        cur.push(c);
        cw += width(c);
    }
    rows.push(cur);
    rows
}

/// Rows the approval prompt shows for `cmd` on a terminal `cols` wide.
pub fn run_rows(cmd: &str, cols: u16) -> usize {
    let w = usize::from(cols)
        .saturating_sub(2)
        .max(10)
        .saturating_sub(2)
        .max(1);
    wrap_run(&show_cmd(cmd), w).len()
}

fn width(c: char) -> usize {
    c.width().unwrap_or(0)
}

impl Renderer {
    pub fn new(theme: &Theme, terminal_cols: u16) -> Self {
        Renderer {
            color: theme.enabled,
            accent: theme.accent.clone(),
            dim: theme.dim.clone(),
            cols: usize::from(terminal_cols).saturating_sub(2).max(10),
            line: String::new(),
            fence: None,
            held: None,
            deferred: Vec::new(),
            pending_blank: false,
            wrote: false,
        }
    }

    /// Feed a piece of the answer; rows that became complete are appended to `out`.
    pub fn push(&mut self, text: &str, out: &mut Vec<u8>) {
        for c in text.chars() {
            if c == '\n' {
                let l = std::mem::take(&mut self.line);
                self.line_done(l.trim_end_matches('\r'), out);
            } else {
                self.line.push(c);
            }
        }
    }

    /// The stream ended: flush what is pending; returns the command to put
    /// in the input line, if the answer ended with a complete bash block.
    /// `allow_cmd` is false for cut-off answers. An unterminated fence, and a
    /// command with control/bidi/zero-width characters, are printed instead.
    pub fn finish(&mut self, out: &mut Vec<u8>, allow_cmd: bool) -> Option<String> {
        if !self.line.is_empty() {
            let l = std::mem::take(&mut self.line);
            self.line_done(&l, out);
        }
        if let Some(mut f) = self.fence.take() {
            f.cmd = false;
            self.close_fence(f, out);
        }
        let cmd = self
            .held
            .as_ref()
            .map(|l| join_cmd(l))
            .filter(|c| !c.is_empty());
        match cmd {
            Some(c) if allow_cmd && !unsafe_cmd(&c) => {
                self.held = None;
                out.append(&mut self.deferred);
                Some(c)
            }
            c => {
                self.release_held(out);
                if c.is_some_and(|c| allow_cmd && unsafe_cmd(&c)) {
                    self.note("command with control characters not inserted", out);
                }
                None
            }
        }
    }

    /// A dim one-line message (errors, `⏹`).
    pub fn note(&self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        self.bar(out);
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(sanitize(text).as_bytes());
        self.reset(out);
    }

    /// A dim conversation row: `◇ text`.
    pub fn mark(&self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(format!("◇ {}", sanitize(text)).as_bytes());
        self.reset(out);
    }

    /// A row: `head` as is, `tail` dim.
    pub fn split_row(&self, head: &str, tail: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        self.bar(out);
        out.extend_from_slice(sanitize(head).as_bytes());
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(sanitize(tail).as_bytes());
        self.reset(out);
    }

    /// Like `note` for several lines of already-formed text.
    pub fn plain_row(&self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        self.bar(out);
        out.extend_from_slice(sanitize(text).as_bytes());
    }

    fn sgr(&self, out: &mut Vec<u8>, code: &str) {
        if self.color {
            out.extend_from_slice(format!("\x1b[{code}m").as_bytes());
        }
    }

    fn reset(&self, out: &mut Vec<u8>) {
        if self.color {
            out.extend_from_slice(b"\x1b[0m");
        }
    }

    fn bar(&self, out: &mut Vec<u8>) {
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice("│ ".as_bytes());
        self.reset(out);
    }

    fn line_done(&mut self, line: &str, out: &mut Vec<u8>) {
        if let Some(f) = &mut self.fence {
            if line.trim() == "```" {
                let f = self.fence.take().unwrap();
                self.close_fence(f, out);
            } else {
                f.lines.push(line.to_owned());
            }
            return;
        }
        let t = line.trim_start();
        if let Some(lang) = t.strip_prefix("```") {
            if self.held.is_some() {
                // The held block was not the last one.
                self.release_held(out);
            }
            self.fence = Some(Fence {
                cmd: is_cmd_lang(lang),
                lines: Vec::new(),
            });
            return;
        }
        if t.is_empty() {
            self.pending_blank = true;
            return;
        }
        let rows = self.prose_rows(line);
        let mut buf = Vec::new();
        self.emit_blank(&mut buf);
        for r in rows {
            buf.extend_from_slice(b"\r\n");
            self.bar(&mut buf);
            buf.extend_from_slice(&r);
        }
        self.route(buf, out);
    }

    fn emit_blank(&mut self, buf: &mut Vec<u8>) {
        if std::mem::take(&mut self.pending_blank) && (self.wrote || self.held.is_some()) {
            buf.extend_from_slice(b"\r\n");
            self.bar(buf);
        }
    }

    /// Rows go to the terminal, or behind a held command block.
    fn route(&mut self, buf: Vec<u8>, out: &mut Vec<u8>) {
        if buf.is_empty() {
            return;
        }
        if self.held.is_some() {
            self.deferred.extend(buf);
        } else {
            self.wrote = true;
            out.extend(buf);
        }
    }

    fn release_held(&mut self, out: &mut Vec<u8>) {
        if let Some(h) = self.held.take() {
            let mut buf = Vec::new();
            self.emit_blank(&mut buf);
            self.code_rows(&h, &mut buf);
            self.wrote = true;
            out.extend(buf);
            out.append(&mut self.deferred);
        }
    }

    fn close_fence(&mut self, f: Fence, out: &mut Vec<u8>) {
        if f.cmd {
            self.held = Some(f.lines);
            return;
        }
        let mut buf = Vec::new();
        self.emit_blank(&mut buf);
        self.code_rows(&f.lines, &mut buf);
        self.route(buf, out);
    }

    /// The approval prompt of a command the model wants to run:
    /// `│ ▶ cmd` (the whole command, wrapped, whitespace made visible) and a
    /// dim hint row. The last row is the hint; no trailing newline.
    /// `note`: a dim line above the hint (the judge's reason). The caller has
    /// refused commands too long to be shown whole (`run_rows`).
    pub fn run_prompt(&self, cmd: &str, note: Option<&str>, hints: &[&str], out: &mut Vec<u8>) {
        let rows = wrap_run(&show_cmd(cmd), self.cols.saturating_sub(2).max(1));
        for (i, r) in rows.iter().enumerate() {
            out.extend_from_slice(b"\r\n");
            self.bar(out);
            self.sgr(out, &self.dim.clone());
            out.extend_from_slice(if i == 0 { "▶ " } else { "  " }.as_bytes());
            self.reset(out);
            self.sgr(out, &self.accent.clone());
            out.extend_from_slice(r.as_bytes());
            self.reset(out);
        }
        if let Some(n) = note {
            self.run_note(&format!("⚖ {n}"), out);
        }
        // the longest hint that fits: the bar, two spaces and the text
        let room = self.cols.saturating_sub(2);
        let hint = hints
            .iter()
            .find(|h| h.chars().count() < room)
            .or(hints.last())
            .copied()
            .unwrap_or("");
        out.extend_from_slice(b"\r\n");
        self.bar(out);
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(b"  ");
        out.extend_from_slice(fit_line(&sanitize(hint), room).as_bytes());
        self.reset(out);
    }

    fn run_note(&self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        self.bar(out);
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(b"  ");
        out.extend_from_slice(fit_line(&sanitize(text), self.cols.saturating_sub(2)).as_bytes());
        self.reset(out);
    }

    /// Replace the hint row of `run_prompt` by a dim line (the cursor stays on it).
    pub fn run_prompt_hint(&self, text: &str, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\x1b[2K");
        self.bar(out);
        self.sgr(out, &self.dim.clone());
        out.extend_from_slice(b"  ");
        out.extend_from_slice(fit_line(&sanitize(text), self.cols.saturating_sub(2)).as_bytes());
        self.reset(out);
    }

    /// Take the hint row of `run_prompt` back (the cursor ends on the row
    /// above, so the next output reuses it); `note` replaces it instead.
    pub fn run_prompt_done(&self, note: Option<&str>, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\x1b[2K");
        match note {
            Some(n) => {
                self.bar(out);
                self.sgr(out, &self.dim.clone());
                out.extend_from_slice(b"  ");
                out.extend_from_slice(sanitize(n).as_bytes());
                self.reset(out);
            }
            None => out.extend_from_slice(b"\x1b[A"),
        }
    }

    /// A command shown as accent-colored rows under the bar (the overlay shows
    /// the command before it is inserted).
    pub fn code_block(&self, cmd: &str, out: &mut Vec<u8>) {
        let lines: Vec<String> = cmd.lines().map(str::to_owned).collect();
        self.code_rows(&lines, out);
    }

    fn code_rows(&self, lines: &[String], buf: &mut Vec<u8>) {
        let w = self.cols.saturating_sub(2).max(1);
        for l in lines {
            let l = l.replace('\t', "    ");
            let mut chunks: Vec<String> = vec![String::new()];
            let mut cw = 0;
            for c in l.chars().filter(|&c| !is_bad(c)) {
                if cw + width(c) > w {
                    chunks.push(String::new());
                    cw = 0;
                }
                chunks.last_mut().unwrap().push(c);
                cw += width(c);
            }
            for ch in chunks {
                buf.extend_from_slice(b"\r\n");
                self.bar(buf);
                self.sgr(buf, &self.accent.clone());
                buf.extend_from_slice(b"  ");
                buf.extend_from_slice(ch.as_bytes());
                self.reset(buf);
            }
        }
    }

    /// Styled, wrapped rows (without bar and newline) for a prose line.
    fn prose_rows(&self, line: &str) -> Vec<Vec<u8>> {
        let line = line.replace('\t', "    ");
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        let mut hang = indent;
        let mut lead = " ".repeat(indent);
        let mut text = body;
        let mut header = false;
        let hashes = body.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && body[hashes..].starts_with(' ') {
            header = true;
            text = body[hashes..].trim_start();
        } else if let Some(m) = list_marker(body) {
            lead.push_str(&body[..m]);
            hang = indent + m;
            text = &body[m..];
        }
        let styled = parse_inline(text, header);
        let mut rows: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<(char, Sty)> = Vec::new();
        let mut cur_w = hang.min(self.cols.saturating_sub(4));
        let avail = self.cols;
        let mut first = true;
        let flush = |cur: &mut Vec<(char, Sty)>, first: &mut bool, rows: &mut Vec<Vec<u8>>| {
            let prefix = if *first {
                lead.clone()
            } else {
                " ".repeat(hang)
            };
            *first = false;
            rows.push(self.styled_bytes(&prefix, cur));
            cur.clear();
        };
        let mut i = 0;
        while i < styled.len() {
            // next word with trailing spaces
            let mut j = i;
            while j < styled.len() && styled[j].0 != ' ' {
                j += 1;
            }
            let word_end = j;
            while j < styled.len() && styled[j].0 == ' ' {
                j += 1;
            }
            let ww: usize = styled[i..word_end].iter().map(|x| width(x.0)).sum();
            let sp: usize = j - word_end;
            let cur_first_w = if first { lead.len() } else { hang };
            if !cur.is_empty() && cur_w + ww > avail {
                // trim trailing spaces of the row
                while cur.last().is_some_and(|x| x.0 == ' ') {
                    cur.pop();
                }
                flush(&mut cur, &mut first, &mut rows);
                cur_w = hang;
            }
            if cur.is_empty() && !first {
                cur_w = hang;
            } else if cur.is_empty() {
                cur_w = cur_first_w;
            }
            for &(c, s) in &styled[i..word_end] {
                if cur_w + width(c) > avail && !cur.is_empty() {
                    flush(&mut cur, &mut first, &mut rows);
                    cur_w = hang;
                }
                cur.push((c, s));
                cur_w += width(c);
            }
            for &(c, s) in &styled[word_end..j] {
                cur.push((c, s));
                cur_w += width(c);
            }
            let _ = sp;
            i = j;
        }
        while cur.last().is_some_and(|x| x.0 == ' ') {
            cur.pop();
        }
        if !cur.is_empty() || rows.is_empty() {
            flush(&mut cur, &mut first, &mut rows);
        }
        rows
    }

    fn styled_bytes(&self, prefix: &str, cs: &[(char, Sty)]) -> Vec<u8> {
        let mut b = prefix.as_bytes().to_vec();
        let mut cur = Sty::default();
        let mut tmp = [0u8; 4];
        for &(c, s) in cs {
            if s != cur {
                if self.color {
                    b.extend_from_slice(b"\x1b[0m");
                    if s.bold {
                        b.extend_from_slice(b"\x1b[1m");
                    }
                    if s.code {
                        b.extend_from_slice(format!("\x1b[{}m", self.accent).as_bytes());
                    }
                }
                cur = s;
            }
            if !is_bad(c) {
                b.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
        if cur != Sty::default() {
            self.reset(&mut b);
        }
        b
    }
}

fn join_cmd(lines: &[String]) -> String {
    let mut v: Vec<&str> = lines.iter().map(String::as_str).collect();
    while v.last().is_some_and(|l| l.trim().is_empty()) {
        v.pop();
    }
    while v.first().is_some_and(|l| l.trim().is_empty()) {
        v.remove(0);
    }
    v.join("\n")
}

/// Length of a list marker (`- `, `* `, `+ `, `12. `) at the start, if any.
fn list_marker(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'-' | b'*' | b'+') && b[1] == b' ' {
        return Some(2);
    }
    let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if d > 0 && d < 4 && b.len() > d + 1 && b[d] == b'.' && b[d + 1] == b' ' {
        return Some(d + 2);
    }
    None
}

/// `**bold**` and `` `code` `` (markers removed); unmatched markers stay literal.
fn parse_inline(s: &str, all_bold: bool) -> Vec<(char, Sty)> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::with_capacity(cs.len());
    let mut st = Sty {
        bold: all_bold,
        code: false,
    };
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if !st.code && c == '*' && cs.get(i + 1) == Some(&'*') {
            let closes = st.bold && !all_bold || cs[i + 2..].windows(2).any(|w| w == ['*', '*']);
            if closes {
                if !all_bold {
                    st.bold = !st.bold;
                }
                i += 2;
                continue;
            }
        }
        if c == '`' {
            if st.code {
                st.code = false;
                i += 1;
                continue;
            } else if cs[i + 1..].contains(&'`') {
                st.code = true;
                i += 1;
                continue;
            }
        }
        out.push((c, st));
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn fit_line_cuts_to_the_width_with_an_ellipsis() {
        assert_eq!(super::fit_line("abc", 4), "abc");
        assert_eq!(super::fit_line("abcd", 4), "ab…");
        assert_eq!(super::fit_line("ждёмждём", 6), "ждём…");
        assert_eq!(super::fit_line("日本語日本語", 6), "日本…");
    }

    use super::*;

    fn run(parts: &[&str], color: bool, cols: u16) -> (String, Option<String>) {
        let th = if color {
            Theme::default()
        } else {
            Theme::plain()
        };
        let mut r = Renderer::new(&th, cols);
        let mut out = Vec::new();
        for p in parts {
            r.push(p, &mut out);
        }
        let cmd = r.finish(&mut out, true);
        (String::from_utf8(out).unwrap(), cmd)
    }

    #[test]
    fn plain_rows_have_bar_and_no_trailing_newline() {
        let (o, c) = run(&["hello\nworld"], false, 80);
        assert_eq!(o, "\r\n│ hello\r\n│ world");
        assert_eq!(c, None);
    }

    #[test]
    fn markdown_styles() {
        let (o, _) = run(&["use **bold** and `code` now\n# Title\n"], true, 80);
        assert!(o.contains("\x1b[1mbold\x1b[0m"), "{o:?}");
        assert!(o.contains("\x1b[36mcode\x1b[0m"), "{o:?}");
        assert!(o.contains("\x1b[1mTitle"), "{o:?}");
        assert!(o.contains("\x1b[2m│ \x1b[0m"));
        assert!(!o.contains("**") && !o.contains('`'));
    }

    #[test]
    fn last_bash_block_is_held_and_returned() {
        let (o, c) = run(&["Try this:\n```ba", "sh\nls -la\n", "```\n"], false, 80);
        assert_eq!(o, "\r\n│ Try this:");
        assert_eq!(c.as_deref(), Some("ls -la"));
    }

    #[test]
    fn earlier_bash_block_is_printed_in_order() {
        let (o, c) = run(
            &["a\n```bash\none\n```\nb\n```bash\ntwo\n```\nend\n"],
            false,
            80,
        );
        assert_eq!(c.as_deref(), Some("two"));
        let a = o.find("one").unwrap();
        let b = o.find("│ b").unwrap();
        assert!(a < b && o.contains("end"), "{o:?}");
        assert!(!o.contains("two"));
    }

    #[test]
    fn other_code_block_is_printed_indented() {
        let (o, c) = run(&["```python\nprint(1)\n```\n"], false, 80);
        assert_eq!(o, "\r\n│   print(1)");
        assert_eq!(c, None);
    }

    #[test]
    fn multiline_command() {
        let (_, c) = run(&["```bash\ncd /tmp \\\n  && ls\n```\n"], false, 80);
        assert_eq!(c.as_deref(), Some("cd /tmp \\\n  && ls"));
    }

    #[test]
    fn unterminated_fence_is_never_a_command() {
        let (o, c) = run(&["```bash\nrm -rf /tm"], false, 80);
        assert_eq!(c, None);
        assert!(o.contains("rm -rf /tm"), "{o:?}");
    }

    #[test]
    fn truncated_answer_prints_block_instead_of_command() {
        let mut r = Renderer::new(&Theme::plain(), 80);
        let mut out = Vec::new();
        r.push("```bash\nls\n```\n", &mut out);
        assert_eq!(r.finish(&mut out, false), None);
        assert!(String::from_utf8(out).unwrap().contains("ls"));
    }

    #[test]
    fn hostile_commands_are_not_prefilled() {
        for bad in [
            "echo \u{1b}[2J",
            "ls \u{202E}gpj.exe",
            "ls\u{200B}x",
            "a\rb",
            "a\u{9b}b",
        ] {
            let (o, c) = run(&[&format!("```bash\n{bad}\n```\n")], false, 80);
            assert_eq!(c, None, "{bad:?}");
            assert!(o.contains("not inserted"), "{o:?}");
            assert!(
                !o.contains('\u{1b}') && !o.contains('\u{202E}') && !o.contains('\u{200B}'),
                "{o:?}"
            );
        }
        let (_, c) = run(&["```bash\nls\n\techo a\n```\n"], false, 80);
        assert!(c.is_some());
    }

    #[test]
    fn notes_strip_terminal_escapes() {
        let r = Renderer::new(&Theme::plain(), 80);
        let mut o = Vec::new();
        r.note("bad \u{1b}]0;x\u{7} \u{9b}31m\u{202E}end", &mut o);
        r.plain_row("\u{1b}[2Jhi", &mut o);
        let s = String::from_utf8(o).unwrap();
        assert!(
            !s.contains('\u{1b}') && !s.contains('\u{7}') && !s.contains('\u{9b}'),
            "{s:?}"
        );
        assert!(s.contains("end") && s.contains("hi"));
    }

    #[test]
    fn only_command_prints_nothing() {
        let (o, c) = run(&["```bash\ndu -sh .\n```"], false, 80);
        assert_eq!(o, "");
        assert_eq!(c.as_deref(), Some("du -sh ."));
    }

    #[test]
    fn wraps_to_width_minus_two_and_keeps_lists() {
        let (o, _) = run(&["- one two three four five six seven eight\n"], false, 22);
        let rows: Vec<&str> = o.split("\r\n").filter(|r| !r.is_empty()).collect();
        assert!(rows.len() >= 2, "{o:?}");
        for r in &rows {
            assert!(r.chars().count() <= 22, "{r:?}");
        }
        assert!(rows[0].starts_with("│ - one"));
        assert!(rows[1].starts_with("│   "), "{rows:?}");
    }

    #[test]
    fn blank_lines_collapse_and_trailing_dropped() {
        let (o, _) = run(&["\n\na\n\n\nb\n\n"], false, 80);
        assert_eq!(o, "\r\n│ a\r\n│ \r\n│ b");
    }

    #[test]
    fn wide_chars_wrap_by_width() {
        let (o, _) = run(&["日本語日本語日本語日本語日本語\n"], false, 14);
        for r in o.split("\r\n").filter(|r| !r.is_empty()) {
            let w: usize = r.chars().map(width).sum();
            assert!(w <= 14, "{r:?} {w}");
        }
    }
}
