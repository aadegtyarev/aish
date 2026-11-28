//! `aish update`: replace a tarball/install.sh binary with the latest GitHub
//! release (sha256-checked), plus the quiet once-a-day "new version" notice.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::Setting;
use crate::llm::Llm;

const API: &str = "https://api.github.com/repos/aadegtyarev/aish/releases/latest";
const APT_LINE: &str = "sudo apt update && sudo apt install --only-upgrade aish";
const DAY: u64 = 24 * 60 * 60;
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

/// Debug builds only (like the AISH_TEST_WEB_* hooks): a mock release API.
/// Tests that need it skip themselves in release mode.
#[cfg(any(debug_assertions, test))]
fn api_url() -> String {
    std::env::var("AISH_TEST_UPDATE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| API.into())
}

#[cfg(not(any(debug_assertions, test)))]
fn api_url() -> String {
    API.into()
}

/// Proxy from the environment (ureq does not read it itself), honouring `no_proxy`.
fn proxy_for(url: &str) -> Option<ureq::Proxy> {
    let get = |k: &str| {
        std::env::var(k)
            .or_else(|_| std::env::var(k.to_ascii_uppercase()))
            .ok()
            .filter(|v| !v.is_empty())
    };
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', ':']).next().unwrap_or("");
    let skip = get("no_proxy").is_some_and(|l| {
        l.split(',').map(str::trim).any(|e| {
            if let Some((net, bits)) = e.split_once('/') {
                return in_cidr(host, net, bits);
            }
            let e = e.trim_start_matches('.');
            e == "*" || (!e.is_empty() && (host == e || host.ends_with(&format!(".{e}"))))
        })
    });
    if skip {
        return None;
    }
    let p = if scheme == "https" {
        get("https_proxy")
    } else {
        get("http_proxy")
    };
    ureq::Proxy::new(p.or_else(|| get("all_proxy"))?).ok()
}

/// IPv4 `host` inside `net/bits` (anything else: no match).
fn in_cidr(host: &str, net: &str, bits: &str) -> bool {
    use std::net::Ipv4Addr;
    let (Ok(h), Ok(n), Ok(b)) = (
        host.parse::<Ipv4Addr>(),
        net.parse::<Ipv4Addr>(),
        bits.parse::<u32>(),
    ) else {
        return false;
    };
    let mask = u32::MAX.checked_shl(32 - b.min(32)).unwrap_or(0);
    u32::from(h) & mask == u32::from(n) & mask
}

fn agent(url: &str) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .user_agent("aish-update");
    if let Some(p) = proxy_for(url) {
        b = b.proxy(p);
    }
    b.build()
}

type Version = ((u64, u64, u64), Option<String>);

/// `0.2.0`, `v0.2.0`, `0.2.0-rc1` -> ((0, 2, 0), pre-release).
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches('v');
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p.to_owned())),
        None => (s, None),
    };
    let mut p = core.split('.');
    let v = (
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    );
    p.next().is_none().then_some((v, pre))
}

/// Semver precedence of pre-release tags: numeric ids < alphanumeric, a
/// shorter prefix is lower.
fn pre_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (mut x, mut y) = (a.split('.'), b.split('.'));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Equal,
            (None, _) => return Less,
            (_, None) => return Greater,
            (Some(p), Some(q)) => {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), _) => Less,
                    (_, Ok(_)) => Greater,
                    _ => p.cmp(q),
                };
                if o != Equal {
                    return o;
                }
            }
        }
    }
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    let (Some((lc, lp)), Some((cc, cp))) = (parse_version(latest), parse_version(current)) else {
        return false;
    };
    if lc != cc {
        return lc > cc;
    }
    match (lp, cp) {
        (None, Some(_)) => true,
        (Some(l), Some(c)) => pre_cmp(&l, &c) == std::cmp::Ordering::Greater,
        _ => false,
    }
}

struct Release {
    version: String,
    /// (asset name, download url)
    assets: Vec<(String, String)>,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, u)| u.as_str())
    }
}

fn parse_release(body: &str) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad reply: {e}"))?;
    let tag = v["tag_name"].as_str().ok_or("no tag_name in the reply")?;
    let version = tag.trim_start_matches('v').to_owned();
    if parse_version(&version).is_none() {
        return Err(format!("odd version: {tag}"));
    }
    let assets = v["assets"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x["name"].as_str()?.to_owned(),
                        x["browser_download_url"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { version, assets })
}

fn get(url: &str) -> Result<Vec<u8>, String> {
    let r = agent(url)
        .get(url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(c, _) => format!("{url}: http {c}"),
            ureq::Error::Transport(t) => format!("{url}: {}", t.kind()),
        })?;
    let mut buf = Vec::new();
    r.into_reader()
        .take(MAX_DOWNLOAD)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{url}: {e}"))?;
    Ok(buf)
}

fn latest() -> Result<Release, String> {
    let body = get(&api_url())?;
    parse_release(&String::from_utf8_lossy(&body))
}

/// `<hex>  <name>` lines of SHA256SUMS: the hash for `name`.
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, n) = l.split_once(char::is_whitespace)?;
        (n.trim().trim_start_matches('*') == name).then(|| h.to_ascii_lowercase())
    })
}

fn verify(data: &[u8], want: &str) -> Result<(), String> {
    let got: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got == want.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(format!("sha256 mismatch (want {want}, got {got})"))
    }
}

/// Pull one member out of a .tar.gz held in memory (uses the system `tar`).
fn extract(tgz: &[u8], member: &str) -> Result<Vec<u8>, String> {
    use std::process::{Command, Stdio};
    let mut ch = Command::new("tar")
        .args(["-xzOf", "-", member])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("tar: {e}"))?;
    let mut stdin = ch.stdin.take().ok_or("tar: no stdin")?;
    let data = tgz.to_vec();
    let w = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = ch.wait_with_output().map_err(|e| format!("tar: {e}"))?;
    let _ = w.join();
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!("{member} not found in the archive"));
    }
    Ok(out.stdout)
}

/// The new binary as a closed 0755 temp file next to `exe` (removed on drop).
fn stage(exe: &Path, bytes: &[u8]) -> Result<tempfile::TempPath, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = exe.parent().unwrap_or(Path::new("."));
    let err = |e: std::io::Error| format!("{}: {e}", dir.display());
    let mut f = tempfile::Builder::new()
        .prefix(".aish.new.")
        .permissions(std::fs::Permissions::from_mode(0o755))
        .tempfile_in(dir)
        .map_err(err)?;
    f.write_all(bytes)
        .and_then(|()| f.as_file().sync_all())
        .map_err(err)?;
    Ok(f.into_temp_path())
}

fn check_version(bin: &Path, want: &str) -> Result<(), String> {
    let run = || {
        std::process::Command::new(bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
    };
    // ETXTBSY: a fork in another thread may briefly hold the just-written file open
    let mut out = run();
    for _ in 0..50 {
        match &out {
            Err(e) if e.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32) => {
                std::thread::sleep(Duration::from_millis(10));
                out = run();
            }
            _ => break,
        }
    }
    let out = out.map_err(|e| format!("new binary does not run: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    if first == format!("aish {want}") {
        Ok(())
    } else {
        Err(format!(
            "new binary reports '{first}', expected 'aish {want}'"
        ))
    }
}

/// Stage next to `exe`, check the version, then rename over it: atomic, and a
/// running process keeps its old inode.
fn replace_exe(exe: &Path, bytes: &[u8], want: &str) -> Result<(), String> {
    let tmp = stage(exe, bytes)?;
    check_version(&tmp, want)?;
    tmp.persist(exe)
        .map_err(|e| format!("{}: {}", exe.display(), e.error))
}

/// Only replace a file we own (and, as root, only root's own).
fn check_owner(file_uid: u32, my_uid: u32) -> Result<(), String> {
    if file_uid == my_uid {
        Ok(())
    } else if my_uid == 0 {
        Err("refusing to replace a file owned by another user as root".into())
    } else {
        Err("the binary is owned by another user (update it as that user)".into())
    }
}

/// A symlink named on the command line / PATH is not replaced blindly.
fn symlink_hint(argv0: &std::ffi::OsStr) -> Option<String> {
    let p = Path::new(argv0);
    let found = if argv0.as_encoded_bytes().contains(&b'/') {
        p.to_path_buf()
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(p))
            .find(|c| c.is_file())?
    };
    let t = std::fs::read_link(&found).ok()?;
    Some(format!(
        "{} is a symlink to {}; update the target or reinstall with install.sh",
        found.display(),
        t.display()
    ))
}

/// Download, verify and install `rel` over `exe`.
fn install(rel: &Release, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let me = std::fs::metadata("/proc/self")
        .map_err(|e| e.to_string())?
        .uid();
    let owner = std::fs::metadata(exe)
        .map_err(|e| format!("{}: {e}", exe.display()))?
        .uid();
    check_owner(owner, me)?;
    let arch = std::env::consts::ARCH;
    let dir = format!("aish-{}-{arch}-linux", rel.version);
    let tar = format!("{dir}.tar.gz");
    let (Some(tar_url), Some(sums_url)) = (rel.asset(&tar), rel.asset("SHA256SUMS")) else {
        return Err(format!("the release has no {tar}"));
    };
    let sums = String::from_utf8_lossy(&get(sums_url)?).into_owned();
    let want = sum_for(&sums, &tar).ok_or_else(|| format!("no checksum for {tar}"))?;
    let data = get(tar_url)?;
    verify(&data, &want)?;
    let bin = extract(&data, &format!("{dir}/aish"))?;
    replace_exe(exe, &bin, &rel.version)
}

fn dpkg_owned(exe: &Path) -> bool {
    use std::process::{Command, Stdio};
    exe == Path::new("/usr/bin/aish")
        || Command::new("dpkg")
            .arg("-S")
            .arg(exe)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// `aish update [--check]`; returns the exit code.
pub fn run(args: &[std::ffi::OsString]) -> i32 {
    let check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => {
            eprintln!("usage: aish update [--check]");
            return 2;
        }
    };
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
        eprintln!("aish: cannot find own path");
        return 1;
    };
    if dpkg_owned(&exe) {
        println!("{APT_LINE}");
        return 0;
    }
    if let Some(h) = std::env::args_os().next().and_then(|a| symlink_hint(&a)) {
        eprintln!("aish: {h}");
        return 1;
    }
    let cur = env!("CARGO_PKG_VERSION");
    let rel = match latest() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("aish: {e}");
            return 1;
        }
    };
    if !is_newer(&rel.version, cur) {
        println!("aish {cur} — up to date");
        return 0;
    }
    if check {
        println!("aish {cur} → {} · aish update", rel.version);
        return 0;
    }
    match install(&rel, &exe) {
        Ok(()) => {
            println!("aish {cur} → {}", rel.version);
            0
        }
        Err(e) => {
            eprintln!("aish: update failed: {e}");
            1
        }
    }
}

/// `<checked ts>\n<latest version>\n<notified ts>\n`
#[derive(Default, Debug, PartialEq)]
struct Cache {
    checked: u64,
    latest: String,
    notified: u64,
}

impl Cache {
    fn parse(s: &str) -> Cache {
        let l: Vec<&str> = s.lines().map(str::trim).collect();
        let num = |i: usize| l.get(i).and_then(|x| x.parse().ok()).unwrap_or(0);
        Cache {
            checked: num(0),
            latest: l.get(1).copied().unwrap_or("").to_owned(),
            notified: num(2),
        }
    }

    fn render(&self) -> String {
        format!("{}\n{}\n{}\n", self.checked, self.latest, self.notified)
    }

    fn due(&self, now: u64) -> bool {
        now.saturating_sub(self.checked) >= DAY
    }

    fn should_notify(&self, now: u64, cur: &str) -> bool {
        is_newer(&self.latest, cur) && now.saturating_sub(self.notified) >= DAY
    }
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("aish/update-check"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Background: refresh the cache at most once a day and queue the one-line
/// notice. Never touches the network on the caller's thread; stays silent
/// without a config (the first-run line owns that moment) or with
/// `update_check: off`.
pub fn spawn_check(llm: Arc<Llm>) {
    if !llm.config_exists() {
        return;
    }
    std::thread::spawn(move || {
        if llm
            .config()
            .is_some_and(|c| matches!(c.update_check, Setting::Off))
        {
            return;
        }
        let Some(path) = cache_path() else { return };
        let mut c = Cache::parse(&std::fs::read_to_string(&path).unwrap_or_default());
        let t = now();
        if c.due(t) {
            if let Ok(r) = latest() {
                c.latest = r.version;
            }
            c.checked = t; // a failed check also waits a day
        }
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&path, c.render());
        if c.should_notify(t, env!("CARGO_PKG_VERSION")) {
            // "notified" is written when the line was shown, not before: a
            // session that ends first leaves the notice for the next one
            let text = format!(" ↑ aish {} — aish update", c.latest);
            c.notified = t;
            llm.set_notice_then(text, move || {
                let _ = std::fs::write(&path, c.render());
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some(((0, 2, 0), None)));
        assert_eq!(parse_version("1.10.3-rc1").unwrap().0, (1, 10, 3));
        assert!(is_newer("0.4.0", "0.4.0-rc1"));
        assert!(!is_newer("0.4.0-rc1", "0.4.0"));
        assert!(is_newer("0.4.0-rc2", "0.4.0-rc1"));
        assert!(is_newer("0.4.0-rc.10", "0.4.0-rc.9"));
        assert!(!is_newer("0.4.0-rc1", "0.4.0-rc1"));
        assert_eq!(parse_version("1.2"), None);
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("junk", "0.2.0"));
    }

    #[test]
    fn release_json() {
        let r = parse_release(
            r#"{"tag_name":"v0.3.1","assets":[{"name":"SHA256SUMS","browser_download_url":"http://x/s"}]}"#,
        )
        .unwrap();
        assert_eq!(r.version, "0.3.1");
        assert_eq!(r.asset("SHA256SUMS"), Some("http://x/s"));
        assert!(parse_release(r#"{"tag_name":"nightly"}"#).is_err());
    }

    #[test]
    fn checksum() {
        let d = b"hello";
        let h = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify(d, h).is_ok());
        assert!(verify(b"hellO", h).unwrap_err().contains("mismatch"));
        let sums = format!("{h}  a.tar.gz\n{} *b.deb\n", "0".repeat(64));
        assert_eq!(sum_for(&sums, "a.tar.gz").as_deref(), Some(h));
        assert!(sum_for(&sums, "b.deb").is_some());
        assert!(sum_for(&sums, "c").is_none());
    }

    fn script(v: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'aish {v}'\n").into_bytes()
    }

    #[test]
    fn replace_checks_version_first() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("aish");
        std::fs::write(&exe, b"old").unwrap();
        let e = replace_exe(&exe, &script("0.9.0"), "1.0.0").unwrap_err();
        assert!(e.contains("expected 'aish 1.0.0'"), "{e}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        replace_exe(&exe, &script("1.0.0"), "1.0.0").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), script("1.0.0"));
        let m = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o755);
        // no temp files left behind, even after the refusal
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn owner_and_symlink_guards() {
        assert!(check_owner(1000, 1000).is_ok());
        assert!(check_owner(0, 1000).is_err());
        assert!(check_owner(1000, 0).unwrap_err().contains("as root"));
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::write(&real, "x").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(symlink_hint(link.as_os_str()).unwrap().contains("symlink"));
        assert!(symlink_hint(real.as_os_str()).is_none());
    }

    #[test]
    fn cache_schedule() {
        let c = Cache::parse("1000\n0.2.0\n500\n");
        assert_eq!(c.latest, "0.2.0");
        assert_eq!(Cache::parse(&c.render()), c);
        assert!(!c.due(1000 + DAY - 1));
        assert!(c.due(1000 + DAY));
        assert!(c.should_notify(500 + DAY, "0.1.0"));
        assert!(!c.should_notify(500 + DAY - 1, "0.1.0"));
        assert!(!c.should_notify(500 + DAY, "0.2.0"));
        assert!(Cache::parse("").due(now()));
    }

    #[test]
    fn cidr() {
        assert!(in_cidr("127.0.0.1", "127.0.0.0", "8"));
        assert!(in_cidr("10.1.2.3", "10.0.0.0", "8"));
        assert!(!in_cidr("11.0.0.1", "10.0.0.0", "8"));
        assert!(!in_cidr("example.com", "10.0.0.0", "8"));
    }
}
