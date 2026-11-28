//! The `aish <subcommand>` entry points that run outside the shell.

use crate::llm::cmd;
use crate::llm::Llm;

/// Real CLI subcommands. Only these exact first arguments are ours; returns
/// the exit code, or None to let bash handle the arguments.
pub fn dispatch(args: &[std::ffi::OsString]) -> Option<i32> {
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
            for l in cmd::run(&llm, &cmd::parse(&line), 80) {
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
            for l in cmd::status_lines(&llm) {
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
