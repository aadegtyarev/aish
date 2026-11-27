//! Brave Search API (needs a key).

use super::html::strip_tags;
use super::{agent, read_text, Hit};

pub fn parse(json: &str) -> Result<Vec<Hit>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(v["web"]["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Hit {
                        title: strip_tags(r["title"].as_str()?),
                        url: r["url"].as_str()?.to_owned(),
                        snippet: strip_tags(r["description"].as_str().unwrap_or("")),
                    })
                })
                .take(super::MAX_HITS)
                .collect()
        })
        .unwrap_or_default())
}

pub fn search(base: &str, key: &str, q: &str) -> Result<Vec<Hit>, String> {
    let resp = agent()
        .get(base)
        .query("q", q)
        .query("count", &super::MAX_HITS.to_string())
        .set("Accept", "application/json")
        .set("X-Subscription-Token", key)
        .call()
        .map_err(|e| super::short_err(&e))?;
    parse(&read_text(resp, 1024 * 1024)?)
}
