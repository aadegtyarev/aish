//! Editor tests with an in-memory writer (no terminal involved).

use crate::completion::{Candidates, CompletionSource, Outcome, Reply, Request};
use crate::plugins::{
    CompletionPager, HistorySearch, HistorySuggest, Intercept, SharedHistory, SyntaxHighlight,
};
use crate::*;

fn editor_with(hist: &str) -> Editor {
    let h = SharedHistory::new(Box::new(MemHistory::parse(hist)));
    Editor::builder()
        .theme(Theme::default())
        .with(HistorySuggest::new(h.clone()))
        .with(HistorySearch::new(h))
        .build()
}

fn feed(e: &mut Editor, s: &str) -> Vec<Event> {
    let mut ev = Vec::new();
    e.input(s.as_bytes(), &mut ev);
    ev
}

fn drawn(e: &mut Editor) -> String {
    let mut out: Vec<u8> = Vec::new();
    e.draw(&mut out, 80).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn ghost_text_accept_all_and_word() {
    let mut e = editor_with("echo hello big world\n");
    feed(&mut e, "ech");
    assert!(drawn(&mut e).contains("o hello big world"));
    feed(&mut e, "\x1b[1;3C"); // Alt-Right: one word
    assert_eq!(e.text(), "echo");
    feed(&mut e, "\x1b[C"); // Right: rest
    assert_eq!(e.text(), "echo hello big world");
    assert_eq!(
        feed(&mut e, "\r"),
        vec![Event::Submit("echo hello big world".into())]
    );
    assert_eq!(e.text(), "");
}

#[test]
fn no_ghost_without_colors_or_mid_line() {
    let h = SharedHistory::new(Box::new(MemHistory::parse("echo hi\n")));
    let mut p = Editor::builder()
        .theme(Theme::plain())
        .with(HistorySuggest::new(h))
        .build();
    feed(&mut p, "ec");
    assert!(!drawn(&mut p).contains("ho hi"));
    let mut e = editor_with("echo hi\n");
    feed(&mut e, "ec\x1b[D");
    assert!(!drawn(&mut e).contains("ho hi"));
}

#[test]
fn history_navigation_prefix_filtered() {
    let mut e = editor_with("git a\nls\ngit b\n");
    feed(&mut e, "git\x1b[A");
    assert_eq!(e.text(), "git b");
    feed(&mut e, "\x1b[A\x1b[A");
    assert_eq!(e.text(), "git a");
    feed(&mut e, "\x1b[B");
    assert_eq!(e.text(), "git b");
    feed(&mut e, "\x1b[B");
    assert_eq!(e.text(), "git");
    let mut f = editor_with("a\nb\n");
    feed(&mut f, "\x10");
    assert_eq!(f.text(), "b");
}

#[test]
fn control_keys_become_pass_through() {
    let mut e = editor_with("");
    assert_eq!(feed(&mut e, "\x04"), vec![Event::PassKey(vec![4])]);
    feed(&mut e, "ab\x1b[D");
    assert!(feed(&mut e, "\x04").is_empty());
    assert_eq!(e.text(), "a");
    assert_eq!(feed(&mut e, "\x03"), vec![Event::PassKey(vec![3])]);
    assert_eq!(e.text(), "");
    assert_eq!(feed(&mut e, "\x0c"), vec![Event::PassKey(vec![12])]);
    // keys after a finished line go along
    assert_eq!(
        feed(&mut e, "ls\rxy"),
        vec![Event::Submit("ls".into()), Event::PassKey(b"xy".to_vec())]
    );
}

#[test]
fn search_accepts_without_submitting_and_cancels() {
    let mut e = editor_with("cargo build\ngit commit\nls -la\n");
    feed(&mut e, "x\x12gc");
    let d = drawn(&mut e);
    assert!(d.contains("ommit"), "{d:?}");
    assert!(feed(&mut e, "\r").is_empty());
    assert_eq!(e.text(), "git commit");
    feed(&mut e, "\x12ls\x1b");
    // lone ESC arrives alone: needs the flush
    assert!(e.has_pending_input());
    let mut ev = vec![];
    e.flush_input(&mut ev);
    assert_eq!(e.text(), "git commit");
    assert!(!drawn(&mut e).contains("ls -la"));
}

#[test]
fn paste_is_verbatim() {
    let mut e = editor_with("");
    feed(&mut e, "\x1b[200~a\r\nb\x1b[201~");
    assert_eq!(e.text(), "a\nb");
}

#[test]
fn intercept_plugin_claims_lines() {
    let h = SharedHistory::new(Box::new(MemHistory::default()));
    let mut e = Editor::builder()
        .with(HistorySuggest::new(h))
        .with(Intercept::new(|l| l.starts_with("? ")))
        .build();
    assert_eq!(
        feed(&mut e, "? hi\r"),
        vec![Event::Intercepted("? hi".into())]
    );
    assert_eq!(feed(&mut e, "ls\r"), vec![Event::Submit("ls".into())]);
    // intercepted lines are still recalled from history
    feed(&mut e, "\x1b[A");
    assert_eq!(e.text(), "ls");
}

#[test]
fn keymap_rebinding_and_named_actions() {
    let h = SharedHistory::new(Box::new(MemHistory::parse("one\n")));
    let mut km = Keymap::emacs();
    km.bind_action(Key::Ctrl(b'o'), "history-prev");
    km.bind(Key::Ctrl(b'j'), Command::KillToStart);
    let mut e = Editor::builder()
        .keymap(km)
        .with(HistorySuggest::new(h))
        .build();
    feed(&mut e, "\x0f");
    assert_eq!(e.text(), "one");
    feed(&mut e, "\x0a"); // Ctrl-J rebound (decoded as Enter by the key decoder)
}

#[test]
fn highlight_marks_unknown_command() {
    let mut e = Editor::builder()
        .theme(Theme::default())
        .with(SyntaxHighlight::new(|w| w == "ls"))
        .build();
    feed(&mut e, "lss 'x'");
    let d = drawn(&mut e);
    assert!(d.contains("\x1b[0;31mlss"), "{d:?}");
    assert!(d.contains("\x1b[0;33m'x'"), "{d:?}");
}

struct Fake(Vec<&'static str>, bool);

impl CompletionSource for Fake {
    fn complete(&mut self, _req: &Request, reply: Reply) -> Outcome {
        let c = Candidates {
            filenames: self.1,
            nospace: false,
            items: self.0.iter().map(|s| s.to_string()).collect(),
        };
        let _ = reply;
        Outcome::Ready(c)
    }
}

fn completing(items: Vec<&'static str>) -> Editor {
    Editor::builder()
        .theme(Theme::default())
        .with(CompletionPager::new(Fake(items, false)))
        .build()
}

#[test]
fn completion_single_prefix_and_pager() {
    let mut e = completing(vec!["checkout"]);
    feed(&mut e, "git che\t");
    assert_eq!(e.text(), "git checkout ");

    let mut e = completing(vec!["foobar", "foobaz"]);
    feed(&mut e, "x fo\t");
    assert_eq!(e.text(), "x fooba");

    let mut e = completing(vec!["alpha", "beta", "gamma"]);
    feed(&mut e, "x \t"); // no common prefix: pager opens
                          // "x " + empty word -> candidates differ, pager
    let d = drawn(&mut e);
    assert!(d.contains("alpha") && d.contains("gamma"), "{d:?}");
    feed(&mut e, "\t");
    assert_eq!(e.text(), "x alpha");
    feed(&mut e, "\t\t");
    assert_eq!(e.text(), "x gamma");
    feed(&mut e, "\x1b[Z");
    assert_eq!(e.text(), "x beta");
    // printable accepts and is inserted
    feed(&mut e, "!");
    assert_eq!(e.text(), "x beta!");
    // Esc reverts
    let mut e = completing(vec!["alpha", "beta"]);
    feed(&mut e, "x \t\t");
    assert_eq!(e.text(), "x alpha");
    feed(&mut e, "\x1b");
    let mut ev = vec![];
    e.flush_input(&mut ev);
    assert_eq!(e.text(), "x ");
    assert!(!drawn(&mut e).contains("beta"));
}

#[test]
fn pager_shows_more_marker() {
    let items: Vec<&'static str> = (0..100)
        .map(|i| {
            &*Box::leak(format!("{}item{i:03}", (b'a' + (i % 26) as u8) as char).into_boxed_str())
        })
        .collect();
    let mut e = completing(items);
    feed(&mut e, "x \t");
    let d = drawn(&mut e);
    assert!(d.contains("more"), "{d:?}");
    assert!(d.matches("\r\n").count() <= 9, "{d:?}");
}

struct Slow(std::sync::Arc<std::sync::Mutex<Option<Reply>>>);
impl CompletionSource for Slow {
    fn complete(&mut self, _r: &Request, reply: Reply) -> Outcome {
        *self.0.lock().unwrap() = Some(reply);
        Outcome::Pending
    }
}

#[test]
fn async_completion_delivery_and_stale_drop() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut e = Editor::builder()
        .with(CompletionPager::new(Slow(slot.clone())))
        .build();
    feed(&mut e, "ls fo\t");
    let reply = slot.lock().unwrap().take().unwrap();
    let cands = Candidates {
        filenames: false,
        nospace: false,
        items: vec!["foo".into()],
    };
    // keystrokes were not blocked
    feed(&mut e, "x");
    reply.send(cands.clone());
    let mut ev = vec![];
    e.poll(&mut ev);
    assert_eq!(e.text(), "ls fox"); // stale: dropped
    assert!(ev.is_empty());
    // fresh one applies
    feed(&mut e, "\x15ls fo\t");
    let reply = slot.lock().unwrap().take().unwrap();
    reply.send(cands);
    e.poll(&mut ev);
    assert_eq!(e.text(), "ls foo ");
    assert_eq!(ev, vec![Event::NeedRedraw]);
}

#[test]
fn key_to_render_cost() {
    let mut data = String::new();
    for i in 0..100_000 {
        data.push_str(&format!("some-command --opt {i} /path/to/file{}\n", i % 89));
    }
    let h = SharedHistory::new(Box::new(MemHistory::parse(&data)));
    let mut e = Editor::builder()
        .theme(Theme::default())
        .with(HistorySuggest::new(h))
        .with(SyntaxHighlight::new(|w| w == "git"))
        .build();
    e.reset_screen(4);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    let line = "git commit -m \"fix the frobnicator\" --amend $HOME # zz";
    let mut worst = std::time::Duration::ZERO;
    let t = std::time::Instant::now();
    let mut n = 0;
    let mut ev = Vec::new();
    for c in line.chars() {
        let k = std::time::Instant::now();
        let mut b = [0u8; 4];
        e.input(c.encode_utf8(&mut b).as_bytes(), &mut ev);
        out.clear();
        e.draw(&mut out, 80).unwrap();
        worst = worst.max(k.elapsed());
        n += 1;
    }
    eprintln!(
        "key->render (no tty write), 100k history, worst case (no ghost hit): avg {:?} worst {:?}",
        t.elapsed() / n,
        worst
    );
    assert!(worst < std::time::Duration::from_millis(10));
}

#[test]
fn draw_final_leaves_line_without_ghost() {
    let mut e = editor_with("echo hi\n");
    feed(&mut e, "ec");
    let mut out: Vec<u8> = Vec::new();
    e.draw_final(&mut out, 80, "? q").unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("? q") && !s.contains("ho hi"));
}

#[test]
fn keys_after_enter_are_forwarded_raw() {
    let mut e = editor_with("");
    // arrows, Alt and a bracketed paste after the line keep their bytes
    let rest = "\x1b[A\x1b[1;5C\x1bb\x1b[200~p\x1b[201~";
    let ev = feed(&mut e, &format!("ls\r{rest}"));
    assert_eq!(
        ev,
        vec![
            Event::Submit("ls".into()),
            Event::PassKey(rest.as_bytes().to_vec())
        ]
    );
    // a partial sequence at the end is forwarded as is, too
    let ev = feed(&mut e, "ls\r\x1b[1");
    assert_eq!(ev[1], Event::PassKey(b"\x1b[1".to_vec()));
    assert!(!e.has_pending_input());
}

#[test]
fn late_completion_after_cursor_move_is_dropped() {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut e = Editor::builder()
        .with(CompletionPager::new(Slow(slot.clone())))
        .build();
    feed(&mut e, "ls fo\t");
    let reply = slot.lock().unwrap().take().unwrap();
    feed(&mut e, "\x1b[D"); // cursor moved: same text, different position
    reply.send(Candidates {
        filenames: false,
        nospace: false,
        items: vec!["foo".into()],
    });
    let mut ev = vec![];
    e.poll(&mut ev);
    assert_eq!(e.text(), "ls fo");
    assert!(ev.is_empty());
}

/// A misbehaving plugin passing offsets inside a multi-byte character.
struct Bad;
impl Plugin for Bad {
    fn name(&self) -> &'static str {
        "bad"
    }
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[(Key::Ctrl(b'o'), "bad")]
    }
    fn on_action(&mut self, ctx: &mut Ctx, _a: &str) -> Handled {
        ctx.replace_range(1, 2, "x"); // 1..2 is inside '中'
        ctx.set_cursor(2);
        ctx.set_cursor(999);
        ctx.replace_range(50, 10, "y");
        Handled::Yes
    }
}

#[test]
fn plugin_offsets_are_clamped_not_panicking() {
    let mut e = Editor::builder().with(Bad).build();
    feed(&mut e, "中文\x0f");
    assert!(e.text().is_char_boundary(e.cursor()));
}

struct Rows;
impl Plugin for Rows {
    fn name(&self) -> &'static str {
        "rows"
    }
    fn decorate(&mut self, _c: &mut Ctx, _w: u16, out: &mut Decoration) {
        out.push_row(|r| {
            r.push(
                RowStyle::Normal,
                "a\x1b[31mb\u{7}c-this-is-way-too-long-for-the-terminal",
            )
        });
    }
}

#[test]
fn decoration_rows_are_sanitized_and_truncated() {
    let mut e = Editor::builder().with(Rows).build();
    let mut out: Vec<u8> = Vec::new();
    e.draw(&mut out, 20).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(!s.contains("\x1b[31mb"), "{s:?}");
    assert!(s.contains("a [31mb c-this"), "{s:?}");
    assert!(!s.contains("terminal"), "{s:?}");
}

#[test]
fn resize_erase_uses_new_width() {
    let mut e = editor_with("");
    e.reset_screen(3);
    feed(&mut e, &"a".repeat(30));
    let mut out: Vec<u8> = Vec::new();
    e.draw(&mut out, 20).unwrap(); // cursor on row 1 at width 20
    out.clear();
    e.erase_resized(&mut out, 10).unwrap(); // at width 10: (3+30)/10 = 3 rows up
    assert!(String::from_utf8(out).unwrap().contains("\x1b[3A"));
}
