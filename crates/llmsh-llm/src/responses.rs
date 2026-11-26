//! Responses API: POST {url}/responses with `stream: true`.

use serde_json::{json, Value};

use crate::{Endpoint, Error, Item, Request, Role, Usage};

pub(crate) fn body(ep: &Endpoint, req: &Request) -> String {
    let input: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            json!({"role": role, "content": m.text()})
        })
        .collect();
    let mut b = json!({
        "model": ep.model,
        "input": input,
        "stream": true,
        "max_output_tokens": if ep.effort.is_some() {
            req.max_tokens.max(crate::REASONING_MIN_TOKENS)
        } else {
            req.max_tokens
        },
    });
    if !req.system.is_empty() {
        b["instructions"] = json!(req.system);
    }
    if let Some(e) = &ep.effort {
        b["reasoning"] = json!({"effort": e});
    }
    // Reasoning models reject `temperature`.
    if let (Some(t), None) = (req.temperature, &ep.effort) {
        b["temperature"] = json!(t);
    }
    b.to_string()
}

pub(crate) fn parse(v: &Value) -> Item {
    match v["type"].as_str().unwrap_or("") {
        "response.output_text.delta" | "response.refusal.delta" => match v["delta"].as_str() {
            Some(s) if !s.is_empty() => Item::Delta(s.to_owned()),
            _ => Item::Alive,
        },
        "response.completed" => {
            let u = &v["response"]["usage"];
            Item::Done(Usage {
                input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                truncated: false,
            })
        }
        "error" => Item::Err(Error::Api(
            v["message"]
                .as_str()
                .or(v["error"]["message"].as_str())
                .unwrap_or("error")
                .to_owned(),
        )),
        // Soft end: the partial answer stands (the caller shows a note).
        "response.incomplete" => Item::Done(Usage {
            input_tokens: v["response"]["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["response"]["usage"]["output_tokens"]
                .as_u64()
                .unwrap_or(0),
            truncated: true,
        }),
        "response.failed" => Item::Err(Error::Api(
            v["response"]["error"]["message"]
                .as_str()
                .or(v["response"]["incomplete_details"]["reason"].as_str())
                .unwrap_or("failed")
                .to_owned(),
        )),
        _ => Item::Alive,
    }
}
