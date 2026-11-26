//! The `llmsh` command inside a session (status, chat/complete, on/off,
//! config) and the same status as a real CLI subcommand.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Llm, Target};
use crate::config::Setting;

pub const SUBS: &[&str] = &["status", "chat", "complete", "on", "off", "config"];

/// `llmsh` or `llmsh <known subcommand> ...`; anything else (`llmsh -c ...`)
/// is a real command.
pub fn is_llmsh_line(s: &str) -> bool {
    let mut w = s.split_whitespace();
    w.next() == Some("llmsh") && w.next().is_none_or(|x| SUBS.contains(&x))
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    Status,
    Set(&'static str, String),
    Suggest(bool),
    Config,
    Usage,
}

pub fn parse(line: &str) -> Cmd {
    let w: Vec<&str> = line.split_whitespace().collect();
    match w.get(1).copied() {
        None | Some("status") => Cmd::Status,
        Some("on") => Cmd::Suggest(true),
        Some("off") => Cmd::Suggest(false),
        Some("config") => Cmd::Config,
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
            "llmsh [status | chat <провайдер/модель> | complete <…> | on | off | config]".into()
        }
        Cmd::Status | Cmd::Config => String::new(),
    }
}

fn probe(t: &Target) -> String {
    let t0 = Instant::now();
    match llmsh_llm::list_models(&t.ep.url, &t.ep.key, Duration::from_secs(1)) {
        Ok(_) => format!("ok {}ms", t0.elapsed().as_millis()),
        Err(llmsh_llm::Error::Http { status, .. }) => format!("{status}"),
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
        "--version" | "-V" if args.len() == 1 => {
            println!("llmsh {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        "--help" | "-h" if args.len() == 1 => {
            println!(
                "llmsh {}: bash with a line editor and LLM help\n\n  llmsh            start the shell\n  llmsh status     show models and whether they answer\n  llmsh config     edit the config (prints the path if not a terminal)\n\nOther arguments go to bash.",
                env!("CARGO_PKG_VERSION")
            );
            Some(0)
        }
        "status" if args.len() == 1 => {
            let llm = Llm::new();
            if llm.config().is_none() {
                println!("модель не настроена · llmsh config");
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
                eprintln!("llmsh: no HOME");
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
            "llmsh",
            "llmsh status",
            "llmsh chat a/b",
            "llmsh off",
            "llmsh  config",
        ] {
            assert!(is_llmsh_line(t), "{t}");
        }
        for t in [
            "llm",
            "llm chat x",
            "llmsh -c ls",
            "llmshx",
            "echo llmsh",
            "llmsh foo",
        ] {
            assert!(!is_llmsh_line(t), "{t}");
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(parse("llmsh"), Cmd::Status);
        assert_eq!(parse("llmsh chat a/b"), Cmd::Set("chat", "a/b".into()));
        assert_eq!(parse("llmsh chat"), Cmd::Usage);
        assert_eq!(parse("llmsh off"), Cmd::Suggest(false));
    }
}
