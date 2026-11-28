//! The `aish` command inside a session (status, chat/complete, on/off,
//! config) and the same status as a real CLI subcommand.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Llm, Target};
use crate::config::{Config, Setting};

pub const SUBS: &[&str] = &[
    "status", "chat", "complete", "model", "models", "profile", "on", "off", "config", "init",
    "help",
];

fn is_cmd_line(s: &str, name: &str) -> bool {
    let mut w = s.split_whitespace();
    w.next() == Some(name) && w.next().is_none_or(|x| SUBS.contains(&x) || x == "-h")
}

/// `aish` or `aish <known subcommand> ...`; anything else (`aish -c ...`)
/// is a real command.
pub fn is_aish_line(s: &str) -> bool {
    is_cmd_line(s, "aish")
}

/// The configured short form (`ai` by default): every line that starts with
/// it is ours (an unknown subcommand gets a hint, never bash's "not found").
pub fn is_short_line(s: &str, name: Option<&str>) -> bool {
    name.is_some_and(|n| s.split_whitespace().next() == Some(n))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The candidate `w` most likely meant: a unique prefix, else the nearest by
/// edit distance (at most 2, for words of 3+ letters).
pub fn closest<'a>(w: &str, cands: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let cands: Vec<&str> = cands.into_iter().collect();
    let pre: Vec<&&str> = cands
        .iter()
        .filter(|c| c.starts_with(w) && !w.is_empty())
        .collect();
    if let [one] = pre.as_slice() {
        return Some(**one);
    }
    if w.chars().count() < 3 {
        return None;
    }
    cands
        .iter()
        .map(|c| (edit_distance(w, c), *c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|x| x.1)
}

/// ` · did you mean x?` or ` · have: a, b, c, …` (cut to the width by the caller).
fn did_you_mean(w: &str, cands: &[&str], fallback: &str) -> String {
    match closest(w, cands.iter().copied()) {
        Some(c) => format!("· did you mean {c}?"),
        None if cands.is_empty() => format!("· {fallback}"),
        None => format!(
            "· have: {}, …",
            cands.iter().take(6).copied().collect::<Vec<_>>().join(", ")
        ),
    }
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Status,
    Help,
    Models,
    /// `model <role> <ref>` (also `chat <ref>`, `complete <ref>`, `model <ref>`).
    Model(String, String),
    /// `profile [name]`
    Profile(Option<String>),
    Suggest(bool),
    Config,
    Init,
    /// A first word that is no subcommand: a hint and the help, nothing else.
    Unknown(String),
}

pub fn parse(line: &str) -> Cmd {
    let w: Vec<&str> = line.split_whitespace().collect();
    match (w.get(1).copied(), w.get(2).copied(), w.get(3).copied()) {
        (Some("status"), ..) => Cmd::Status,
        (Some("on"), ..) => Cmd::Suggest(true),
        (Some("off"), ..) => Cmd::Suggest(false),
        (Some("config"), ..) => Cmd::Config,
        (Some("init"), ..) => Cmd::Init,
        (Some("models"), ..) => Cmd::Models,
        (Some(r @ ("chat" | "complete")), Some(m), _) => Cmd::Model(r.into(), m.into()),
        (Some("model"), Some(m), None) => Cmd::Model("chat".into(), m.into()),
        (Some("model"), Some(r), Some(m)) => Cmd::Model(r.into(), m.into()),
        (Some("profile"), n, _) => Cmd::Profile(n.map(Into::into)),
        (None, ..)
        | (Some("help" | "-h" | "--help"), ..)
        | (Some("chat" | "complete" | "model"), None, _) => Cmd::Help,
        (Some(w), ..) => Cmd::Unknown(w.into()),
    }
}

/// One output row: `.0` as is, `.1` dim.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ln(pub String, pub String);

impl Ln {
    pub fn dim(s: impl Into<String>) -> Ln {
        Ln(String::new(), s.into())
    }
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// `head — description`, the description wrapped under the head, indented.
fn help_rows(head: &str, desc: &str, w: usize) -> Vec<Ln> {
    let mut rows = Vec::new();
    let mut cur = format!(" — {}", desc.split(' ').next().unwrap_or(""));
    let mut room = w.saturating_sub(chars(head));
    for word in desc.split(' ').skip(1) {
        if chars(&cur) + 1 + chars(word) > room {
            rows.push(std::mem::take(&mut cur));
            room = w.saturating_sub(2);
            cur = format!("  {word}");
            continue;
        }
        cur.push(' ');
        cur.push_str(word);
    }
    rows.push(cur);
    rows.into_iter()
        .enumerate()
        .map(|(i, r)| {
            if i == 0 {
                Ln(head.into(), r)
            } else {
                Ln::dim(r)
            }
        })
        .collect()
}

const HELP: &[(&str, &str)] = &[
    ("status", "models and link"),
    ("models", "list models"),
    ("model [role] <model>", "switch model and save"),
    ("chat|complete <model>", "same for a role"),
    ("profile [name]", "profiles, switch"),
    ("on | off", "suggestions"),
    ("config", "open the config"),
    ("init", "set up launch"),
    ("help", "this help"),
];

pub fn help_lines(w: usize) -> Vec<Ln> {
    let mut v = vec![Ln::dim("ai or aish + command:")];
    for (h, d) in HELP {
        v.extend(help_rows(h, d, w));
    }
    v
}

/// Models by provider: alias, id (dim, cut with `…`), roles using it.
pub fn models_lines(llm: &Llm, w: usize) -> Vec<Ln> {
    let Some(cfg) = llm.config() else {
        return vec![Ln::dim(format!(
            "no model configured · {} config",
            llm.command_label()
        ))];
    };
    let roles: Vec<(&str, Vec<String>)> = ["chat", "complete"]
        .into_iter()
        .map(|r| (r, llm.labels(r)))
        .collect();
    let mut v = Vec::new();
    if let Some(p) = &cfg.profile {
        v.push(Ln::dim(format!("profile {p}")));
    }
    for p in &cfg.providers {
        v.push(Ln::dim(p.id.clone()));
        let mut al: Vec<(&String, &crate::config::Model)> = p.models.iter().collect();
        al.sort_by(|a, b| a.0.cmp(b.0));
        for (alias, m) in al {
            let label = format!("{}/{alias}", p.id);
            let marks: Vec<String> = roles
                .iter()
                .filter_map(|(r, ls)| {
                    ls.iter().position(|l| *l == label).map(|i| {
                        if i == 0 {
                            (*r).to_owned()
                        } else {
                            format!("{r}·{}", i + 1)
                        }
                    })
                })
                .collect();
            let marks = marks.join(" ");
            let head = format!("  {alias}");
            let mut tail = String::new();
            let mut room = w.saturating_sub(chars(&head));
            if !marks.is_empty() {
                room = room.saturating_sub(chars(&marks) + 1);
            }
            if m.id != *alias && room > 2 {
                let id = if chars(&m.id) + 1 > room {
                    let cut: String = m.id.chars().take(room.saturating_sub(2)).collect();
                    format!(" {cut}…")
                } else {
                    format!(" {}", m.id)
                };
                tail.push_str(&id);
            }
            if !marks.is_empty() {
                tail.push(' ');
                tail.push_str(&marks);
            }
            v.push(Ln(head, tail));
        }
    }
    if cfg.providers.is_empty() {
        v.push(Ln::dim("no models"));
    }
    v
}

fn profile_lines(llm: &Llm, name: Option<&str>) -> Vec<Ln> {
    let Some(cfg) = llm.config() else {
        return vec![Ln::dim("no model configured")];
    };
    let Some(name) = name else {
        if cfg.profiles.0.is_empty() {
            return vec![Ln::dim("no profiles · see config")];
        }
        return cfg
            .profiles
            .0
            .iter()
            .map(|(n, _)| {
                let on = cfg.profile.as_deref() == Some(n);
                Ln(format!("{} {n}", if on { "*" } else { " " }), String::new())
            })
            .collect();
    };
    if cfg.profile_roles(name).is_none() {
        let names: Vec<&str> = cfg.profiles.0.iter().map(|(n, _)| n.as_str()).collect();
        let hint = did_you_mean(name, &names, "no profiles");
        return vec![Ln::dim(format!("no profile: {name} {hint}"))];
    }
    if cfg.profile.as_deref() == Some(name) {
        return vec![Ln::dim(format!("profile {name} already selected"))];
    }
    let ok = persist(
        llm,
        |s| crate::config_edit::set_profile(s, name),
        |c| c.profile.as_deref() == Some(name),
    );
    if ok {
        llm.clear_overrides();
        vec![Ln::dim(format!("profile → {name}"))]
    } else {
        vec![Ln::dim("config unchanged · edit profile: by hand")]
    }
}

/// Apply `edit` to the config file text; written only if the result parses
/// and `check` holds. False: nothing was changed.
fn persist(
    llm: &Llm,
    edit: impl FnOnce(&str) -> Option<String>,
    check: impl FnOnce(&Config) -> bool,
) -> bool {
    let Some(path) = llm.path() else { return false };
    let Ok(src) = std::fs::read_to_string(path) else {
        return false;
    };
    let Some(new) = edit(&src) else { return false };
    match Config::parse(&new) {
        Ok(c) if check(&c) => {}
        _ => return false,
    }
    crate::config_edit::write_atomic(path, &new).is_ok()
}

fn bad_ref(cfg: &Config, r: &str) -> String {
    let mut aliases: Vec<&str> = Vec::new();
    for p in &cfg.providers {
        let mut a: Vec<&str> = p.models.keys().map(String::as_str).collect();
        a.sort_unstable();
        for x in a {
            if !aliases.contains(&x) {
                aliases.push(x);
            }
        }
    }
    match r.split_once('/') {
        Some((p, _)) if cfg.providers.iter().all(|x| x.id != p) => {
            let ps: Vec<&str> = cfg.providers.iter().map(|x| x.id.as_str()).collect();
            format!("no provider: {p} {}", did_you_mean(p, &ps, "no providers"))
        }
        Some((p, a)) => {
            let own: Vec<&str> = cfg
                .providers
                .iter()
                .find(|x| x.id == p)
                .map(|x| x.models.keys().map(String::as_str).collect())
                .unwrap_or_default();
            format!("no model {p}/{a} {}", did_you_mean(a, &own, "no models"))
        }
        None => match cfg.providers_with(r).as_slice() {
            [] => format!("no model: {r} {}", did_you_mean(r, &aliases, "no models")),
            ps => format!("{r}: on {} · use {}/{r}", ps.join(", "), ps[0]),
        },
    }
}

fn set_model(llm: &Llm, role: &str, r: &str) -> Vec<Ln> {
    if !matches!(role, "chat" | "complete") {
        let hint = did_you_mean(role, &["chat", "complete"], "chat | complete");
        return vec![Ln::dim(format!("no role: {role} {hint}"))];
    }
    let Some(cfg) = llm.config() else {
        return vec![Ln::dim(format!(
            "no model configured · {} config",
            llm.command_label()
        ))];
    };
    let Ok(new) = cfg.resolve_ref(r, &Setting::Unset) else {
        return vec![Ln::dim(bad_ref(&cfg, r))];
    };
    let want = format!("{}/{}", new.provider.id, new.alias);
    // the model goes to the front, the rest stay as fallbacks
    let mut refs = vec![r.to_owned()];
    for old in cfg.role(role).map(|x| x.ids.clone()).unwrap_or_default() {
        let same = cfg
            .resolve_ref(&old, &Setting::Unset)
            .is_ok_and(|o| format!("{}/{}", o.provider.id, o.alias) == want);
        if !same {
            refs.push(old);
        }
    }
    let profile = cfg.profile.clone();
    let ok = persist(
        llm,
        |s| crate::config_edit::set_role(s, profile.as_deref(), role, &refs),
        |c| {
            // the whole list must read back as written
            c.role(role).is_some_and(|x| x.ids == refs)
                && c.resolve(role)
                    .is_ok_and(|x| format!("{}/{}", x.provider.id, x.alias) == want)
        },
    );
    let mut v = Vec::new();
    if ok {
        llm.clear_override(role);
    } else {
        llm.set_override(role, r);
        v.push(Ln::dim("session only: config unchanged"));
    }
    let shown = if ok { refs.join(" · ") } else { r.to_owned() };
    v.insert(0, Ln::dim(format!("{role} → {shown}")));
    v
}

/// Output of a non-status command.
pub fn run(llm: &Llm, cmd: &Cmd, width: usize) -> Vec<Ln> {
    match cmd {
        Cmd::Suggest(on) => {
            llm.suggest_on.store(*on, Ordering::Relaxed);
            vec![Ln::dim(format!(
                "suggestions: {}",
                if *on { "on" } else { "off" }
            ))]
        }
        Cmd::Help => help_lines(width),
        Cmd::Models => models_lines(llm, width),
        Cmd::Model(role, r) => set_model(llm, role, r),
        Cmd::Profile(n) => profile_lines(llm, n.as_deref()),
        Cmd::Unknown(w) => {
            let mut v = Vec::new();
            if let Some(c) = closest(w, SUBS.iter().copied()) {
                v.push(Ln::dim(format!("unknown {w} · did you mean {c}?")));
            }
            v.push(Ln::dim("ask the model: ? <question>"));
            v.extend(help_lines(width));
            v
        }
        Cmd::Status | Cmd::Config | Cmd::Init => Vec::new(),
    }
}

/// Text for the input line that opens the config in the user's editor.
pub fn config_command(llm: &Llm) -> String {
    let path = llm
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let shown = match path.strip_prefix(&home) {
        Some(r) if !home.is_empty() && r.starts_with('/') => format!("~{r}"),
        _ => format!("'{path}'"),
    };
    format!("${{EDITOR:-vi}} {shown}")
}

fn probe(t: &Target) -> String {
    let t0 = Instant::now();
    match aish_llm::list_models(&t.ep.url, &t.ep.key, Duration::from_secs(1)) {
        Ok(_) => format!("ok {}ms", t0.elapsed().as_millis()),
        Err(aish_llm::Error::Http { status, .. }) => format!("{status}"),
        Err(_) => "no answer".into(),
    }
}

/// One line per role (probed in parallel; fallbacks listed under it) and the
/// suggestions switch.
pub fn status_lines(llm: &Arc<Llm>) -> Vec<String> {
    let roles = ["chat", "complete"];
    let hs: Vec<_> = roles
        .iter()
        .map(|r| {
            let ts = llm.targets(r);
            std::thread::spawn(move || {
                let first = ts.first()?;
                let line = format!("{} · {} · {}", first.label, first.ep.model, probe(first));
                let rest: Vec<&str> = ts[1..].iter().map(|t| t.label.as_str()).collect();
                Some((line, rest.join(" · ")))
            })
        })
        .collect();
    let mut v = Vec::new();
    for (r, h) in roles.iter().zip(hs) {
        match h.join().ok().flatten() {
            Some((body, rest)) => {
                v.push(format!("{r:<9}{body}"));
                if !rest.is_empty() {
                    v.push(format!("{:<9}↪ {rest}", ""));
                }
            }
            None => v.push(format!("{r:<9}not set")),
        }
    }
    let on = llm.suggest_on.load(Ordering::Relaxed);
    v.push(format!("suggestions: {}", if on { "on" } else { "off" }));
    v
}

/// Real CLI subcommands. Only these exact first arguments are ours; returns
/// the exit code, or None to let bash handle the arguments.
pub fn cli(args: &[std::ffi::OsString]) -> Option<i32> {
    let a = args.first()?.to_str()?;
    match a {
        "--version" if args.len() == 1 => {
            println!("aish {}", env!("CARGO_PKG_VERSION"));
            // Probes that look for "GNU bash" keep working.
            let st = std::process::Command::new("bash").arg("--version").status();
            Some(st.ok().and_then(|s| s.code()).unwrap_or(0))
        }
        "--help" if args.len() == 1 => {
            println!(
                "aish {}: bash with a line editor and LLM help\n\n  aish            start the shell\n  aish status     show models and whether they answer\n  aish models     list models by provider\n  aish model [chat|complete] <model>   set a role's model (saved)\n  aish profile [name]   list or switch profiles\n  aish help       the commands\n  aish config     edit the config (prints the path if not a terminal)\n  aish init       set up: start aish from ~/.bashrc, detect models (--remove undoes)\n  aish update     install the latest release (--check: only look)\n\nOther arguments go to bash.",
                env!("CARGO_PKG_VERSION")
            );
            Some(0)
        }
        // A file of that name in the cwd means bash semantics (`bash status`).
        "status" | "config" | "init" | "update" | "help" | "models" | "model" | "profile"
            if std::path::Path::new(a).exists() =>
        {
            None
        }
        "help" | "models" | "model" | "profile" => {
            let mut line = String::from("aish");
            for a in args {
                line.push(' ');
                line.push_str(a.to_str()?);
            }
            let llm = Llm::new();
            for l in run(&llm, &parse(&line), 80) {
                println!("{}{}", l.0, l.1);
            }
            Some(0)
        }
        "init" => Some(crate::init::run(&args[1..])),
        "update" => Some(crate::update::run(&args[1..])),
        "status" if args.len() == 1 => {
            let llm = Llm::new();
            if llm.config().is_none() {
                println!("no model configured · aish config");
                return Some(1);
            }
            for l in status_lines(&llm) {
                println!("{l}");
            }
            Some(0)
        }
        "config" if args.len() == 1 => {
            let llm = Llm::new();
            let Some(p) = llm.path() else {
                eprintln!("aish: no HOME");
                return Some(1);
            };
            use std::io::IsTerminal;
            if !std::io::stdout().is_terminal() {
                println!("{}", p.display());
                return Some(0);
            }
            let ed = std::env::var("EDITOR").ok().filter(|e| !e.is_empty());
            let ed = ed.unwrap_or_else(|| "vi".into());
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let st = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{ed} \"$1\""))
                .arg("sh")
                .arg(p)
                .status();
            Some(st.ok().and_then(|s| s.code()).unwrap_or(1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_detection() {
        for t in [
            "aish",
            "aish status",
            "aish chat a/b",
            "aish off",
            "aish  config",
        ] {
            assert!(is_aish_line(t), "{t}");
        }
        for t in [
            "llm",
            "llm chat x",
            "aish -c ls",
            "aishx",
            "echo aish",
            "aish foo",
        ] {
            assert!(!is_aish_line(t), "{t}");
        }
        for t in ["ai", "ai chat a/b", "ai  off", "ai config"] {
            assert!(is_short_line(t, Some("ai")), "{t}");
        }
        assert!(is_short_line("ai -h", Some("ai")));
        assert!(is_short_line("ai models", Some("ai")));
        for t in ["aix", "echo ai"] {
            assert!(!is_short_line(t, Some("ai")), "{t}");
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(parse("aish"), Cmd::Help);
        assert_eq!(parse("ai status"), Cmd::Status);
        assert_eq!(parse("ai modls"), Cmd::Unknown("modls".into()));
        assert_eq!(parse("ai тест"), Cmd::Unknown("тест".into()));
        assert_eq!(parse("ai как найти файлы"), Cmd::Unknown("как".into()));
        assert_eq!(
            parse("aish chat a/b"),
            Cmd::Model("chat".into(), "a/b".into())
        );
        assert_eq!(parse("aish chat"), Cmd::Help);
        assert_eq!(parse("ai model x"), Cmd::Model("chat".into(), "x".into()));
        assert_eq!(
            parse("ai model complete x"),
            Cmd::Model("complete".into(), "x".into())
        );
        assert_eq!(parse("ai model"), Cmd::Help);
        assert_eq!(parse("ai -h"), Cmd::Help);
        assert_eq!(parse("ai models"), Cmd::Models);
        assert_eq!(parse("ai profile"), Cmd::Profile(None));
        assert_eq!(parse("ai profile inet"), Cmd::Profile(Some("inet".into())));
        assert_eq!(parse("aish off"), Cmd::Suggest(false));
    }

    #[test]
    fn typos_and_closest() {
        assert!(is_short_line("ai write a poem", Some("ai")));
        assert_eq!(closest("chst", SUBS.iter().copied()), Some("chat"));
        assert_eq!(closest("modls", SUBS.iter().copied()), Some("models"));
        assert_eq!(closest("sta", SUBS.iter().copied()), Some("status"));
        assert_eq!(closest("тест", SUBS.iter().copied()), None);
        assert_eq!(closest("x", SUBS.iter().copied()), None);
        assert_eq!(parse("ai chst тест"), Cmd::Unknown("chst".into()));
    }

    #[test]
    fn help_fits_narrow_width() {
        for l in help_lines(33) {
            assert!(chars(&l.0) + chars(&l.1) <= 33, "{l:?}");
        }
        assert!(help_lines(33).len() > HELP.len());
    }
}
