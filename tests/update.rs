//! `aish update` against a mock release API, and the quiet update notice.

use std::process::Command;
use std::time::Duration;

use aish_llm::mock::{self, Mock};
use expectrl::Session;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const NEW: &[u8] = b"#!/bin/sh\necho new-aish\n";

/// A tarball like the release one, with `aish` inside `aish-<ver>-<arch>-linux/`.
fn tarball(ver: &str, body: &[u8]) -> Vec<u8> {
    let d = tempfile::tempdir().unwrap();
    let dir = format!("aish-{ver}-{}-linux", std::env::consts::ARCH);
    std::fs::create_dir(d.path().join(&dir)).unwrap();
    std::fs::write(d.path().join(&dir).join("aish"), body).unwrap();
    let out = Command::new("tar")
        .arg("-czC")
        .arg(d.path())
        .arg(&dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

fn hex(d: &[u8]) -> String {
    Sha256::digest(d)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Serves release `ver`; `sum` is what SHA256SUMS claims for the tarball.
fn release(ver: &'static str, sum: Option<String>) -> Mock {
    let tgz = tarball(ver, NEW);
    let name = format!("aish-{ver}-{}-linux.tar.gz", std::env::consts::ARCH);
    let sums = format!("{}  {name}\n", sum.unwrap_or_else(|| hex(&tgz)));
    Mock::start(move |r, c| {
        let port = c.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        match r.path.as_str() {
            "/latest" => {
                let j = serde_json::json!({"tag_name": format!("v{ver}"), "assets": [
                    {"name": name, "browser_download_url": format!("{base}/dl/{name}")},
                    {"name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/SHA256SUMS")},
                ]});
                mock::json(c, 200, &j.to_string());
            }
            p if p.ends_with("SHA256SUMS") => raw(c, sums.as_bytes()),
            p if p.ends_with(".tar.gz") => raw(c, &tgz),
            _ => mock::json(c, 404, "{}"),
        }
    })
}

fn raw(c: &mut std::net::TcpStream, body: &[u8]) {
    use std::io::Write;
    let _ = write!(
        c,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = c.write_all(body);
}

/// A private copy of the binary so the test can replace it.
fn copy_exe(d: &TempDir) -> std::path::PathBuf {
    let p = d.path().join("aish");
    std::fs::copy(env!("CARGO_BIN_EXE_aish"), &p).unwrap();
    p
}

fn update(exe: &std::path::Path, m: &Mock, args: &[&str]) -> (bool, String, String) {
    let o = Command::new(exe)
        .arg("update")
        .args(args)
        .env(
            "AISH_TEST_UPDATE_URL",
            format!("http://127.0.0.1:{}/latest", m.port),
        )
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

#[test]
fn replaces_the_binary() {
    let d = tempfile::tempdir().unwrap();
    let exe = copy_exe(&d);
    let m = release("99.0.0", None);
    let (ok, out, err) = update(&exe, &m, &["--check"]);
    assert!(ok, "{err}");
    assert!(out.contains("→ 99.0.0"), "{out}");
    assert_ne!(
        std::fs::read(&exe).unwrap(),
        NEW,
        "--check must not install"
    );
    let (ok, out, err) = update(&exe, &m, &[]);
    assert!(ok, "{err}");
    assert_eq!(
        out,
        format!("aish {} → 99.0.0\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(std::fs::read(&exe).unwrap(), NEW);
}

#[test]
fn refuses_a_bad_checksum() {
    let d = tempfile::tempdir().unwrap();
    let exe = copy_exe(&d);
    let before = std::fs::read(&exe).unwrap();
    let m = release("99.0.0", Some("0".repeat(64)));
    let (ok, _, err) = update(&exe, &m, &[]);
    assert!(!ok);
    assert!(err.contains("mismatch"), "{err}");
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[test]
fn latest_version_is_reported() {
    let d = tempfile::tempdir().unwrap();
    let exe = copy_exe(&d);
    let m = release(env!("CARGO_PKG_VERSION"), None);
    let (ok, out, _) = update(&exe, &m, &[]);
    assert!(ok);
    assert_eq!(
        out,
        format!("aish {} — последняя версия\n", env!("CARGO_PKG_VERSION"))
    );
}

fn session(home: &TempDir, m: &Mock, extra_cfg: &str) -> Session {
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    std::fs::write(home.path().join("hist"), "").unwrap();
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("providers:\n  - id: p\n    url: http://127.0.0.1:9/v1\n    type: chat\n    models: {{m: {{id: t}}}}\nroles:\n  chat: {{id: p/m}}\n{extra_cfg}"),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env(
            "AISH_TEST_UPDATE_URL",
            format!("http://127.0.0.1:{}/latest", m.port),
        );
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

#[test]
fn notice_once_a_day() {
    let home = tempfile::tempdir().unwrap();
    let m = release("99.0.0", None);
    let mut p = session(&home, &m, "");
    p.expect("↑ aish 99.0.0 — aish update").unwrap();
    let c = std::fs::read_to_string(home.path().join("state/aish/update-check")).unwrap();
    assert!(c.contains("99.0.0"), "{c}");
    drop(p);
    // second start the same day: no new request, no notice
    let n = m.requests().len();
    let mut p = session(&home, &m, "");
    p.expect("E> ").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(m.requests().len(), n);
    p.set_expect_timeout(Some(Duration::from_millis(500)));
    assert!(p.expect("↑ aish").is_err());
}

#[test]
fn notice_can_be_disabled() {
    let home = tempfile::tempdir().unwrap();
    let m = release("99.0.0", None);
    let mut p = session(&home, &m, "update_check: off\n");
    p.expect("E> ").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(m.requests().is_empty());
    p.set_expect_timeout(Some(Duration::from_millis(500)));
    assert!(p.expect("↑ aish").is_err());
}
