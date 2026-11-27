//! A tiny REPL: history file, ghost suggestions, Ctrl-R, Tab file completion.
//!
//!     cargo run --example standalone [history-file]

use lineedit_wip::completion::{Candidates, CompletionSource, Outcome, Reply, Request};
use lineedit_wip::plugins::{
    CompletionPager, HistorySearch, HistorySuggest, SharedHistory, SyntaxHighlight,
};
use lineedit_wip::{Editor, LineReader, MemHistory, Prompt, Signal};

/// Completes the word under the cursor with names from the file system.
struct Files;

impl CompletionSource for Files {
    fn complete(&mut self, req: &Request, _reply: Reply) -> Outcome {
        let cur = req.cur();
        let (dir, base) = match cur.rfind('/') {
            Some(i) => (&cur[..=i], &cur[i + 1..]),
            None => ("", cur),
        };
        let rd = std::fs::read_dir(if dir.is_empty() { "." } else { dir });
        let items = rd
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_str()?.to_owned();
                let is_dir = e.file_type().ok()?.is_dir();
                n.starts_with(base)
                    .then(|| format!("{dir}{n}{}", if is_dir { "/" } else { "" }))
            })
            .collect();
        Outcome::Ready(Candidates {
            filenames: true,
            nospace: false,
            items,
        })
    }
}

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "history.txt".into());
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let history = SharedHistory::new(Box::new(MemHistory::parse(&text)));

    let editor = Editor::builder()
        .with(HistorySuggest::new(history.clone()))
        .with(HistorySearch::new(history))
        .with(CompletionPager::new(Files))
        .with(SyntaxHighlight::new(|w| {
            matches!(w, "echo" | "ls" | "cat" | "exit")
        }))
        .build();
    let mut reader = LineReader::new(editor);
    let prompt = Prompt {
        left: "\x1b[1;32m>\x1b[0m ".into(),
        right: "\x1b[2mCtrl-R search, Tab complete\x1b[0m".into(),
    };
    loop {
        match reader.read_line(&prompt)? {
            Signal::Line(l) if l.trim() == "exit" => break,
            Signal::Line(l) => {
                println!("you typed: {l}");
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                use std::io::Write;
                writeln!(f, "{l}")?;
            }
            Signal::Interrupt => println!("^C"),
            Signal::Eof => break,
            _ => {}
        }
    }
    Ok(())
}
