//! Chat Completions: POST {url}/chat/completions with `stream: true`.

use serde_json::{json, Value};

use crate::{Endpoint, Error, Item, Part, Request, Role, State, ToolCall};

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
        let calls: Vec<Value> = x
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(json!({
                    "id": c.id, "type": "function",
                    "function": {"name": c.name, "arguments": c.arguments},
                })),
                _ => None,
            })
            .collect();
        let results: Vec<&Part> = x
            .parts
            .iter()
            .filter(|p| matches!(p, Part::ToolResult { .. }))
            .collect();
        for r in results {
            if let Part::ToolResult { id, content } = r {
                m.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
            }
        }
        let text = x.text();
        if !calls.is_empty() {
            let c = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            m.push(json!({"role": role, "content": c, "tool_calls": calls}));
        } else if !text.is_empty() || x.parts.is_empty() {
            m.push(json!({"role": role, "content": text}));
        }
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
    if !req.tools.is_empty() {
        b["tools"] = json!(req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {
                "name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect::<Vec<_>>());
    }
    if let Some(g) = &req.grammar {
        b["grammar"] = json!(g);
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
    if let Some(tc) = d["tool_calls"].as_array() {
        for c in tc {
            let i = c["index"].as_u64().unwrap_or(0) as usize;
            if i > 64 {
                continue;
            }
            while st.calls.len() <= i {
                st.calls.push(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
            }
            let t = &mut st.calls[i];
            if let Some(id) = c["id"].as_str() {
                t.id = id.to_owned();
            }
            if let Some(n) = c["function"]["name"].as_str() {
                t.name.push_str(n);
            }
            if let Some(a) = c["function"]["arguments"].as_str() {
                t.arguments.push_str(a);
            }
        }
        return Item::Alive;
    }
    match d["content"].as_str() {
        Some(s) if !s.is_empty() => Item::Delta(s.to_owned()),
        _ if d["reasoning_content"].is_string() || d["reasoning"].is_string() => Item::Alive,
        _ if d.is_object() => Item::Alive,
        _ => Item::Skip,
    }
}
