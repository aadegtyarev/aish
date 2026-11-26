use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::mock::*;
use crate::*;

fn ep(kind: Kind, m: &Mock) -> Endpoint {
    Endpoint {
        kind,
        url: m.url(),
        key: Some("sk-test".into()),
        model: "m1".into(),
        effort: Some("low".into()),
    }
}

fn run(e: &Endpoint, req: &Request) -> (Result<Usage, Error>, String) {
    let mut s = String::new();
    let r = stream(e, req, &AtomicBool::new(false), |Event::Delta(d)| {
        s.push_str(&d)
    });
    (r, s)
}

fn req() -> Request {
    Request {
        max_tokens: 99,
        ..Request::chat("sys", vec![Message::user("hi")])
    }
}

#[test]
fn chat_streams_and_sends_expected_request() {
    let m = Mock::start(|_, c| chat_stream(c, &["Hel", "lo ", "wor", "ld"], Duration::ZERO));
    let (r, s) = run(&ep(Kind::Chat, &m), &req());
    assert_eq!(s, "Hello world");
    assert_eq!(
        r.unwrap(),
        Usage {
            input_tokens: 7,
            output_tokens: 3,
            truncated: false
        }
    );
    let q = &m.requests()[0];
    assert_eq!(q.path, "/v1/chat/completions");
    assert_eq!(q.header("authorization"), Some("Bearer sk-test"));
    let b: serde_json::Value = serde_json::from_str(&q.body).unwrap();
    assert_eq!(b["stream"], true);
    // effort set: reasoning style, with room for thinking, no temperature/stop
    assert_eq!(b["max_completion_tokens"], 8192);
    assert!(b.get("max_tokens").is_none() && b.get("temperature").is_none());
    assert_eq!(b["reasoning_effort"], "low");
    assert_eq!(b["messages"][0]["role"], "system");
    assert_eq!(b["messages"][1]["content"], "hi");
}

#[test]
fn responses_streams_and_ignores_reasoning() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(
            c,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        );
        sse_data(c, &resp_delta("ans"));
        sse_data(c, &resp_delta("wer"));
        sse_data(
            c,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":2}}}"#,
        );
    });
    let (r, s) = run(&ep(Kind::Responses, &m), &req());
    assert_eq!(s, "answer");
    assert_eq!(r.unwrap().input_tokens, 5);
    let q = &m.requests()[0];
    assert_eq!(q.path, "/v1/responses");
    let b: serde_json::Value = serde_json::from_str(&q.body).unwrap();
    assert_eq!(b["max_output_tokens"], 8192);
    assert_eq!(b["reasoning"]["effort"], "low");
    assert_eq!(b["instructions"], "sys");
}

#[test]
fn error_json_and_stream_error() {
    let m = Mock::start(|_, c| json(c, 401, r#"{"error":{"message":"bad key"}}"#));
    let (r, _) = run(&ep(Kind::Chat, &m), &req());
    assert_eq!(
        r,
        Err(Error::Http {
            status: 401,
            message: "bad key".into()
        })
    );
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &resp_delta("x"));
        sse_data(c, r#"{"type":"error","message":"overloaded"}"#);
    });
    let (r, s) = run(&ep(Kind::Responses, &m), &req());
    assert_eq!(r, Err(Error::Api("overloaded".into())));
    assert_eq!(s, "x");
}

#[test]
fn cancel_mid_stream_is_prompt() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &chat_chunk("one"));
        std::thread::sleep(Duration::from_secs(3));
        sse_data(c, &chat_chunk("two"));
    });
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let r = stream(&ep(Kind::Chat, &m), &req(), &cancel, |_| {
        cancel.store(true, Ordering::Relaxed)
    });
    assert_eq!(r, Err(Error::Cancelled));
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn first_token_timeout() {
    let m = Mock::start(|_, _c| std::thread::sleep(Duration::from_secs(3)));
    let mut r = req();
    r.first_token_timeout = Duration::from_millis(300);
    let t = Instant::now();
    let (res, _) = run(&ep(Kind::Chat, &m), &r);
    assert_eq!(res, Err(Error::Timeout));
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn connection_refused_is_connect_error() {
    let m = Mock::start(|_, _| {});
    let e = Endpoint {
        url: "http://127.0.0.1:1/v1".into(),
        ..ep(Kind::Chat, &m)
    };
    assert!(matches!(run(&e, &req()).0, Err(Error::Connect(_))));
}

#[test]
fn list_models_parses_ids() {
    let m = Mock::start(|r, c| {
        assert_eq!(r.path, "/v1/models");
        json(c, 200, r#"{"data":[{"id":"a"},{"id":"b:7b"}]}"#)
    });
    let v = list_models(&m.url(), &None, Duration::from_secs(1)).unwrap();
    assert_eq!(v, ["a", "b:7b"]);
}

#[test]
fn classic_chat_params_without_effort() {
    let m = Mock::start(|_, c| chat_stream(c, &["x"], Duration::ZERO));
    let e = Endpoint {
        effort: None,
        ..ep(Kind::Chat, &m)
    };
    let mut r = req();
    r.temperature = Some(0.0);
    r.stop = vec!["\n".into()];
    run(&e, &r).0.unwrap();
    let b: serde_json::Value = serde_json::from_str(&m.requests()[0].body).unwrap();
    assert_eq!(b["max_tokens"], 99);
    assert_eq!(b["temperature"], 0.0);
    assert_eq!(b["stop"][0], "\n");
    assert!(b.get("max_completion_tokens").is_none() && b.get("reasoning_effort").is_none());
}

#[test]
fn eof_without_terminal_event_is_an_error() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &chat_chunk("```bash\nrm -rf /tm"));
    });
    let (r, s) = run(&ep(Kind::Chat, &m), &req());
    assert!(matches!(r, Err(Error::Api(_))), "{r:?}");
    assert!(s.contains("rm -rf"));
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &resp_delta("x"));
    });
    assert!(run(&ep(Kind::Responses, &m), &req()).0.is_err());
}

#[test]
fn finish_reason_ends_stream_and_length_is_truncated() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &chat_chunk("a"));
        sse_data(c, r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#);
    });
    let (r, s) = run(&ep(Kind::Chat, &m), &req());
    assert_eq!(s, "a");
    assert!(r.unwrap().truncated);
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &chat_chunk("a"));
        sse_data(c, r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
    });
    assert!(!run(&ep(Kind::Chat, &m), &req()).0.unwrap().truncated);
}

#[test]
fn incomplete_responses_is_soft_end_and_refusal_is_text() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, r#"{"type":"response.refusal.delta","delta":"no"}"#);
        sse_data(
            c,
            r#"{"type":"response.incomplete","response":{"usage":{"output_tokens":9}}}"#,
        );
    });
    let (r, s) = run(&ep(Kind::Responses, &m), &req());
    assert_eq!(s, "no");
    assert!(r.unwrap().truncated);
}

#[test]
fn quiet_server_longer_than_read_timeout_does_not_break_the_stream() {
    let m = Mock::start(|_, c| {
        sse_head(c);
        sse_data(c, &chat_chunk("one "));
        std::thread::sleep(Duration::from_millis(700));
        sse_data(c, &chat_chunk("two"));
        sse_data(c, "[DONE]");
    });
    let mut r = req();
    r.first_token_timeout = Duration::from_millis(250); // socket read timeout
    let (res, s) = run(&ep(Kind::Chat, &m), &r);
    assert_eq!(s, "one two");
    res.unwrap();
}
