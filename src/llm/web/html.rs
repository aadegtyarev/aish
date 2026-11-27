//! A small HTML helper set: readable-text extraction, attribute and entity
//! handling. Hand-written on purpose (no heavy dependencies); it is lenient
//! and never panics on broken markup.

/// Tags whose whole content is dropped.
const SKIP: &[&str] = &[
    "script", "style", "nav", "footer", "noscript", "svg", "template", "iframe", "select",
];
/// Tags that end the current line.
const LINE: &[&str] = &[
    "br", "li", "tr", "div", "section", "article", "header", "main", "dl", "dt", "dd", "form",
    "figure", "hr", "aside", "details", "summary", "address",
];
/// Tags that are surrounded by a blank line.
const PARA: &[&str] = &[
    "p",
    "ul",
    "ol",
    "table",
    "blockquote",
    "pre",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
];

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.char_indices().take(12).find(|(_, c)| *c == ';');
        let dec = end.and_then(|(e, _)| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                "hellip" => '…',
                "mdash" => '—',
                "ndash" => '–',
                "laquo" => '«',
                "raquo" => '»',
                "copy" => '©',
                n if n.starts_with("#x") || n.starts_with("#X") => {
                    char::from_u32(u32::from_str_radix(&n[2..], 16).ok()?)?
                }
                n if n.starts_with('#') => char::from_u32(n[1..].parse().ok()?)?,
                _ => return None,
            };
            Some((c, e))
        });
        match dec {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Attribute `name` of a tag's attribute text (`class="x" href=y`).
pub fn attr(attrs: &str, name: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        let key = &attrs[ks..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut val = "";
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                i = vs;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                val = &attrs[vs..i];
                i += 1;
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                val = &attrs[vs..i];
            }
        }
        if !key.is_empty() && key.eq_ignore_ascii_case(name) {
            return Some(decode_entities(val));
        }
    }
    None
}

/// Index of the `>` closing the tag that starts at `lt` (quotes respected).
fn tag_end(html: &str, lt: usize) -> Option<usize> {
    let mut q: Option<u8> = None;
    for (i, &c) in html.as_bytes().iter().enumerate().skip(lt + 1) {
        match (q, c) {
            (Some(x), c) if c == x => q = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => q = Some(c),
            (None, b'>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// Text of `html` without tags, entities decoded, whitespace collapsed.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < html.len() {
        match html[i..].find('<') {
            None => {
                out.push_str(&html[i..]);
                break;
            }
            Some(r) => {
                out.push_str(&html[i..i + r]);
                i = match tag_end(html, i + r) {
                    Some(e) => e + 1,
                    None => html.len(),
                };
            }
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

struct Ex {
    out: String,
    space: bool,
}

impl Ex {
    fn text(&mut self, s: &str) {
        for ch in decode_entities(s).chars() {
            if ch.is_whitespace() || ch == '\u{a0}' {
                self.space = true;
            } else if !ch.is_control() {
                if self.space && !self.out.is_empty() && !self.out.ends_with('\n') {
                    self.out.push(' ');
                }
                self.space = false;
                self.out.push(ch);
            }
        }
    }

    /// Make the output end with `n` newlines (nothing at the very start).
    fn nl(&mut self, n: usize) {
        self.space = false;
        if self.out.is_empty() {
            return;
        }
        let t = self.out.trim_end_matches(' ').len();
        self.out.truncate(t);
        let have = self.out.chars().rev().take_while(|c| *c == '\n').count();
        for _ in have..n {
            self.out.push('\n');
        }
    }
}

/// End of the element whose content starts at `from`, for `tag` (nesting of
/// the same tag counted); `html.len()` if never closed.
fn skip_element(lower: &str, from: usize, tag: &str) -> usize {
    let (open, close) = (format!("<{tag}"), format!("</{tag}"));
    let mut depth = 1;
    let mut i = from;
    while depth > 0 {
        let o = lower[i..].find(&open).map(|x| x + i);
        let c = lower[i..].find(&close).map(|x| x + i);
        let boundary = |p: usize, len: usize| {
            lower
                .as_bytes()
                .get(p + len)
                .is_none_or(|b| !b.is_ascii_alphanumeric())
        };
        match (o, c) {
            (_, None) => return lower.len(),
            (Some(o), Some(c)) if o < c && boundary(o, open.len()) => {
                depth += 1;
                i = o + open.len();
            }
            (Some(o), Some(c)) if o < c => i = o + open.len(),
            (_, Some(c)) => {
                if boundary(c, close.len()) {
                    depth -= 1;
                }
                i = c + close.len();
            }
        }
    }
    match lower[i..].find('>') {
        Some(g) => i + g + 1,
        None => lower.len(),
    }
}

/// Readable markdown-ish text of an HTML page: headings, paragraphs, lists and
/// links are kept; scripts, styles, navigation and footers are dropped.
pub fn to_text(html: &str, base: Option<&url::Url>) -> String {
    let lower = html.to_ascii_lowercase();
    let mut ex = Ex {
        out: String::new(),
        space: false,
    };
    let mut title = String::new();
    let mut link: Option<(String, usize)> = None;
    let mut i = 0;
    while i < html.len() {
        let Some(r) = html[i..].find('<') else {
            ex.text(&html[i..]);
            break;
        };
        ex.text(&html[i..i + r]);
        let lt = i + r;
        if html[lt..].starts_with("<!--") {
            i = html[lt..].find("-->").map_or(html.len(), |e| lt + e + 3);
            continue;
        }
        let Some(gt) = tag_end(html, lt) else { break };
        i = gt + 1;
        let body = &html[lt + 1..gt];
        if body.starts_with(['!', '?']) {
            continue;
        }
        let closing = body.starts_with('/');
        let inner = body.trim_start_matches('/');
        let name_len = inner
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(inner.len());
        let name = inner[..name_len].to_ascii_lowercase();
        let attrs = &inner[name_len..];
        if closing {
            match name.as_str() {
                "a" => {
                    if let Some((href, start)) = link.take() {
                        let t = ex.out.get(start..).unwrap_or("").trim().to_owned();
                        if !t.is_empty() {
                            ex.out.truncate(start);
                            ex.out.push_str(&format!("[{t}]({href})"));
                        }
                    }
                }
                n if PARA.contains(&n) => ex.nl(2),
                n if LINE.contains(&n) => ex.nl(1),
                _ => {}
            }
            continue;
        }
        let selfclosed = body.ends_with('/');
        if name == "title" && !selfclosed {
            let end = skip_element(&lower, i, "title");
            let close = lower[i..end].rfind("</title").map_or(end, |p| i + p);
            title = strip_tags(&html[i..close]);
            i = end;
            continue;
        }
        if SKIP.contains(&name.as_str()) {
            if !selfclosed {
                i = skip_element(&lower, i, &name);
            }
            continue;
        }
        match name.as_str() {
            "a" => {
                let href = attr(attrs, "href").and_then(|h| resolve(&h, base));
                link = href.map(|h| (h, ex.out.len()));
            }
            "li" => {
                ex.nl(1);
                ex.out.push_str("- ");
            }
            n if n.len() == 2 && n.starts_with('h') && n.as_bytes()[1].is_ascii_digit() => {
                ex.nl(2);
                let lvl = (n.as_bytes()[1] - b'0') as usize;
                ex.out.push_str(&"#".repeat(lvl.clamp(1, 6)));
                ex.out.push(' ');
            }
            n if PARA.contains(&n) => ex.nl(2),
            n if LINE.contains(&n) => ex.nl(1),
            "td" | "th" => ex.space = true,
            _ => {}
        }
    }
    let mut text = String::new();
    if !title.is_empty() {
        text.push_str(&format!("# {title}\n\n"));
    }
    let mut blank = 0;
    for l in ex.out.lines() {
        let l = l.trim();
        if l.is_empty() {
            blank += 1;
            if blank == 1 {
                text.push('\n');
            }
        } else {
            blank = 0;
            text.push_str(l);
            text.push('\n');
        }
    }
    text.trim().to_owned()
}

fn resolve(href: &str, base: Option<&url::Url>) -> Option<String> {
    let h = href.trim();
    let low = h.to_ascii_lowercase();
    if h.is_empty()
        || h.starts_with('#')
        || low.starts_with("javascript:")
        || low.starts_with("mailto:")
    {
        return None;
    }
    match base {
        Some(b) => b.join(h).ok().map(|u| u.to_string()),
        None => Some(h.to_owned()),
    }
}
