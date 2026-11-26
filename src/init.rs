//! `llmsh init`: set llmsh up without editing rc files by hand. Up to three
//! questions (where to start it, which models, apply?), idempotent marked
//! blocks in ~/.bashrc (and ~/.tmux.conf), a backup of each file once.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::llm::firstrun::{self, Detected};

pub const START: &str = "# >>> llmsh >>>";
pub const END: &str = "# <<< llmsh <<<";

/// The `.bashrc` block. LLMSH is set while llmsh's own rc sources ~/.bashrc, so
/// there is no exec loop; `LLMSH_DISABLE=1` is the escape hatch.
pub const BASHRC_BLOCK: &str = "[[ $- == *i* && -t 0 && -z ${LLMSH-} && -z ${LLMSH_DISABLE-} ]] && command -v llmsh >/dev/null && exec llmsh";

fn block(body: &str) -> String {
    format!("{START}\n{body}\n{END}\n")
}

/// `text` without any llmsh block (and the blank line before it).
pub fn without_block(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for l in text.lines() {
        if l.trim_end() == START {
            skipping = true;
            if out.last().is_some_and(|p| p.trim().is_empty()) {
                out.pop(); // the blank line `with_block` put before it
            }
        } else if skipping {
            if l.trim_end() == END {
                skipping = false;
            }
        } else {
            out.push(l);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("{}\n", out.join("\n"))
    }
}

/// `text` with the block replaced by (or, without one, ended with) `body`.
pub fn with_block(text: &str, body: &str) -> String {
    let base = without_block(text);
    if base.is_empty() {
        block(body)
    } else {
        format!("{base}\n{}", block(body))
    }
}

pub fn has_block(text: &str) -> bool {
    text.lines().any(|l| l.trim_end() == START)
}

/// What a run would do to one file.
pub struct FileChange {
    pub path: PathBuf,
    pub new: String,
    /// Lines the block adds.
    pub added: usize,
}

fn tilde(p: &Path) -> String {
    let h = std::env::var("HOME").unwrap_or_default();
    let s = p.display().to_string();
    match s.strip_prefix(&h) {
        Some(r) if !h.is_empty() && r.starts_with('/') => format!("~{r}"),
        _ => s,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn tmux_installed() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("tmux").is_file()))
}

/// Write `path` (backing the original up once as `<name>.llmsh-bak`).
fn write_with_backup(path: &Path, content: &str) -> std::io::Result<()> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".llmsh-bak");
        let bak = PathBuf::from(bak);
        if !bak.exists() {
            std::fs::copy(path, &bak)?;
        }
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, content)
}

struct Style {
    dim: &'static str,
    off: &'static str,
}

fn style() -> Style {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        Style { dim: "", off: "" }
    } else {
        Style {
            dim: "\x1b[2m",
            off: "\x1b[0m",
        }
    }
}

fn ask(prompt: &str, default: &str) -> Option<String> {
    let st = style();
    print!("{prompt} {}[{default}]{} ", st.dim, st.off);
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match std::io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let t = s.trim();
            Some(if t.is_empty() { default } else { t }.to_owned())
        }
    }
}

fn yes(a: &str) -> bool {
    matches!(a.to_lowercase().as_str(), "y" | "yes" | "д" | "да" | "1")
}

/// `llmsh init [--remove] [--yes]`; the exit code.
pub fn run(args: &[std::ffi::OsString]) -> i32 {
    let flags: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if flags.iter().any(|f| f != "--remove" && f != "--yes") {
        eprintln!("llmsh init [--remove] [--yes]");
        return 2;
    }
    let remove = flags.iter().any(|f| f == "--remove");
    let yes_all = flags.iter().any(|f| f == "--yes");
    let Some(home) = home() else {
        eprintln!("llmsh: no HOME");
        return 1;
    };
    let bashrc = home.join(".bashrc");
    let tmux = home.join(".tmux.conf");
    if remove {
        return do_remove(&[bashrc, tmux]);
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let st = style();

    // 1. where to start
    let mut mode = "1".to_owned();
    if interactive && !yes_all {
        let tm = if tmux_installed() {
            ""
        } else {
            " (tmux не найден)"
        };
        let a = ask(
            &format!("Запускать llmsh: 1 — в каждом интерактивном bash, 2 — ещё и в tmux{tm}, 3 — только вручную"),
            "1",
        );
        match a {
            Some(a) if matches!(a.as_str(), "1" | "2" | "3") => mode = a,
            Some(_) => {
                eprintln!("llmsh: ожидалось 1, 2 или 3");
                return 1;
            }
            None => return 1,
        }
    }

    // 2. models
    let cfg_path = crate::config::config_path();
    let cfg_exists = cfg_path.as_ref().is_some_and(|p| p.exists());
    let mut regenerate = !cfg_exists;
    if cfg_exists {
        let shown = cfg_path.as_deref().map(tilde).unwrap_or_default();
        println!("{}конфиг уже есть, оставляю: {shown}{}", st.dim, st.off);
        if interactive && !yes_all {
            if let Some(a) = ask(
                "Перегенерировать по автоопределению (старый уйдёт в .bak)?",
                "n",
            ) {
                regenerate = yes(&a);
            }
        }
    }
    let mut new_cfg: Option<(String, String, String, String)> = None;
    if regenerate {
        let d: Detected = firstrun::detect();
        match d.config() {
            Some((yaml, chat, comp)) => new_cfg = Some((yaml, chat, comp, d.names())),
            None => println!(
                "{}модели не найдены (llama.cpp :8080, ollama, LM Studio, $OPENAI_API_KEY, $DEEPSEEK_API_KEY) — конфиг не создаю{}",
                st.dim, st.off
            ),
        }
    }

    // 3. summary and apply
    let mut changes: Vec<FileChange> = Vec::new();
    if mode != "3" {
        let old = std::fs::read_to_string(&bashrc).unwrap_or_default();
        changes.push(FileChange {
            new: with_block(&old, BASHRC_BLOCK),
            path: bashrc.clone(),
            added: block(BASHRC_BLOCK).lines().count(),
        });
    }
    if mode == "2" && tmux_installed() {
        if let Ok(exe) = std::env::current_exe() {
            let body = format!("set -g default-shell {}", exe.display());
            let old = std::fs::read_to_string(&tmux).unwrap_or_default();
            changes.push(FileChange {
                new: with_block(&old, &body),
                path: tmux.clone(),
                added: block(&body).lines().count(),
            });
        }
    }
    let changes: Vec<FileChange> = changes
        .into_iter()
        .filter(|c| std::fs::read_to_string(&c.path).ok().as_deref() != Some(c.new.as_str()))
        .collect();
    for c in &changes {
        println!("  {}  +{} стр.", tilde(&c.path), c.added);
    }
    if let (Some((_, chat, comp, names)), Some(p)) = (&new_cfg, &cfg_path) {
        let bak = if cfg_exists {
            ", старый → config.yaml.bak"
        } else {
            ""
        };
        println!(
            "  {}  chat: {chat} · complete: {comp} ({names}){bak}",
            tilde(p)
        );
    }
    if changes.is_empty() && new_cfg.is_none() {
        println!("{}всё уже настроено{}", st.dim, st.off);
        return 0;
    }
    if !interactive && !yes_all {
        eprintln!(
            "llmsh init: ввод не из терминала — ничего не меняю (--yes применит это без вопросов)"
        );
        return 1;
    }
    if !yes_all {
        match ask("Применить?", "Y") {
            Some(a) if !matches!(a.to_lowercase().as_str(), "n" | "no" | "н" | "нет") => {}
            _ => {
                println!("{}отменено{}", st.dim, st.off);
                return 1;
            }
        }
    }
    for c in &changes {
        if let Err(e) = write_with_backup(&c.path, &c.new) {
            eprintln!("llmsh: {}: {e}", tilde(&c.path));
            return 1;
        }
    }
    if let (Some((yaml, ..)), Some(p)) = (&new_cfg, &cfg_path) {
        if cfg_exists {
            let mut bak = p.as_os_str().to_owned();
            bak.push(".bak");
            if let Err(e) = std::fs::copy(p, PathBuf::from(bak)) {
                eprintln!("llmsh: backup: {e}");
                return 1;
            }
        }
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = std::fs::write(p, yaml) {
            eprintln!("llmsh: {}: {e}", tilde(p));
            return 1;
        }
    }
    println!(
        "{}готово. Новый bash запустит llmsh; отключить на раз: LLMSH_DISABLE=1 bash{}",
        st.dim, st.off
    );
    println!(
        "{}логин-шелл: добавьте путь llmsh в /etc/shells и выполните chsh -s <путь>{}",
        st.dim, st.off
    );
    0
}

fn do_remove(files: &[PathBuf]) -> i32 {
    let mut code = 0;
    for f in files {
        match std::fs::read_to_string(f) {
            Ok(t) if has_block(&t) => match std::fs::write(f, without_block(&t)) {
                Ok(()) => println!("{}: блок удалён (копия .llmsh-bak осталась)", tilde(f)),
                Err(e) => {
                    eprintln!("llmsh: {}: {e}", tilde(f));
                    code = 1;
                }
            },
            _ => println!("{}: блока нет", tilde(f)),
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_is_added_replaced_and_removed_idempotently() {
        let orig = "export A=1\nalias x=y\n";
        let a = with_block(orig, "one");
        assert_eq!(
            a,
            "export A=1\nalias x=y\n\n# >>> llmsh >>>\none\n# <<< llmsh <<<\n"
        );
        assert_eq!(with_block(&a, "one"), a, "idempotent");
        let b = with_block(&a, "two");
        assert!(b.contains("two") && !b.contains("one") && b.matches(START).count() == 1);
        assert_eq!(without_block(&b), orig);
        assert_eq!(without_block(orig), orig);
        assert_eq!(with_block("", "x"), "# >>> llmsh >>>\nx\n# <<< llmsh <<<\n");
        assert_eq!(without_block(&with_block("", "x")), "");
        // text after the block survives
        let c = format!("{a}tail\n");
        assert_eq!(without_block(&c), "export A=1\nalias x=y\ntail\n");
    }
}
