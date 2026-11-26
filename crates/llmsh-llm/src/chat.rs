//! Chat Completions: POST {url}/chat/completions with `stream: true`.

use serde_json::{json, Value};

use crate::{Endpoint, Error, Item, Request, Role, State};

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

/// Reasoning models (and OpenAI itself) take `max_completion_tokens` and reject
/// `temperature`/`stop`; other servers keep the classic parameters.
fn reasoning_style(ep: &Endpoint) -> bool {
    ep.effort.is_some() || ep.url.contains("api.openai.com")
}

pub(crate) fn body(ep: &Endpoint, req: &Request) -> String {
    let mut b = json!({
        "model": ep.model,
        "messages": messages(req),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if reasoning_style(ep) {
        // Reasoning tokens count against the limit: leave room for them.
        let n = if ep.effort.is_some() {
            req.max_tokens.max(crate::REASONING_MIN_TOKENS)
        } else {
            req.max_tokens
        };
        b["max_completion_tokens"] = json!(n);
    } else {
        b["max_tokens"] = json!(req.max_tokens);
        if let Some(t) = req.temperature {
            b["temperature"] = json!(t);
        }
        if !req.stop.is_empty() {
            b["stop"] = json!(req.stop);
        }
    }
    if let Some(e) = &ep.effort {
        b["reasoning_effort"] = json!(e);
    }
    b.to_string()
}

pub(crate) fn parse(v: &Value, st: &mut State) -> Item {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let m = e["message"].as_str().or(e.as_str()).unwrap_or("error");
        return Item::Err(Error::Api(m.to_owned()));
    }
    if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
        st.usage.input_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
        st.usage.output_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
    }
    if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
        st.finished = true;
        if r == "length" {
            st.usage.truncated = true;
        }
    }
    let d = &v["choices"][0]["delta"];
    match d["content"].as_str() {
        Some(s) if !s.is_empty() => Item::Delta(s.to_owned()),
        _ if d["reasoning_content"].is_string() || d["reasoning"].is_string() => Item::Alive,
        _ if d.is_object() => Item::Alive,
        _ => Item::Skip,
    }
}
