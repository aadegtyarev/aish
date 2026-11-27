//! A tiny terminal emulator for narrow-screen tests: feeds the bytes aish
//! writes to a character grid (xterm-style deferred wrap, reflow on resize),
//! answers cursor-position queries and lets tests assert on what a user sees.
#![allow(dead_code)]

use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::Session;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};

/// Blank cell left at the end of a row when a wide glyph moved to the next.
const PAD: char = '\u{1}';

#[derive(Clone)]
struct Row {
    cells: Vec<(char, bool)>, // (glyph, dim); '\0' = right half of a wide glyph
    wrapped: bool,            // continues on the next row (autowrap, not a newline)
}

impl Row {
    fn new(cols: usize) -> Row {
        Row {
            cells: vec![(' ', false); cols],
            wrapped: false,
        }
    }
    fn text(&self) -> String {
        let s: String = self
            .cells
            .iter()
            .filter(|c| c.0 != '\0' && c.0 != PAD)
            .map(|c| c.0)
            .collect();
        s.trim_end().to_string()
    }
}

pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    grid: Vec<Row>,
    pub cx: usize,
    pub cy: usize,
    pending: bool,
    dim: bool,
    saved: (usize, usize),
    alt_saved: Option<(Vec<Row>, usize, usize)>,
    scroll: Vec<Row>,
    pub replies: Vec<u8>,
    pub unknown: Vec<String>,
    pub raw: Vec<u8>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        Screen {
            cols,
            rows,
            grid: vec![Row::new(cols); rows],
            cx: 0,
            cy: 0,
            pending: false,
            dim: false,
            saved: (0, 0),
            alt_saved: None,
            scroll: vec![],
            replies: vec![],
            unknown: vec![],
            raw: vec![],
        }
    }

    fn linefeed(&mut self) {
        self.pending = false;
        if self.cy + 1 == self.rows {
            let top = self.grid.remove(0);
            if self.alt_saved.is_none() {
                self.scroll.push(top);
            }
            self.grid.push(Row::new(self.cols));
        } else {
            self.cy += 1;
        }
    }

    fn put(&mut self, c: char) {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            return;
        }
        if self.pending || self.cx + w > self.cols {
            if !self.pending && self.cx < self.cols {
                self.grid[self.cy].cells[self.cx] = (PAD, false);
            }
            self.grid[self.cy].wrapped = true;
            self.cx = 0;
            self.linefeed();
        }
        let dim = self.dim;
        let row = &mut self.grid[self.cy];
        row.cells[self.cx] = (c, dim);
        if w == 2 {
            row.cells[self.cx + 1] = ('\0', dim);
        }
        self.cx += w;
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
            self.pending = true;
        }
    }

    fn erase_cells(&mut self, y: usize, from: usize, to: usize) {
        for x in from..to.min(self.cols) {
            self.grid[y].cells[x] = (' ', false);
        }
    }

    /// Terminal resize with reflow of wrapped lines (as phone terminals do).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        // logical lines (as glyphs) and the cursor as a glyph index inside them
        let mut lines: Vec<Vec<(char, bool)>> = vec![];
        let mut cur = (0usize, 0usize);
        let mut acc: Vec<(char, bool)> = vec![];
        for (y, r) in self.grid.iter().enumerate() {
            let glyph = |c: &&(char, bool)| c.0 != '\0' && c.0 != PAD;
            if y == self.cy {
                let upto = self.cx + usize::from(self.pending);
                let before = r.cells[..upto.min(r.cells.len())]
                    .iter()
                    .filter(glyph)
                    .count();
                cur = (lines.len(), acc.len() + before);
            }
            let mut cells: Vec<(char, bool)> = r.cells.iter().filter(glyph).copied().collect();
            if !r.wrapped {
                while cells.last().is_some_and(|c| c.0 == ' ') {
                    cells.pop();
                }
            }
            acc.extend(cells);
            if !r.wrapped {
                lines.push(std::mem::take(&mut acc));
            }
        }
        if !acc.is_empty() {
            lines.push(acc);
        }
        let mut out: Vec<Row> = vec![];
        let mut cur_rc = (0usize, 0usize);
        for (li, l) in lines.iter().enumerate() {
            let mut r = Row::new(cols);
            let mut x = 0;
            for (i, &(c, d)) in l.iter().enumerate() {
                let w = UnicodeWidthChar::width(c).unwrap_or(1).max(1);
                if x + w > cols {
                    if x < cols {
                        r.cells[x] = (PAD, false); // blank left by a wide glyph
                    }
                    r.wrapped = true;
                    out.push(r);
                    r = Row::new(cols);
                    x = 0;
                }
                if li == cur.0 && i == cur.1 {
                    cur_rc = (out.len(), x);
                }
                r.cells[x] = (c, d);
                if w == 2 {
                    r.cells[x + 1] = ('\0', d);
                }
                x += w;
            }
            if li == cur.0 && cur.1 >= l.len() {
                if x >= cols {
                    out.push(r);
                    r = Row::new(cols);
                    x = 0;
                }
                cur_rc = (out.len(), x);
            }
            out.push(r);
            while li == cur.0 && out.len() <= cur_rc.0 {
                out.push(Row::new(cols));
            }
        }
        // drop blank rows below the cursor when too long, then scroll the top off
        while out.len() > rows
            && out.len() > cur_rc.0 + 1
            && out.last().is_some_and(|r| r.text().is_empty())
        {
            out.pop();
        }
        while out.len() > rows {
            let top = out.remove(0);
            self.scroll.push(top);
            cur_rc.0 = cur_rc.0.saturating_sub(1);
        }
        while out.len() < rows {
            out.push(Row::new(cols));
        }
        self.grid = out;
        self.cols = cols;
        self.rows = rows;
        self.cy = cur_rc.0.min(rows - 1);
        self.cx = cur_rc.1.min(cols - 1);
        self.pending = false;
    }

    /// Visible rows (trailing blanks trimmed), cut after the last non-empty row or the cursor.
    pub fn lines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.grid.iter().map(Row::text).collect();
        while v.len() > self.cy + 1 && v.last().is_some_and(String::is_empty) {
            v.pop();
        }
        v
    }

    /// Scrollback plus screen as one string (for "nothing was duplicated" checks).
    pub fn everything(&self) -> String {
        let mut v: Vec<String> = self.scroll.iter().map(Row::text).collect();
        v.extend(self.lines());
        v.join("\n")
    }

    pub fn dump(&self) -> String {
        let mut s = format!("cursor=({},{}) cols={}\n", self.cx, self.cy, self.cols);
        for (i, r) in self.grid.iter().enumerate() {
            let t = r.text();
            if !t.is_empty() || i <= self.cy {
                s += &format!("{:2}{} |{t}|\n", i, if r.wrapped { "w" } else { " " });
            }
        }
        s
    }

    /// Row `y` as (text, dim-mask-of-first-cells) helpers.
    pub fn row_text(&self, y: usize) -> String {
        self.grid[y].text()
    }
    pub fn row_wrapped(&self, y: usize) -> bool {
        self.grid[y].wrapped
    }
    /// True if the glyph at (x, y) is dim.
    pub fn is_dim(&self, x: usize, y: usize) -> bool {
        self.grid[y].cells[x].1
    }
    /// Rows where the dim `?` hint sits in its column (cols - 2).
    pub fn marker_rows(&self) -> Vec<usize> {
        let x = self.cols - 2;
        (0..self.rows)
            .filter(|&y| self.grid[y].cells[x].0 == '?' && self.grid[y].cells[x].1)
            .collect()
    }
    /// Like `lines`, with the hint removed.
    pub fn lines_no_marker(&self) -> Vec<String> {
        let x = self.cols - 2;
        let mut v = vec![];
        for y in 0..self.rows {
            let mut r = self.grid[y].clone();
            if r.cells[x].0 == '?' && r.cells[x].1 {
                r.cells[x] = (' ', false);
            }
            v.push(r.text());
        }
        while v.len() > self.cy + 1 && v.last().is_some_and(String::is_empty) {
            v.pop();
        }
        v
    }
    /// Index of the last row holding text.
    pub fn last_text_row(&self) -> usize {
        self.grid
            .iter()
            .rposition(|r| !r.text().is_empty())
            .unwrap_or(0)
    }
    /// All text on screen with a given needle (rows joined, wraps unjoined).
    pub fn find_row(&self, needle: &str) -> Option<usize> {
        self.grid.iter().position(|r| r.text().contains(needle))
    }
}

fn param(p: &Params, i: usize, default: usize) -> usize {
    p.iter()
        .nth(i)
        .and_then(|s| s.first().copied())
        .map(|v| v as usize)
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put(c);
    }
    fn execute(&mut self, b: u8) {
        match b {
            b'\r' => {
                self.cx = 0;
                self.pending = false;
            }
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            0x08 => {
                self.cx = self.cx.saturating_sub(1);
                self.pending = false;
            }
            b'\t' => {
                self.cx = ((self.cx / 8 + 1) * 8).min(self.cols - 1);
                self.pending = false;
            }
            _ => {}
        }
    }
    fn csi_dispatch(&mut self, p: &Params, im: &[u8], _ignore: bool, c: char) {
        let q = im.first() == Some(&b'?');
        let n = param(p, 0, 1);
        match (q, c) {
            (false, 'A') => {
                self.cy = self.cy.saturating_sub(n);
                self.pending = false;
            }
            (false, 'B') => {
                self.cy = (self.cy + n).min(self.rows - 1);
                self.pending = false;
            }
            (false, 'C') => {
                self.cx = (self.cx + n).min(self.cols - 1);
                self.pending = false;
            }
            (false, 'D') => {
                self.cx = self.cx.saturating_sub(n);
                self.pending = false;
            }
            (false, 'G') => {
                self.cx = (n - 1).min(self.cols - 1);
                self.pending = false;
            }
            (false, 'd') => {
                self.cy = (n - 1).min(self.rows - 1);
                self.pending = false;
            }
            (false, 'H') | (false, 'f') => {
                self.cy = (n - 1).min(self.rows - 1);
                self.cx = (param(p, 1, 1) - 1).min(self.cols - 1);
                self.pending = false;
            }
            (false, 'J') => {
                let m = p
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                let (cy, cx, cols) = (self.cy, self.cx, self.cols);
                match m {
                    0 => {
                        self.erase_cells(cy, cx, cols);
                        for y in cy + 1..self.rows {
                            self.grid[y] = Row::new(cols);
                        }
                        self.grid[cy].wrapped = false;
                    }
                    1 => {
                        for y in 0..cy {
                            self.grid[y] = Row::new(cols);
                        }
                        self.erase_cells(cy, 0, cx + 1);
                    }
                    _ => {
                        for y in 0..self.rows {
                            self.grid[y] = Row::new(cols);
                        }
                    }
                }
            }
            (false, 'K') => {
                let m = p
                    .iter()
                    .next()
                    .and_then(|s| s.first().copied())
                    .unwrap_or(0);
                let (cy, cx, cols) = (self.cy, self.cx, self.cols);
                match m {
                    0 => {
                        self.erase_cells(cy, cx, cols);
                        self.grid[cy].wrapped = false;
                    }
                    1 => self.erase_cells(cy, 0, cx + 1),
                    _ => {
                        self.erase_cells(cy, 0, cols);
                        self.grid[cy].wrapped = false;
                    }
                }
            }
            (false, 'P') => {
                let (cy, cx, cols) = (self.cy, self.cx, self.cols);
                let n = n.min(cols - cx);
                self.grid[cy].cells.drain(cx..cx + n);
                self.grid[cy].cells.extend(vec![(' ', false); n]);
            }
            (false, '@') => {
                let (cy, cx, cols) = (self.cy, self.cx, self.cols);
                let n = n.min(cols - cx);
                self.grid[cy].cells.truncate(cols - n);
                for _ in 0..n {
                    self.grid[cy].cells.insert(cx, (' ', false));
                }
            }
            (false, 'X') => {
                let (cy, cx) = (self.cy, self.cx);
                self.erase_cells(cy, cx, cx + n);
            }
            (false, 'm') => {
                for s in p.iter() {
                    match s.first().copied().unwrap_or(0) {
                        0 | 22 => self.dim = false,
                        2 => self.dim = true,
                        _ => {}
                    }
                }
            }
            (false, 'n') if n == 6 => {
                let _ = write!(self.replies, "\x1b[{};{}R", self.cy + 1, self.cx + 1);
            }
            (false, 's') => self.saved = (self.cx, self.cy),
            (false, 'u') => {
                (self.cx, self.cy) = self.saved;
                self.pending = false;
            }
            (true, 'h') if n == 1049 => {
                self.alt_saved = Some((
                    std::mem::replace(&mut self.grid, vec![Row::new(self.cols); self.rows]),
                    self.cx,
                    self.cy,
                ));
                self.cx = 0;
                self.cy = 0;
            }
            (true, 'l') if n == 1049 => {
                if let Some((g, x, y)) = self.alt_saved.take() {
                    self.grid = g;
                    self.cx = x;
                    self.cy = y;
                    self.pending = false;
                }
            }
            (true, 'h' | 'l')
                if matches!(
                    n,
                    2004 | 2026 | 25 | 7 | 1 | 1004 | 1000 | 1002 | 1003 | 1006 | 1015
                ) => {}
            (false, 'c' | 'q' | 'r') => {}
            _ => self
                .unknown
                .push(format!("{im:?}{c} {:?}", p.iter().collect::<Vec<_>>())),
        }
    }
    fn esc_dispatch(&mut self, _im: &[u8], _ig: bool, b: u8) {
        match b {
            b'7' => self.saved = (self.cx, self.cy),
            b'8' => (self.cx, self.cy) = self.saved,
            b'M' => self.cy = self.cy.saturating_sub(1),
            _ => {}
        }
    }
}

/// aish under a pty of a fixed size, with an emulated screen.
pub struct Term {
    pub p: Session,
    pub scr: Screen,
    parser: Parser,
}

/// Spawn `cmd` on a pty of `cols`x`rows`, set before aish starts. The pty
/// library sets 80x24 on the master right after the fork, so the child waits
/// for a flag file that is created once our size is in place.
pub fn spawn_sized(cmd: Command, cols: u16, rows: u16) -> Session {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let flag = std::env::temp_dir().join(format!(
        "aish-narrow-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(format!(
            "i=0; while [ ! -e '{}' ] && [ $i -lt 500 ]; do i=$((i+1)); sleep 0.01; done; rm -f '{}'; exec \"$0\"",
            flag.display(),
            flag.display()
        ))
        .arg(cmd.get_program());
    for a in cmd.get_args() {
        c.arg(a);
    }
    c.env_clear();
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            c.env(k, v);
        }
    }
    let mut p = Session::spawn(c).unwrap();
    p.get_process_mut().set_window_size(cols, rows).unwrap();
    std::fs::write(&flag, "").unwrap();
    p
}

impl Term {
    pub fn new(mut p: Session, cols: u16, rows: u16) -> Term {
        p.set_expect_timeout(Some(Duration::from_secs(10)));
        Term {
            p,
            scr: Screen::new(cols as usize, rows as usize),
            parser: Parser::new(),
        }
    }

    /// Read until the output has been quiet for `quiet`.
    pub fn pump_for(&mut self, quiet: Duration) {
        let mut last = Instant::now();
        let mut buf = [0u8; 4096];
        let end = Instant::now() + Duration::from_secs(10);
        while last.elapsed() < quiet && Instant::now() < end {
            match self.p.try_read(&mut buf) {
                Ok(n) if n > 0 => {
                    last = Instant::now();
                    self.scr.raw.extend_from_slice(&buf[..n]);
                    self.parser.advance(&mut self.scr, &buf[..n]);
                    if !self.scr.replies.is_empty() {
                        let r = std::mem::take(&mut self.scr.replies);
                        let _ = self.p.write_all(&r);
                    }
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    pub fn pump(&mut self) {
        self.pump_for(Duration::from_millis(300));
    }

    pub fn send(&mut self, s: &str) {
        self.p.write_all(s.as_bytes()).unwrap();
        self.pump();
    }

    /// Pump until the screen text contains `needle` (panics with a dump).
    pub fn wait_for(&mut self, needle: &str) {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump_for(Duration::from_millis(100));
            if self.scr.everything().contains(needle) {
                self.pump();
                return;
            }
            assert!(
                Instant::now() < end,
                "timeout waiting for {needle:?}\n{}",
                self.scr.dump()
            );
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.pump();
        self.p
            .get_process_mut()
            .set_window_size(cols, rows)
            .unwrap();
        self.scr.resize(cols as usize, rows as usize);
        self.pump();
    }

    /// Nothing may be left of the old screen state: no unknown sequences.
    pub fn assert_clean(&self) {
        assert!(
            self.scr.unknown.is_empty(),
            "unknown sequences: {:?}",
            self.scr.unknown
        );
    }
}
