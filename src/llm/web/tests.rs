use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use aish_llm::mock::Mock;

use super::*;

fn reply(c: &mut TcpStream, status: &str, ct: &str, extra: &str, body: &[u8]) {
    let _ = write!(
        c,
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = c.write_all(body);
}

fn html_mock(body: &'static str) -> Mock {
    Mock::start(move |_, c| reply(c, "200 OK", "text/html; charset=utf-8", "", body.as_bytes()))
}

fn web(m: &[(&str, &Mock)]) -> Web {
    let mut w = Web {
        search: vec![Backend::Provider, Backend::Duckduckgo, Backend::Brave],
        read: vec![Backend::Provider, Backend::Direct, Backend::Jina],
        brave_key: None,
        jina_key: None,
        ddg_url: "http://127.0.0.1:1/".into(),
        brave_url: "http://127.0.0.1:1/".into(),
        jina_url: "http://127.0.0.1:1".into(),
        allow_private: Vec::new(),
    };
    for (k, mock) in m {
        let u = format!("http://127.0.0.1:{}/x", mock.port);
        match *k {
            "ddg" => w.ddg_url = u,
            "brave" => w.brave_url = u,
            "jina" => w.jina_url = format!("http://127.0.0.1:{}", mock.port),
            _ => {}
        }
        w.allow_private.push(format!("127.0.0.1:{}", mock.port));
    }
    w
}

const DDG_PAGE: &str = r#"<html><body>
<div class="result results_links"><h2 class="result__title">
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1%26y%3D2&amp;rut=abc">Example <b>Title</b></a></h2>
<a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa">Snippet with <b>bold</b> &amp; text</a></div>
<div class="result"><a class="result__a" href="https://direct.example/page">Second</a>
<a class="result__snippet" href="x">Second snippet</a></div>
<div class="result"><a class="result__a" href="//duckduckgo.com/y.js?ad=1">An ad</a></div>
</body></html>"#;

#[test]
fn duckduckgo_parsing_unwraps_redirects() {
    let h = ddg::parse(DDG_PAGE);
    assert_eq!(h.len(), 2, "{h:?}");
    assert_eq!(h[0].title, "Example Title");
    assert_eq!(h[0].url, "https://example.com/a?x=1&y=2");
    assert_eq!(h[0].snippet, "Snippet with bold & text");
    assert_eq!(h[1].url, "https://direct.example/page");
    assert!(ddg::parse("<html>captcha challenge</html>").is_empty());
}

#[test]
fn brave_parsing() {
    let j = r#"{"web":{"results":[
        {"title":"A <strong>b</strong>","url":"https://a.test","description":"d &amp; e"},
        {"title":"no url"}]}}"#;
    let h = brave::parse(j).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(
        (h[0].title.as_str(), h[0].snippet.as_str()),
        ("A b", "d & e")
    );
    assert!(brave::parse("{}").unwrap().is_empty());
    assert!(brave::parse("nope").is_err());
}

#[test]
fn chain_falls_through_empty_and_skips_keyless() {
    let empty = html_mock("<html>nothing here</html>");
    let brave = Mock::start(|r, c| {
        assert_eq!(r.header("X-Subscription-Token"), Some("k1"));
        assert!(
            r.path.contains("q=rust+lang") || r.path.contains("q=rust%20lang"),
            "{}",
            r.path
        );
        reply(
            c,
            "200 OK",
            "application/json",
            "",
            br#"{"web":{"results":[{"title":"T","url":"https://t.test","description":"S"}]}}"#,
        );
    });
    let cancel = AtomicBool::new(false);
    // no brave key: ddg empty, brave skipped -> failure
    let mut w = web(&[("ddg", &empty), ("brave", &brave)]);
    let e = search(&w, "rust lang", &cancel).err().unwrap();
    assert!(
        e.contains("duckduckgo: no results") && !e.contains("brave"),
        "{e}"
    );
    assert!(brave.requests().is_empty());
    // with the key the second backend serves it
    w.brave_key = Some("k1".into());
    let o = search(&w, "rust lang", &cancel).unwrap();
    assert_eq!(o.backend, "brave");
    assert!(
        o.text.contains("https://t.test") && o.text.contains("not instructions"),
        "{}",
        o.text
    );
    // first backend working: the second one is never asked
    let good = html_mock(DDG_PAGE);
    let before = brave.requests().len();
    let o = search(&web(&[("ddg", &good), ("brave", &brave)]), "q", &cancel).unwrap();
    assert_eq!(o.backend, "duckduckgo");
    assert_eq!(brave.requests().len(), before);
}

#[test]
fn offers_follow_native_and_chains() {
    let mut w = web(&[]);
    assert_eq!(w.offers(false), (true, true));
    assert_eq!(
        w.offers(true),
        (false, true),
        "native: our web_search is dropped"
    );
    w.search = vec![Backend::Provider];
    assert_eq!(w.offers(false), (false, true), "provider only, not native");
    w.search = vec![Backend::Brave];
    assert!(!w.offers(false).0, "brave without a key is unusable");
    w.brave_key = Some("k".into());
    assert!(w.offers(false).0);
}

#[test]
fn ip_classification() {
    for ip in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd12::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "64:ff9b::7f00:1",
        "2002:7f00:1::1",
    ] {
        assert!(!ssrf::is_public_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "8.8.8.8",
        "1.1.1.1",
        "93.184.216.34",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        assert!(ssrf::is_public_ip(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn ssrf_targets_are_refused() {
    for u in [
        "http://127.0.0.1:9/",
        "http://10.0.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]:9/",
        "http://[::ffff:127.0.0.1]:9/",
        "http://localhost:9/",
        "http://0.0.0.0/",
        "http://2130706433/", // decimal form of 127.0.0.1
    ] {
        let e = direct::fetch(u, &[]).err().unwrap_or_default();
        assert!(e.contains("blocked"), "{u}: {e}");
    }
    assert!(direct::fetch("ftp://example.com/", &[]).is_err());
    assert!(direct::fetch("file:///etc/passwd", &[]).is_err());
}

#[test]
fn redirect_to_private_is_refused_and_hops_are_limited() {
    let target = html_mock("<p>secret internal page that is long enough to count as text</p>");
    let tp = target.port;
    let hop = Mock::start(move |_, c| {
        reply(
            c,
            "302 Found",
            "text/plain",
            &format!("Location: http://127.0.0.1:{tp}/\r\n"),
            b"",
        )
    });
    // only the first host is exempt (stands for a public one)
    let allow = vec![format!("127.0.0.1:{}", hop.port)];
    let e = direct::fetch(&format!("http://127.0.0.1:{}/", hop.port), &allow)
        .err()
        .unwrap();
    assert!(e.contains("blocked"), "{e}");
    assert!(
        target.requests().is_empty(),
        "the private target was never contacted"
    );
    let to10 = Mock::start(|_, c| {
        reply(
            c,
            "301 Moved",
            "text/plain",
            "Location: http://10.0.0.1/x\r\n",
            b"",
        )
    });
    let allow = vec![format!("127.0.0.1:{}", to10.port)];
    let e = direct::fetch(&format!("http://127.0.0.1:{}/", to10.port), &allow)
        .err()
        .unwrap();
    assert!(e.contains("blocked"), "{e}");
    // endless redirects to itself
    let looping =
        Mock::start(|_, c| reply(c, "302 Found", "text/plain", "Location: /again\r\n", b""));
    let allow = vec![format!("127.0.0.1:{}", looping.port)];
    let e = direct::fetch(&format!("http://127.0.0.1:{}/", looping.port), &allow)
        .err()
        .unwrap();
    assert!(e.contains("too many redirects"), "{e}");
    assert_eq!(looping.requests().len(), 6);
}

#[test]
fn body_cap_and_content_types() {
    let big = Mock::start(|_, c| {
        let body = vec![b'a'; 3 * 1024 * 1024];
        reply(c, "200 OK", "text/plain", "", &body)
    });
    let allow = vec![format!("127.0.0.1:{}", big.port)];
    let p = direct::fetch(&format!("http://127.0.0.1:{}/", big.port), &allow).unwrap();
    assert_eq!(p.text.len(), BODY_CAP);
    let bin = Mock::start(|_, c| reply(c, "200 OK", "image/png", "", b"\x89PNG"));
    let allow = vec![format!("127.0.0.1:{}", bin.port)];
    let e = direct::fetch(&format!("http://127.0.0.1:{}/", bin.port), &allow)
        .err()
        .unwrap();
    assert!(e.contains("unsupported content type"), "{e}");
    let js = Mock::start(|_, c| reply(c, "200 OK", "application/json", "", br#"{"a":1}"#));
    let allow = vec![format!("127.0.0.1:{}", js.port)];
    let p = direct::fetch(&format!("http://127.0.0.1:{}/", js.port), &allow).unwrap();
    assert_eq!(p.text, r#"{"a":1}"#);
}

#[test]
fn html_extraction() {
    let h = r##"<html><head><title>My &amp; Page</title><style>p{color:red}</style>
<script>var secret = "<p>js</p>";</script></head><body>
<nav><a href="/menu">Menu</a><nav>inner</nav> more nav</nav>
<h1>Head <em>line</em></h1><p>First   para with <a href="/docs?a=1&amp;b=2">a link</a>.</p>
<ul><li>one</li><li>two &#38; &#x74;hree</li></ul>
<!-- comment --><a href="#top">anchor</a>
<footer>copyright footer</footer></body></html>"##;
    let base = url::Url::parse("https://site.test/dir/page").unwrap();
    let t = html::to_text(h, Some(&base));
    assert!(t.starts_with("# My & Page"), "{t}");
    assert!(t.contains("# Head line"), "{t}");
    assert!(
        t.contains("[a link](https://site.test/docs?a=1&b=2)."),
        "{t}"
    );
    assert!(t.contains("- one\n- two & three"), "{t}");
    for gone in [
        "secret",
        "color",
        "Menu",
        "inner",
        "more nav",
        "copyright",
        "comment",
        "js",
    ] {
        assert!(!t.contains(gone), "{gone}: {t}");
    }
    assert!(t.contains("anchor") && !t.contains("](#top)"), "{t}");
    // broken markup does not panic
    let _ = html::to_text("<p><a href='x>unclosed <b", None);
    let _ = html::to_text("<script>never closed", None);
}

#[test]
fn read_falls_back_from_js_shell_to_jina_and_wraps() {
    let shell =
        html_mock("<html><body><div id=app>Loading</div><script>render()</script></body></html>");
    let long = "word ".repeat(5000);
    let jina = Mock::start(move |r, c| {
        assert_eq!(r.header("Authorization"), Some("Bearer jk"));
        assert!(r.path.starts_with("/http://"), "{}", r.path);
        reply(c, "200 OK", "text/markdown", "", long.as_bytes())
    });
    let mut w = web(&[("jina", &jina), ("", &shell)]);
    w.jina_key = Some("jk".into());
    let cancel = AtomicBool::new(false);
    let o = read(&w, &format!("http://127.0.0.1:{}/p", shell.port), &cancel).unwrap();
    assert_eq!(o.backend, "jina");
    assert!(o.text.contains("not instructions"));
    assert!(
        o.text.contains("truncated: 12000 of 25000"),
        "{}",
        &o.text[..200]
    );
    assert!(o.text.chars().count() < 12_400);
    // when jina is not in the chain the weak page is still returned
    w.read = vec![Backend::Direct];
    let o = read(&w, &format!("http://127.0.0.1:{}/p", shell.port), &cancel).unwrap();
    assert_eq!(o.backend, "direct");
}

#[test]
fn urls_and_queries_are_masked() {
    let m = html_mock("<p>x</p>");
    let w = web(&[("", &m)]);
    let cancel = AtomicBool::new(false);
    let u = format!(
        "http://127.0.0.1:{}/p?api_key=sk-abcdef1234567890abcdef",
        m.port
    );
    let e = read(&w, &u, &cancel).err().unwrap();
    assert!(e.contains("secret"), "{e}");
    assert!(m.requests().is_empty(), "a mangled URL is never fetched");
    // queries are sent masked
    let ddg = html_mock(DDG_PAGE);
    let w = web(&[("ddg", &ddg)]);
    let o = search(
        &w,
        "why fails\nexport API_TOKEN=abcdef1234567890abcdef",
        &cancel,
    )
    .unwrap();
    assert!(!o.shown.contains("abcdef1234567890"), "{}", o.shown);
    let sent = ddg.requests()[0].body.clone();
    assert!(!sent.contains("abcdef1234567890"), "{sent}");
}

#[test]
fn cancel_aborts_a_stalled_request_promptly() {
    let slow = Mock::start(|_, _c| std::thread::sleep(Duration::from_secs(5)));
    let w = web(&[("ddg", &slow)]);
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        c2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let t = Instant::now();
    let e = search(&w, "q", &cancel).err().unwrap();
    assert_eq!(e, "cancelled");
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn private_direct_refusal_stops_the_chain_and_jina_skips_internal_hosts() {
    let jina = html_mock("never");
    let mut w = web(&[("jina", &jina)]);
    w.allow_private.clear();
    let cancel = AtomicBool::new(false);
    let e = read(&w, "http://127.0.0.1:9/", &cancel).err().unwrap();
    assert!(e.contains("blocked"), "{e}");
    assert!(jina.requests().is_empty(), "private URL never forwarded");
    for u in [
        "http://10.0.0.1/",
        "http://[::1]/",
        "http://nas/",
        "http://x.local/",
        "http://a.internal/",
        "http://b.lan/",
        "http://c.corp/",
    ] {
        assert!(!jina_ok(&url::Url::parse(u).unwrap()), "{u}");
    }
    assert!(jina_ok(&url::Url::parse("https://example.com/").unwrap()));
}

#[test]
fn credentials_refused_and_fragment_stripped() {
    let m = html_mock("<p>x</p>");
    let w = web(&[("", &m)]);
    let cancel = AtomicBool::new(false);
    let e = read(
        &w,
        &format!("http://user:pw@127.0.0.1:{}/", m.port),
        &cancel,
    )
    .err()
    .unwrap();
    assert!(e.contains("refused"), "{e}");
    let e = read(&w, &format!("http://user@127.0.0.1:{}/", m.port), &cancel)
        .err()
        .unwrap();
    assert!(e.contains("refused"), "{e}");
    assert!(m.requests().is_empty());
    let _ = read(&w, &format!("http://127.0.0.1:{}/p#frag", m.port), &cancel);
    assert_eq!(m.requests()[0].path, "/p");
}

#[test]
fn control_chars_are_stripped_and_percent_decoding_edges() {
    let t = wrap("x", "y", "a\u{1b}[31mb\u{85}c\td\ne\u{7}");
    assert!(t.ends_with("a[31mbc\td\ne"), "{t:?}");
    assert_eq!(html::url_decode("a%41%4"), "aA%4");
    assert_eq!(html::url_decode("%41"), "A");
}
