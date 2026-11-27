//! A tiny in-process HTTP server for tests: one thread per connection, the
//! handler writes the whole response itself (so it can stall, stream slowly,
//! or fail). Enabled by the `mock` feature.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Mock {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Req>>>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Req, &mut TcpStream) + Send + Sync + 'static) -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut c) = conn else { continue };
                let (log, handler) = (log.clone(), handler.clone());
                std::thread::spawn(move || {
                    if let Some(r) = read_req(&mut c) {
                        log.lock().unwrap().push(r.clone());
                        handler(&r, &mut c);
                    }
                });
            }
        });
        Mock { port, requests }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_req(c: &mut TcpStream) -> Option<Req> {
    c.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let end = loop {
        let n = c.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < end + len {
        let n = c.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(Req {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&buf[end..]).into_owned(),
    })
}

/// Complete JSON response.
pub fn json(c: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        c,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Start an event stream (body until the connection closes).
pub fn sse_head(c: &mut TcpStream) {
    let _ = c.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    let _ = c.flush();
}

pub fn sse_data(c: &mut TcpStream, data: &str) {
    let _ = write!(c, "data: {data}\n\n");
    let _ = c.flush();
}

/// A Chat Completions chunk carrying `text`.
pub fn chat_chunk(text: &str) -> String {
    serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

/// A Responses text delta event.
pub fn resp_delta(text: &str) -> String {
    serde_json::json!({"type": "response.output_text.delta", "delta": text}).to_string()
}

/// Stream `parts` as a chat answer, then `[DONE]`.
pub fn chat_stream(c: &mut TcpStream, parts: &[&str], delay: Duration) {
    sse_head(c);
    for p in parts {
        sse_data(c, &chat_chunk(p));
        std::thread::sleep(delay);
    }
    sse_data(
        c,
        r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
    );
    sse_data(c, "[DONE]");
}

/// Stream `parts` as a Responses answer, then `response.completed`.
pub fn resp_stream(c: &mut TcpStream, parts: &[&str], delay: Duration) {
    sse_head(c);
    for p in parts {
        sse_data(c, &resp_delta(p));
        std::thread::sleep(delay);
    }
    sse_data(
        c,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":3}}}"#,
    );
}

fn halves(s: &str) -> (&str, &str) {
    let mut i = s.len() / 2;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s.split_at(i)
}

/// A chat answer that calls tools: optional `text`, then each `(id, name,
/// arguments)` with its argument text split over two deltas.
pub fn chat_calls_stream(c: &mut TcpStream, text: &str, calls: &[(&str, &str, &str)]) {
    use serde_json::json;
    sse_head(c);
    if !text.is_empty() {
        sse_data(c, &chat_chunk(text));
    }
    for (i, (id, name, args)) in calls.iter().enumerate() {
        let (a, b) = halves(args);
        let d = |tc: serde_json::Value| json!({"choices": [{"delta": {"tool_calls": [tc]}}]});
        sse_data(
            c,
            &d(json!({"index": i, "id": id, "type": "function",
                "function": {"name": name, "arguments": a}}))
            .to_string(),
        );
        sse_data(
            c,
            &d(json!({"index": i, "function": {"arguments": b}})).to_string(),
        );
    }
    sse_data(
        c,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    );
    sse_data(c, "[DONE]");
}

/// Responses flavour of [`chat_calls_stream`].
pub fn resp_calls_stream(c: &mut TcpStream, text: &str, calls: &[(&str, &str, &str)]) {
    use serde_json::json;
    sse_head(c);
    if !text.is_empty() {
        sse_data(c, &resp_delta(text));
    }
    for (i, (id, name, args)) in calls.iter().enumerate() {
        let item = format!("fc_{i}");
        let (a, b) = halves(args);
        sse_data(
            c,
            &json!({"type": "response.output_item.added", "item": {"type": "function_call",
                "id": item, "call_id": id, "name": name, "arguments": ""}})
            .to_string(),
        );
        for d in [a, b] {
            sse_data(
                c,
                &json!({"type": "response.function_call_arguments.delta",
                    "item_id": item, "delta": d})
                .to_string(),
            );
        }
        sse_data(
            c,
            &json!({"type": "response.function_call_arguments.done",
                "item_id": item, "arguments": args})
            .to_string(),
        );
    }
    sse_data(
        c,
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":3}}}"#,
    );
}
