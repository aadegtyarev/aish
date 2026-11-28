//! Questions to the model server itself (llama.cpp): context size and load state.

use std::time::Duration;

use serde_json::Value;

use crate::{authorize, Endpoint};

fn get_json(url: &str, key: &Option<String>, timeout: Duration) -> Option<Value> {
    let r = authorize(ureq::get(url).timeout(timeout), key)
        .call()
        .ok()?;
    serde_json::from_str(&r.into_string().ok()?).ok()
}

/// `n_ctx` from a llama.cpp `/props` answer.
fn parse_props(v: &Value) -> Option<u64> {
    v["default_generation_settings"]["n_ctx"]
        .as_u64()
        .or_else(|| v["n_ctx"].as_u64())
        .filter(|n| *n > 0)
        .map(|n| n.clamp(2048, 4_000_000))
}

/// `GET <base>/props` for the provider URL `http://host:port/v1`.
fn props_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let u = u.strip_suffix("/v1").unwrap_or(u);
    format!("{u}/props")
}

/// Ask a llama.cpp server for its context size; any failure is `None`.
pub fn fetch_n_ctx(url: &str, key: &Option<String>) -> Option<u64> {
    parse_props(&get_json(
        &props_url(url),
        key,
        Duration::from_millis(1500),
    )?)
}

/// llama.cpp router mode: is the target model `unloaded`/`loading`? None when
/// the server does not say (the field is absent, or it cannot be asked).
pub fn model_loading(ep: &Endpoint) -> Option<bool> {
    let url = format!("{}/models", ep.url.trim_end_matches('/'));
    let v = get_json(&url, &ep.key, Duration::from_secs(2))?;
    let m = v["data"]
        .as_array()?
        .iter()
        .find(|m| m["id"].as_str() == Some(ep.model.as_str()))?;
    let s = m["status"]["value"].as_str()?;
    Some(matches!(s, "unloaded" | "loading"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_parsing() {
        let p = |s: &str| parse_props(&serde_json::from_str(s).unwrap());
        assert_eq!(
            p(r#"{"default_generation_settings":{"n_ctx":32768,"x":1},"total_slots":1}"#),
            Some(32768)
        );
        assert_eq!(p(r#"{"n_ctx":4096}"#), Some(4096));
        assert_eq!(p(r#"{"n_ctx":0}"#), None);
        assert_eq!(p(r#"{"n_ctx":512}"#), Some(2048));
        assert_eq!(p(r#"{"n_ctx":99999999}"#), Some(4_000_000));
        assert_eq!(p("{}"), None);
        assert_eq!(props_url("http://h:8080/v1"), "http://h:8080/props");
        assert_eq!(props_url("http://h:8080/v1/"), "http://h:8080/props");
        assert_eq!(props_url("http://h:8080"), "http://h:8080/props");
    }
}
