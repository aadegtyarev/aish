//! Streaming clients for OpenAI-compatible APIs: Chat Completions (`chat`)
//! and Responses (`responses`).
//!
//! Blocking HTTP (`ureq`) runs on a worker thread; the caller consumes events
//! from a channel with short waits, so cancellation (a shared flag) and the
//! first-token timeout are honoured even while the server is silent.

mod chat;
#[cfg(feature = "mock")]
pub mod mock;
mod responses;

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const FIRST_TOKEN_CHAT: Duration = Duration::from_secs(30);
pub const FIRST_TOKEN_COMPLETE: Duration = Duration::from_millis(1500);
/// Silence after the first token before the stream is considered dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Chat,
    Responses,
}

/// Where and how to talk: one provider + model.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub kind: Kind,
    /// Base URL, e.g. `https://api.openai.com/v1`.
    pub url: String,
    pub key: Option<String>,
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// A tool call requested by the model (reserved for tool use; not sent yet).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    /// Reserved: tool calls are not serialized yet.
    ToolCall(ToolCall),
    /// Reserved: tool results are not serialized yet.
    ToolResult {
        id: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            parts: vec![Part::Text(text.into())],
        }
    }
    /// Concatenated text parts.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub stop: Vec<String>,
    /// Give up if nothing at all (not even reasoning) arrives within this time.
    pub first_token_timeout: Duration,
}

impl Request {
    pub fn chat(system: impl Into<String>, messages: Vec<Message>) -> Self {
        Request {
            system: system.into(),
            messages,
            max_tokens: 2048,
            temperature: None,
            stop: Vec::new(),
            first_token_timeout: FIRST_TOKEN_CHAT,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A piece of the answer text (reasoning is never reported).
    Delta(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Could not connect / transport failure.
    Connect(String),
    /// HTTP status >= 400 with the message from the error JSON, if any.
    Http {
        status: u16,
        message: String,
    },
    /// Error reported inside the stream.
    Api(String),
    /// No first token (or the stream went silent) in time.
    Timeout,
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "connect: {e}"),
            Error::Http { status, message } => write!(f, "http {status}: {message}"),
            Error::Api(m) => write!(f, "api: {m}"),
            Error::Timeout => write!(f, "timeout"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

/// Worker -> consumer.
enum Msg {
    /// Something arrived (reasoning, keep-alive): resets the timeouts.
    Alive,
    Delta(String),
    Done(Usage),
    Err(Error),
}

/// One parsed SSE payload, API independent.
pub(crate) enum Item {
    Alive,
    Delta(String),
    Done(Usage),
    Err(Error),
    Skip,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(IDLE_TIMEOUT)
        .build()
}

fn authorize(r: ureq::Request, key: &Option<String>) -> ureq::Request {
    match key {
        Some(k) => r.set("Authorization", &format!("Bearer {k}")),
        None => r,
    }
}

fn error_message(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    v["error"]["message"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .or_else(|| v["message"].as_str())
        .map_or_else(|| body.chars().take(200).collect(), str::to_owned)
}

fn map_err(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Status(status, resp) => {
            let mut body = String::new();
            let _ = resp.into_reader().take(16 * 1024).read_to_string(&mut body);
            Error::Http {
                status,
                message: error_message(&body),
            }
        }
        ureq::Error::Transport(t) => Error::Connect(t.to_string()),
    }
}

/// Run a streaming request. `on_event` is called on the calling thread.
/// `cancel` is polled every few milliseconds, also while the server is silent.
pub fn stream(
    ep: &Endpoint,
    req: &Request,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> Result<Usage, Error> {
    let (path, body) = match ep.kind {
        Kind::Chat => ("chat/completions", chat::body(ep, req)),
        Kind::Responses => ("responses", responses::body(ep, req)),
    };
    let url = format!("{}/{path}", ep.url.trim_end_matches('/'));
    let (tx, rx) = mpsc::channel::<Msg>();
    let closed = Arc::new(AtomicBool::new(false));
    {
        let (key, kind, closed) = (ep.key.clone(), ep.kind, closed.clone());
        std::thread::spawn(move || worker(&url, key, kind, body, tx, &closed));
    }
    let start = Instant::now();
    let mut last = start;
    let mut first = true;
    let out = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(Error::Cancelled);
        }
        let limit = if first {
            req.first_token_timeout
        } else {
            IDLE_TIMEOUT
        };
        if last.elapsed() >= limit {
            break Err(Error::Timeout);
        }
        match rx.recv_timeout(TICK.min(limit - last.elapsed())) {
            Ok(Msg::Alive) => {
                first = false;
                last = Instant::now();
            }
            Ok(Msg::Delta(s)) => {
                first = false;
                last = Instant::now();
                on_event(Event::Delta(s));
            }
            Ok(Msg::Done(u)) => break Ok(u),
            Ok(Msg::Err(e)) => break Err(e),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Err(Error::Api("stream closed".into())),
        }
    };
    // Tells the reader thread to drop the connection at its next line.
    closed.store(true, Ordering::Relaxed);
    out
}

/// Convenience: collect the whole answer.
pub fn complete(ep: &Endpoint, req: &Request, cancel: &AtomicBool) -> Result<String, Error> {
    let mut s = String::new();
    stream(ep, req, cancel, |Event::Delta(d)| s.push_str(&d))?;
    Ok(s)
}

fn worker(
    url: &str,
    key: Option<String>,
    kind: Kind,
    body: String,
    tx: mpsc::Sender<Msg>,
    closed: &AtomicBool,
) {
    let r = authorize(agent().post(url), &key)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream")
        .send_string(&body);
    let resp = match r {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Msg::Err(map_err(e)));
            return;
        }
    };
    let mut rd = BufReader::new(resp.into_reader());
    let mut line = Vec::with_capacity(512);
    let mut usage = Usage::default();
    loop {
        if closed.load(Ordering::Relaxed) {
            return; // dropping `rd` closes the connection
        }
        line.clear();
        match rd.read_until(b'\n', &mut line) {
            Ok(0) => {
                let _ = tx.send(Msg::Done(usage));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Msg::Err(if e.kind() == std::io::ErrorKind::TimedOut {
                    Error::Timeout
                } else {
                    Error::Connect(e.to_string())
                }));
                return;
            }
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let Some(data) = text.strip_prefix("data:") else {
            continue; // `event:`, comments, blank separators
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            let _ = tx.send(Msg::Done(usage));
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let item = match kind {
            Kind::Chat => chat::parse(&v, &mut usage),
            Kind::Responses => responses::parse(&v),
        };
        let m = match item {
            Item::Skip => continue,
            Item::Alive => Msg::Alive,
            Item::Delta(s) => Msg::Delta(s),
            Item::Done(u) => {
                let _ = tx.send(Msg::Done(u));
                return;
            }
            Item::Err(e) => {
                let _ = tx.send(Msg::Err(e));
                return;
            }
        };
        if tx.send(m).is_err() {
            return;
        }
    }
}

/// `GET {url}/models`: the model ids the server offers.
pub fn list_models(
    url: &str,
    key: &Option<String>,
    timeout: Duration,
) -> Result<Vec<String>, Error> {
    let ag = ureq::AgentBuilder::new().timeout(timeout).build();
    let r = authorize(
        ag.get(&format!("{}/models", url.trim_end_matches('/'))),
        key,
    )
    .call()
    .map_err(map_err)?;
    let mut s = String::new();
    r.into_reader()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut s)
        .map_err(|e| Error::Connect(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| Error::Api(e.to_string()))?;
    Ok(v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(all(test, feature = "mock"))]
mod tests;
