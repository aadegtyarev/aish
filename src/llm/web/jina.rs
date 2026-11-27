//! Jina Reader: renders the page (also JS-heavy ones) to markdown.

use super::{agent, read_text};

pub fn read(base: &str, key: Option<&str>, url: &str) -> Result<String, String> {
    let mut req = agent()
        .get(&format!("{}/{url}", base.trim_end_matches('/')))
        .set("Accept", "text/plain");
    if let Some(k) = key {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    let resp = req.call().map_err(|e| super::short_err(&e))?;
    read_text(resp, super::BODY_CAP)
}
