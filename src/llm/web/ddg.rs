//! DuckDuckGo HTML endpoint (keyless).

use super::html::{attr, decode_entities, strip_tags, url_decode};
use super::{agent, read_text, Hit};

/// The real target of a result link: `//duckduckgo.com/l/?uddg=<enc>&...`.
fn unwrap_link(href: &str) -> Option<String> {
    if let Some(p) = href.find("uddg=") {
        let enc = href[p + 5..].split('&').next().unwrap_or("");
        let u = url_decode(enc);
        return u.starts_with("http").then_some(u);
    }
    if href.starts_with("//") {
        return Some(format!("https:{href}"));
    }
    href.starts_with("http").then(|| href.to_owned())
}

/// Content of the element opened by the tag around `class_at` (up to `</a>`).
fn element(html: &str, class_at: usize) -> Option<(String, String, usize)> {
    let lt = html[..class_at].rfind('<')?;
    let gt = lt + html[lt..].find('>')?;
    let attrs = html[lt + 1..gt].trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    let end = gt + 1 + html[gt + 1..].find("</a>")?;
    Some((
        attr(attrs, "href").unwrap_or_default(),
        strip_tags(&html[gt + 1..end]),
        end,
    ))
}

pub fn parse(html: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut pos = 0;
    while let Some(r) = html[pos..].find("result__a") {
        let at = pos + r;
        pos = at + 9;
        let Some((href, title, end)) = element(html, at) else {
            continue;
        };
        let next = html[end..]
            .find("result__a")
            .map_or(html.len(), |n| end + n);
        let snippet = html[end..next]
            .find("result__snippet")
            .and_then(|s| element(html, end + s))
            .map(|(_, t, _)| t)
            .unwrap_or_default();
        let Some(url) = unwrap_link(&decode_entities(&href)) else {
            continue;
        };
        if url.contains("duckduckgo.com/y.js") || title.is_empty() {
            continue; // ads
        }
        hits.push(Hit {
            title,
            url,
            snippet,
        });
        if hits.len() >= super::MAX_HITS {
            break;
        }
    }
    hits
}

pub fn search(base: &str, q: &str) -> Result<Vec<Hit>, String> {
    let resp = agent()
        .post(base)
        .send_form(&[("q", q)])
        .map_err(|e| super::short_err(&e))?;
    let body = read_text(resp, 1024 * 1024)?;
    Ok(parse(&body))
}
