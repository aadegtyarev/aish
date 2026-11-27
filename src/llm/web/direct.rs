//! Our own page fetch: public hosts only, vetted at connect time, every
//! redirect hop re-checked, size and time limited, no cookies.

use std::io::Read;
use std::time::Duration;

use super::html;
use super::ssrf::Vetted;
use super::BODY_CAP;

const MAX_HOPS: usize = 5;
/// Less visible text than this: probably a JS-rendered shell.
const NEAR_EMPTY: usize = 120;

pub struct Page {
    pub text: String,
    /// Near-empty (JS shell): worth trying the next backend.
    pub weak: bool,
}

fn is_text_type(ct: &str) -> bool {
    ct.starts_with("text/") || matches!(ct, "application/json" | "application/xhtml+xml")
}

/// Short reason for a transport error (the resolver's refusal included).
fn reason(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(c, _) => format!("http {c}"),
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            if s.contains("blocked") {
                "blocked: not a public address".to_owned()
            } else {
                super::short_err(e)
            }
        }
    }
}

pub fn fetch(start: &str, allow: &[String]) -> Result<Page, String> {
    let agent = ureq::AgentBuilder::new()
        .resolver(Vetted {
            allow: allow.to_vec(),
        })
        .redirects(0)
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; aish)")
        .build();
    let mut url = url::Url::parse(start).map_err(|_| "invalid URL".to_owned())?;
    for _ in 0..=MAX_HOPS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("only http and https URLs are allowed".into());
        }
        if url.host_str().is_none() {
            return Err("URL has no host".into());
        }
        let resp = agent
            .get(url.as_str())
            .set(
                "Accept",
                "text/html,text/plain,application/json;q=0.9,*/*;q=0.1",
            )
            .call()
            .map_err(|e| reason(&e))?;
        if (300..400).contains(&resp.status()) {
            let loc = resp.header("location").ok_or("redirect without Location")?;
            url = url
                .join(loc)
                .map_err(|_| "bad redirect target".to_owned())?;
            continue;
        }
        let ct = resp.content_type().to_ascii_lowercase();
        if !is_text_type(&ct) {
            return Err(format!("unsupported content type {ct}"));
        }
        let mut buf = Vec::new();
        resp.into_reader()
            .take(BODY_CAP as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("read: {e}"))?;
        let body = String::from_utf8_lossy(&buf);
        let is_html = ct.contains("html") && !body.trim_start().starts_with(['{', '[']);
        let text = if is_html {
            html::to_text(&body, Some(&url))
        } else {
            body.trim().to_owned()
        };
        let weak = is_html && text.chars().filter(|c| !c.is_whitespace()).count() < NEAR_EMPTY;
        return Ok(Page { text, weak });
    }
    Err("too many redirects".into())
}
