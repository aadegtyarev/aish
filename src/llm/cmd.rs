//! The `aish` command inside a session (status, chat/complete, on/off,
//! config) and the same status as a real CLI subcommand.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Llm, Target};
use crate::config::Setting;

pub const SUBS: &[&str] = &["status", "chat", "complete", "on", "off", "config", "init"];

fn is_cmd_line(s: &str, name: &str) -> bool {
    let mut w = s.split_whitespace();
    w.next() == Some(name) && w.next().is_none_or(|x| SUBS.contains(&x))
}

/// `aish` or `aish <known subcommand> ...`; anything else (`aish -c ...`)
/// is a real command.
pub fn is_aish_line(s: &str) -> bool {
    is_cmd_line(s, "aish")
}

/// The configured short form (`ai` by default), `<name> [<subcommand> ...]`.
pub fn is_short_line(s: &str, name: Option<&str>) -> bool {
    name.is_some_and(|n| is_cmd_line(s, n))
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Status,
    Set(&'static str, String),
    Suggest(bool),
    Config,
    Init,
    Usage,
}

pub fn parse(line: &str) -> Cmd {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.get(1).copied() {
        None | Some("status") => Cmd::Status,
        Some("on") => Cmd::Suggest(true),
        Some("off") => Cmd::Suggest(false),
        Some("config") => Cmd::Config,
        Some("init") => Cmd::Init,
        Some("chat") => w
            .get(2)
            .map_or(Cmd::Usage, |r| Cmd::Set("chat", (*r).into())),
        Some("complete") => w
            .get(2)
            .map_or(Cmd::Usage, |r| Cmd::Set("complete", (*r).into())),
        _ => Cmd::Usage,
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

/// Apply a non-status command; returns the dim message.
pub fn apply(llm: &Llm, cmd: &Cmd) -> String {
    match cmd {
        Cmd::Suggest(on) => {
            llm.suggest_on.store(*on, Ordering::Relaxed);
            format!("подсказки: {}", if *on { "вкл" } else { "выкл" })
        }
        Cmd::Set(role, r) => {
            let ok = llm
                .config()
                .is_some_and(|c| c.resolve_ref(r, &Setting::Unset).is_ok());
            if ok {
                llm.set_override(role, r);
                format!("{role} → {r}")
            } else {
                format!("нет такой модели: {r}")
            }
        }
        Cmd::Usage => {
            "ai | aish [status | chat <провайдер/модель> | complete <…> | on | off | config | init]"
                .into()
        }
        Cmd::Status | Cmd::Config | Cmd::Init => String::new(),
    }
}

fn probe(t: &Target) -> String {
    let t0 = Instant::now();
    match aish_llm::list_models(&t.ep.url, &t.ep.key, Duration::from_secs(1)) {
        Ok(_) => format!("ok {}ms", t0.elapsed().as_millis()),
        Err(aish_llm::Error::Http { status, .. }) => format!("{status}"),
        Err(_) => "нет ответа".into(),
    }
}

/// One line per role (probed in parallel) and the suggestions switch.
pub fn status_lines(llm: &Arc<Llm>) -> Vec<String> {
    let roles = ["chat", "complete"];
    let hs: Vec<_> = roles
        .iter()
        .map(|r| {
            let t = llm.target(r);
            std::thread::spawn(move || {
                t.map(|t| format!("{} · {} · {}", t.label, t.ep.model, probe(&t)))
            })
        })
        .collect();
    let mut v = Vec::new();
    for (r, h) in roles.iter().zip(hs) {
        let body = h
            .join()
            .ok()
            .flatten()
            .unwrap_or_else(|| "не настроен".into());
        v.push(format!("{r:<8}{body}"));
    }
    let on = llm.suggest_on.load(Ordering::Relaxed);
    v.push(format!("подсказки: {}", if on { "вкл" } else { "выкл" }));
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
                "aish {}: bash with a line editor and LLM help\n\n  aish            start the shell\n  aish status     show models and whether they answer\n  aish config     edit the config (prints the path if not a terminal)\n  aish init       set up: start aish from ~/.bashrc, detect models (--remove undoes)\n\nOther arguments go to bash.",
                env!("CARGO_PKG_VERSION")
            );
            Some(0)
        }
        // A file of that name in the cwd means bash semantics (`bash status`).
        "status" | "config" | "init" if std::path::Path::new(a).exists() => None,
        "init" => Some(crate::init::run(&args[1..])),
        "status" if args.len() == 1 => {
            let llm = Llm::new();
            if llm.config().is_none() {
                println!("модель не настроена · aish config");
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
        for t in ["ai write a poem", "aix", "ai -h", "echo ai"] {
            assert!(!is_short_line(t, Some("ai")), "{t}");
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(parse("aish"), Cmd::Status);
        assert_eq!(parse("aish chat a/b"), Cmd::Set("chat", "a/b".into()));
        assert_eq!(parse("aish chat"), Cmd::Usage);
        assert_eq!(parse("aish off"), Cmd::Suggest(false));
    }
}
