//! Chat Completions: POST {url}/chat/completions with `stream: true`.

use serde_json::{json, Value};

use crate::{Endpoint, Error, Item, Request, Role, Usage};

pub(crate) fn messages(req: &Request) -> Vec<Value> {
    let mut m = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        m.push(json!({"role": "system", "content": req.system}));
    }
    for x in &req.messages {
        let role = match x.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        m.push(json!({"role": role, "content": x.text()}));
    }
    m
}

pub(crate) fn body(ep: &Endpoint, req: &Request) -> String {
    let mut b = json!({
        "model": ep.model,
        "messages": messages(req),
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_tokens,
    });
    if let Some(e) = &ep.effort {
        b["reasoning_effort"] = json!(e);
    }
    if let Some(t) = req.temperature {
        b["temperature"] = json!(t);
    }
    if !req.stop.is_empty() {
        b["stop"] = json!(req.stop);
    }
    b.to_string()
}

pub(crate) fn parse(v: &Value, usage: &mut Usage) -> Item {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let m = e["message"].as_str().or(e.as_str()).unwrap_or("error");
        return Item::Err(Error::Api(m.to_owned()));
    }
    if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
        usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
        usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
    }
    let d = &v["choices"][0]["delta"];
    match d["content"].as_str() {
        Some(s) if !s.is_empty() => Item::Delta(s.to_owned()),
        _ if d["reasoning_content"].is_string() || d["reasoning"].is_string() => Item::Alive,
        _ if d.is_object() => Item::Alive,
        _ => Item::Skip,
    }
}
