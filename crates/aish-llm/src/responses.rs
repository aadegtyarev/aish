//! Responses API: POST {url}/responses with `stream: true`.

use serde_json::{json, Value};

use crate::{Endpoint, Error, Item, Part, Request, Role, State, ToolCall, Usage};

pub(crate) fn body(ep: &Endpoint, req: &Request) -> String {
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let text = m.text();
        if !text.is_empty() || m.parts.is_empty() {
            input.push(json!({"role": role, "content": text}));
        }
        for p in &m.parts {
            match p {
                Part::ToolCall(c) => input.push(json!({"type": "function_call",
                    "call_id": c.id, "name": c.name, "arguments": c.arguments})),
                Part::ToolResult { id, content } => input.push(json!({
                    "type": "function_call_output", "call_id": id, "output": content})),
                Part::Text(_) => {}
            }
        }
    }
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
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({"type": "function", "name": t.name,
                "description": t.description, "parameters": t.parameters})
        })
        .collect();
    if req.native_search {
        tools.push(json!({"type": "web_search"}));
    }
    if !tools.is_empty() {
        b["tools"] = json!(tools);
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

fn slot(st: &mut State, item: &str) -> usize {
    if let Some(i) = st.item_ids.iter().position(|x| x == item) {
        return i;
    }
    st.item_ids.push(item.to_owned());
    st.calls.push(ToolCall {
        id: String::new(),
        name: String::new(),
        arguments: String::new(),
    });
    st.calls.len() - 1
}

pub(crate) fn parse(v: &Value, st: &mut State) -> Item {
    match v["type"].as_str().unwrap_or("") {
        "response.output_text.delta" | "response.refusal.delta" => match v["delta"].as_str() {
            Some(s) if !s.is_empty() => Item::Delta(s.to_owned()),
            _ => Item::Alive,
        },
        // the provider's own web search: only a status line, never a tool call
        "response.output_item.done" if v["item"]["type"] == "web_search_call" => {
            let a = &v["item"]["action"];
            let q = a["query"]
                .as_str()
                .or_else(|| a["queries"][0].as_str())
                .unwrap_or("");
            Item::Search(q.to_owned())
        }
        "response.output_item.added" | "response.output_item.done"
            if v["item"]["type"] == "function_call" =>
        {
            let i = slot(st, v["item"]["id"].as_str().unwrap_or(""));
            let c = &mut st.calls[i];
            if let Some(id) = v["item"]["call_id"].as_str() {
                c.id = id.to_owned();
            }
            if let Some(n) = v["item"]["name"].as_str() {
                c.name = n.to_owned();
            }
            // the finished item carries the whole argument text
            if v["type"] == "response.output_item.done" {
                if let Some(a) = v["item"]["arguments"].as_str() {
                    c.arguments = a.to_owned();
                }
            }
            Item::Alive
        }
        "response.function_call_arguments.delta" => {
            let i = slot(st, v["item_id"].as_str().unwrap_or(""));
            if let Some(d) = v["delta"].as_str() {
                st.calls[i].arguments.push_str(d);
            }
            Item::Alive
        }
        "response.function_call_arguments.done" => {
            let i = slot(st, v["item_id"].as_str().unwrap_or(""));
            if let Some(a) = v["arguments"].as_str() {
                st.calls[i].arguments = a.to_owned();
            }
            Item::Alive
        }
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
