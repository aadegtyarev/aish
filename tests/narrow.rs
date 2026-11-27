//! Narrow terminals (phone, ~36 columns): the UI must stay intact. The pty has an
//! explicit size and the output goes through a small terminal emulator, so the
//! tests see what a user sees (wraps, cursor, leftovers).

mod common;

use std::process::Command;
use std::time::Duration;

use aish_llm::mock::{self, Mock};
use common::{spawn_sized, Term};
use tempfile::TempDir;

const W: u16 = 36;

thread_local! {
    static UTF8: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Every scenario runs in a UTF-8 locale (a phone) and in the C locale. They
/// differ for readline: in UTF-8 it lets the terminal autowrap a long prompt
/// (and fills a row exactly with `" \r"`), in C it breaks the rows itself.
fn utf8() -> bool {
    UTF8.with(|u| u.get())
}
const H: u16 = 20;

fn config(url: &str) -> String {
    format!(
        "providers:\n  - id: mock\n    url: {url}\n    type: chat\n    models: {{m: {{id: test}}}}\nroles:\n  chat: {{id: mock/m}}\n  complete: {{id: mock/m}}\n"
    )
}

fn command(home: &TempDir, ps1: &str, hist: &str) -> Command {
    let extra = std::fs::read_to_string(home.path().join("extra_rc")).unwrap_or_default();
    std::fs::write(
        home.path().join(".bashrc"),
        format!("PS1='{ps1}'\ncd \"$HOME/dd\" 2>/dev/null\n{extra}"),
    )
    .unwrap();
    std::fs::write(home.path().join("hist"), hist).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("AISH_TEST_UPDATE_URL", "http://127.0.0.1:9/");
    // (readline only measures wide characters right in a UTF-8 locale; there it
    // also lets the terminal wrap long prompts instead of breaking them itself)
    // (explicit locale and no inputrc: nothing of the runner's setup may leak in)
    cmd.env("INPUTRC", "/dev/null");
    if utf8() || home.path().join("utf8").exists() {
        cmd.env("LC_ALL", "C.UTF-8");
    } else {
        cmd.env("LC_ALL", "C");
    }
    let st = home.path().join("state/aish");
    std::fs::create_dir_all(&st).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(st.join("update-check"), format!("{now}\n\n{now}\n")).unwrap();
    cmd
}

/// The prompt as the terminal shows it: no `\[ \]`, no colour codes.
fn visible(ps1: &str) -> String {
    let mut s = ps1.replace("\\[", "").replace("\\]", "");
    while let Some(i) = s.find("\\e[") {
        let end = s[i..].find('m').map_or(s.len(), |k| i + k + 1);
        s.replace_range(i..end, "");
    }
    s
}

fn start(home: &TempDir, ps1: &str, hist: &str, cols: u16) -> Term {
    start_rows(home, ps1, hist, cols, H)
}

fn start_rows(home: &TempDir, ps1: &str, hist: &str, cols: u16, rows: u16) -> Term {
    let p = spawn_sized(command(home, ps1, hist), cols, rows);
    let mut t = Term::new(p, cols, rows);
    let vis = visible(ps1);
    t.wait_for(&vis.trim_end().chars().take(8).collect::<String>());
    t.pump_for(Duration::from_millis(500));
    t
}

fn start_llm(home: &TempDir, m: &Mock, ps1: &str, cols: u16) -> Term {
    start_llm_rows(home, m, ps1, cols, H)
}

fn start_llm_rows(home: &TempDir, m: &Mock, ps1: &str, cols: u16, rows: u16) -> Term {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
    start_rows(home, ps1, "", cols, rows)
}

fn prompt(n: usize) -> String {
    // n visible columns, ends with "$ "
    let mut s = String::from("u@h:");
    while s.len() < n - 2 {
        s.push('~');
    }
    s + "$ "
}

// ---- the model of a correct screen -------------------------------------

/// What the screen must show for `ps1` + `typed` (+ dim `ghost`) with the cursor
/// after `cur` typed chars: the input never relies on the terminal's own
/// wrapping, it is simply the string cut every `w` columns.
fn view(t: &Term, ps1: &str, typed: &str, ghost: &str, cur: usize) {
    view_p(t, ps1, typed, ghost, cur, None);
}

/// `popup`: rows expected right below the input (None: no popup, hint checked).
fn view_p(t: &Term, ps1: &str, typed: &str, ghost: &str, cur: usize, popup: Option<&[&str]>) {
    let w = t.scr.cols;
    let full: Vec<char> = format!("{ps1}{typed}{ghost}").chars().collect();
    let mut rows: Vec<String> = full
        .chunks(w)
        .map(|c| c.iter().collect::<String>())
        .collect();
    if full.len().is_multiple_of(w) {
        rows.push(String::new());
    }
    let pos = ps1.chars().count() + typed.chars().take(cur).count();
    let (cy, cx) = (pos / w, pos % w);
    while rows.len() > cy + 1 && rows.last().is_some_and(|r| r.trim_end().is_empty()) {
        rows.pop();
    }
    let mut rows: Vec<String> = rows.iter().map(|r| r.trim_end().to_string()).collect();
    let mut got = t.scr.lines_no_marker();
    if let Some(p) = popup {
        let input_rows = full.len() / w + 1;
        let n = got.len();
        assert!(n >= input_rows, "popup missing\n{}", t.scr.dump());
        let tail: Vec<String> = got.split_off(input_rows);
        let want: Vec<String> = p.iter().map(|x| x.to_string()).collect();
        assert_eq!(tail, want, "popup rows\n{}", t.scr.dump());
        // the cursor row cut above must not hide popup rows
        rows.truncate(input_rows);
        while rows.len() < input_rows {
            rows.push(String::new());
        }
    }
    assert_eq!(got, rows, "rows differ\n{}", t.scr.dump());
    assert_eq!((t.scr.cx, t.scr.cy), (cx, cy), "cursor\n{}", t.scr.dump());
    // the hint sits on the input's first row, only when 3 free columns remain
    let start = ps1.chars().count();
    let end = full.len();
    let want: Vec<usize> = if end / w == start / w && end % w + 4 < w {
        vec![start / w]
    } else {
        vec![]
    };
    // (a hint is drawn only when the editor is idle at a prompt; callers with
    // popups pass through `view_nomarker`)
    if popup.is_none() {
        assert_eq!(t.scr.marker_rows(), want, "marker\n{}", t.scr.dump());
    }
    // only bash's own prompt may have been wrapped by the terminal
    let wrapped: Vec<usize> = (0..t.scr.rows).filter(|&y| t.scr.row_wrapped(y)).collect();
    // (a long prompt is wrapped by the terminal or broken by readline itself, depending on the readline build and locale)
    // (only rows of the prompt itself, above the row where the input starts)
    let limit = start / w;
    assert!(
        wrapped.iter().all(|&y| y < limit),
        "stray wraps\n{}",
        t.scr.dump()
    );
}

fn narrow_prompts() -> Vec<usize> {
    // 0, 1, 2, 3 free columns, one exactly wrapping and longer ones
    vec![36, 35, 34, 33, 37, 50, 72]
}

fn long_prompt_typing_ghost_and_editing_stay_intact() {
    let home = TempDir::new().unwrap();
    let hist = "echo history-line-that-is-long-enough\n";
    for n in narrow_prompts() {
        let ps1 = prompt(n);
        let mut t = start(&home, &ps1, hist, W);
        view(&t, &ps1, "", "", 0);
        t.send("ech");
        view(&t, &ps1, "ech", "o history-line-that-is-long-enough", 3);
        t.send("x"); // ghost goes away
        view(&t, &ps1, "echx", "", 4);
        t.send("\x7f");
        view(&t, &ps1, "ech", "o history-line-that-is-long-enough", 3);
        t.send("\x1b[C"); // accept
        view(&t, &ps1, "echo history-line-that-is-long-enough", "", 37);
        t.send("\x01"); // home
        view(&t, &ps1, "echo history-line-that-is-long-enough", "", 0);
        t.send("Z");
        view(&t, &ps1, "Zecho history-line-that-is-long-enough", "", 1);
        t.send("\x05\x15"); // end, kill line
        view(&t, &ps1, "", "", 0);
        t.send("echo hi\r");
        t.wait_for("hi\n");
        t.assert_clean();
        let got = t.scr.lines_no_marker();
        let w = W as usize;
        let mut want: Vec<String> = vec![];
        let first = format!("{ps1}echo hi");
        let fc: Vec<char> = first.chars().collect();
        want.extend(
            fc.chunks(w)
                .map(|c| c.iter().collect::<String>().trim_end().to_string()),
        );
        want.push("hi".into());
        let pc: Vec<char> = ps1.chars().collect();
        want.extend(
            pc.chunks(w)
                .map(|c| c.iter().collect::<String>().trim_end().to_string()),
        );
        if pc.len().is_multiple_of(w) {
            want.push(String::new());
        }
        assert_eq!(got, want, "after Enter, prompt {n}\n{}", t.scr.dump());
        t.send("exit\r");
    }
}

fn with_dir(home: &TempDir) {
    let d = home.path().join("dd");
    std::fs::create_dir_all(&d).unwrap();
    for f in [
        "alpha_file_long_name_here.txt",
        "alpha_other_long_name.txt",
        "beta",
        "gamma_with_a_rather_long_name_indeed_ok",
    ] {
        std::fs::write(d.join(f), "").unwrap();
    }
}

fn tab_pager_across_the_wrap() {
    // (prompt columns, terminal columns, popup rows cut at cols - 4)
    let cases: [(usize, u16, [&str; 4]); 4] = [
        (
            36,
            36,
            [
                "alpha_file_long_name_here.txt",
                "alpha_other_long_name.txt",
                "beta",
                "gamma_with_a_rather_long_name_in",
            ],
        ),
        (
            34,
            36,
            [
                "alpha_file_long_name_here.txt",
                "alpha_other_long_name.txt",
                "beta",
                "gamma_with_a_rather_long_name_in",
            ],
        ),
        (
            50,
            36,
            [
                "alpha_file_long_name_here.txt",
                "alpha_other_long_name.txt",
                "beta",
                "gamma_with_a_rather_long_name_in",
            ],
        ),
        (
            12,
            20,
            [
                "alpha_file_long_",
                "alpha_other_long",
                "beta",
                "gamma_with_a_rat",
            ],
        ),
    ];
    let home = TempDir::new().unwrap();
    with_dir(&home);
    for (n, cols, pop) in cases {
        let ps1 = prompt(n);
        let mut t = start(&home, &ps1, "", cols);
        t.send("ls \t");
        view_p(&t, &ps1, "ls ", "", 3, Some(&pop));
        t.send("\t");
        view_p(
            &t,
            &ps1,
            "ls alpha_file_long_name_here.txt",
            "",
            32,
            Some(&pop),
        );
        t.send("\t");
        view_p(&t, &ps1, "ls alpha_other_long_name.txt", "", 28, Some(&pop));
        t.send("\x15"); // kill line: the pager goes away, nothing is left behind
        view(&t, &ps1, "", "", 0);
        t.send("ls be\t");
        t.send("\r");
        t.wait_for("beta\n");
        t.assert_clean();
        t.send("exit\r");
    }
}

fn ctrl_r_search_across_the_wrap() {
    let home = TempDir::new().unwrap();
    let hist = "echo history-line-that-is-long-enough\nls dd\n";
    for (n, cols) in [(36usize, 36u16), (34, 36), (50, 36), (12, 20)] {
        let ps1 = prompt(n);
        let mut t = start(&home, &ps1, hist, cols);
        let c = cols as usize;
        let cut = |s: &str, k: usize| s.chars().take(k).collect::<String>();
        let l1 = cut("> ls dd", c);
        let l2 = cut("  echo history-line-that-is-long-enough", c - 1);
        t.send("\x12");
        view_p(&t, &ps1, "", "", 0, Some(&[&l1, &l2]));
        t.send("echo");
        view_p(
            &t,
            &ps1,
            "echo",
            "",
            4,
            Some(&[&cut("> echo history-line-that-is-long-enough", c - 1)]),
        );
        t.send("\x03"); // abort: back to an empty line, popup gone
        view(&t, &ps1, "", "", 0);
        t.send("\x12");
        t.send("ls");
        t.send("\r"); // accept: the found line is in the editor
        view(&t, &ps1, "ls dd", "", 5);
        t.send("\x15");
        view(&t, &ps1, "", "", 0);
        t.assert_clean();
        t.send("exit\r");
    }
}

fn ghost_longer_than_the_line_wraps_cleanly_and_leaves_no_residue() {
    let long = "echo aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff gggggggggg";
    let home = TempDir::new().unwrap();
    for (n, cols) in [(34usize, 36u16), (36, 36), (20, 36), (8, 20), (19, 20)] {
        let ps1 = prompt(n);
        let mut t = start(&home, &ps1, &format!("{long}\n"), cols);
        t.send("ec");
        view(&t, &ps1, "ec", &long[2..], 2);
        t.send("h");
        view(&t, &ps1, "ech", &long[3..], 3);
        t.send("\x1b[C"); // accept all
        view(&t, &ps1, long, "", long.len());
        t.send("\x15");
        view(&t, &ps1, "", "", 0);
        t.send("echo");
        view(&t, &ps1, "echo", &long[4..], 4);
        t.send("Q"); // dismissed by a diverging key
        view(&t, &ps1, "echoQ", "", 5);
        t.send("\x15");
        t.send("ec");
        t.send("\x03"); // Ctrl-C drops line and ghost
        t.pump();
        assert!(
            !t.scr.everything().contains("echo"),
            "ghost residue\n{}",
            t.scr.dump()
        );
        t.assert_clean();
        t.send("exit\r");
    }
}

// ---- notices -------------------------------------------------------------

fn start_cmd(cmd: Command, cols: u16, wait: &str) -> Term {
    let p = spawn_sized(cmd, cols, H);
    let mut t = Term::new(p, cols, H);
    t.wait_for(wait);
    t.pump_for(Duration::from_millis(600));
    t
}

/// The notice row(s) and the prompt: one row each, no wrap, no duplicate prompt.
fn assert_notice_rows(t: &Term, mark: &str, ps1: &str, rows: usize) {
    let lines = t.scr.lines_no_marker();
    let hit: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].contains(mark))
        .collect();
    assert_eq!(hit.len(), rows, "notice rows\n{}", t.scr.dump());
    for y in 0..t.scr.rows {
        assert!(!t.scr.row_wrapped(y), "row {y} wrapped\n{}", t.scr.dump());
    }
    for &i in &hit {
        assert!(
            lines[i].chars().count() < t.scr.cols,
            "touches the edge\n{}",
            t.scr.dump()
        );
    }
    let prompts = lines
        .iter()
        .filter(|l| l.as_str() == ps1.trim_end())
        .count();
    assert_eq!(prompts, 1, "prompt repainted once\n{}", t.scr.dump());
    assert_eq!(lines.last().unwrap(), ps1.trim_end(), "{}", t.scr.dump());
}

fn update_notice_is_cut_to_the_width() {
    for cols in [36u16, 20] {
        let home = TempDir::new().unwrap();
        let m = Mock::start(|_, c| mock::json(c, 200, "{}"));
        let dir = home.path().join(".config/aish");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
        let ps1 = prompt(12);
        let cmd = command(&home, &ps1, "");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // a newer version is known and not announced yet
        std::fs::write(
            home.path().join("state/aish/update-check"),
            format!("{now}\n99.0.0\n0\n"),
        )
        .unwrap();
        let t = start_cmd(cmd, cols, "↑ aish");
        let lines = t.scr.lines_no_marker();
        assert_eq!(lines.len(), 2, "{}", t.scr.dump());
        let full = " ↑ aish 99.0.0 — aish update";
        if cols == 36 {
            assert_eq!(lines[0], full);
        } else {
            assert!(
                lines[0].ends_with('…') && lines[0].starts_with(" ↑ aish 99.0.0"),
                "{}",
                t.scr.dump()
            );
        }
        assert_notice_rows(&t, "↑ aish", &ps1, 1);
        t.assert_clean();
    }
}

fn first_run_line_is_cut_to_the_width() {
    let m = Mock::start(|_, c| {
        mock::json(
            c,
            200,
            r#"{"data":[{"id":"qwen3:32b"},{"id":"qwen2.5-coder:1.5b"}]}"#,
        )
    });
    for cols in [36u16, 20] {
        let home = TempDir::new().unwrap();
        let ps1 = prompt(12);
        let mut cmd = command(&home, &ps1, "");
        cmd.env("AISH_PROBE_OLLAMA", m.url())
            .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
        let t = start_cmd(cmd, cols, "aish ·");
        let lines = t.scr.lines_no_marker();
        assert_eq!(lines.len(), 2, "{}", t.scr.dump());
        assert!(
            lines[0].starts_with("aish · chat: ollam"),
            "{}",
            t.scr.dump()
        );
        assert!(lines[0].ends_with('…'), "{}", t.scr.dump());
        assert_notice_rows(&t, "aish ·", &ps1, 1);
        t.assert_clean();
    }
}

fn job_notice_with_a_long_command_is_one_row() {
    let m = Mock::start(|_, c| mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#));
    for (cols, n) in [(36u16, 20usize), (36, 34), (36, 50), (20, 12), (20, 30)] {
        let home = TempDir::new().unwrap();
        let ps1 = prompt(n);
        let mut t = start_llm(&home, &m, &ps1, cols);
        t.send("sh -c 'sleep 1' a-very-long-argument-to-make-the-line-long &\r");
        t.wait_for("✓");
        t.pump_for(Duration::from_millis(800));
        let lines = t.scr.lines_no_marker();
        let row = lines.iter().find(|l| l.contains('✓')).unwrap().clone();
        assert!(
            row.starts_with(" ✓ [1] sh -c") && row.ends_with("s"),
            "{row:?}\n{}",
            t.scr.dump()
        );
        assert!(row.contains('…'), "{row:?}");
        // the prompt is repainted exactly once under the notice
        let k = lines.iter().position(|l| l.contains('✓')).unwrap();
        // (the repainted prompt may be autowrapped in UTF-8; the notice is not)
        assert!(!t.scr.row_wrapped(k), "notice wrapped\n{}", t.scr.dump());
        let tail: Vec<char> = ps1.chars().collect();
        let want: Vec<String> = tail
            .chunks(cols as usize)
            .map(|c| c.iter().collect::<String>().trim_end().to_string())
            .collect();
        assert_eq!(
            &lines[k + 1..k + 1 + want.len()],
            &want[..],
            "{}",
            t.scr.dump()
        );
        assert_eq!(lines.len(), k + 1 + want.len(), "{}", t.scr.dump());
        // no leftover of the prompt that bash drew before the notice replaced it
        let first = &want[0];
        let n_first = lines
            .iter()
            .filter(|l| l.starts_with(first.as_str()))
            .count();
        assert_eq!(n_first, 2, "old prompt row left behind\n{}", t.scr.dump());
        t.assert_clean();
    }
}

// ---- the `?` answer -------------------------------------------------------

const URL: &str = "https://example.com/some/very/long/path/that/never/ends/index.html?q=1";
const LONGWORD: &str = "Supercalifragilisticexpialidocious-and-then-some-more-letters";

fn answer_mock() -> Mock {
    Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else {
            mock::chat_stream(
                c,
                &[
                    &format!("This is a rather long sentence that must wrap onto several rows. See {URL} for more.\n"),
                    &format!("{LONGWORD}\n- a list item that is also long enough to wrap around\nUse `ls -la --color=auto /var/log` now.\nTry:\n```bash\necho one-two-three-four-five-six-seven-eight-nine-ten\n```\n"),
                ],
                Duration::from_millis(10),
            );
        }
    })
}

fn question_answer_keeps_the_bar_and_fits_the_width() {
    for (cols, n) in [(36u16, 12usize), (36, 36), (36, 50), (20, 12)] {
        let home = TempDir::new().unwrap();
        let m = answer_mock();
        let ps1 = prompt(n);
        let mut t = start_llm_rows(&home, &m, &ps1, cols, 40);
        t.send("? say hi\r");
        t.wait_for("Try:");
        t.pump_for(Duration::from_millis(800));
        t.pump_for(Duration::from_millis(500));
        t.assert_clean();
        let c = cols as usize;
        // every answer row starts with the dim bar and fits the line
        let first = t.scr.find_row("This is").unwrap();
        let last_text = t.scr.last_text_row();
        let mut body = String::new();
        for y in first..=last_text {
            let row = t.scr.row_text(y);
            if row.starts_with(&ps1.trim_end()[..ps1.trim_end().len().min(c)]) {
                break; // the new prompt
            }
            assert!(!t.scr.row_wrapped(y), "row {y} wrapped\n{}", t.scr.dump());
            assert!(
                row.starts_with("│ "),
                "row {y} lost the bar: {row:?}\n{}",
                t.scr.dump()
            );
            assert!(t.scr.is_dim(0, y), "bar not dim, row {y}\n{}", t.scr.dump());
            body.push_str(row.trim_start_matches("│ "));
            body.push(' ');
        }
        // long words and the URL are broken, never lost
        let squashed: String = body.split_whitespace().collect();
        assert!(
            squashed.contains(&LONGWORD.replace(' ', "")),
            "long word lost\n{}",
            t.scr.dump()
        );
        assert!(squashed.contains(URL), "url lost\n{}", t.scr.dump());
        // the command is in the input line, complete, after the prompt
        let all = t.scr.lines_no_marker().join("");
        assert!(
            all.contains("echo one-two-three-four-five-six-seven-eight-nine-ten"),
            "{}",
            t.scr.dump()
        );
        let cmd = "echo one-two-three-four-five-six-seven-eight-nine-ten";
        // ... and the line below the answer is exactly prompt + command, cursor at its end
        let lines = t.scr.lines_no_marker();
        let head: String = ps1.trim_end().chars().take(c).collect();
        let at = lines.iter().rposition(|l| l.starts_with(&head)).unwrap();
        let full: Vec<char> = format!("{ps1}{cmd}").chars().collect();
        let mut want: Vec<String> = full
            .chunks(c)
            .map(|k| k.iter().collect::<String>().trim_end().to_string())
            .collect();
        if full.len().is_multiple_of(c) {
            want.push(String::new());
        }
        assert_eq!(&lines[at..], &want[..], "{}", t.scr.dump());
        assert_eq!(
            (t.scr.cy, t.scr.cx),
            (at + full.len() / c, full.len() % c),
            "{}",
            t.scr.dump()
        );
        t.send("\x15");
    }
}

// ---- Ctrl-Q overlay ----------------------------------------------------------

fn overlay_header_input_answer_and_footer_fit_and_the_screen_comes_back() {
    let m = Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else {
            mock::chat_stream(
                c,
                &[&format!(
                    "Это довольно длинный ответ, который не помещается в строку. See {URL} ok.\nTry this:\n```bash\necho INSERTED-TEXT-that-is-long-enough-to-wrap-around\n```\n"
                )],
                Duration::from_millis(10),
            );
        }
    });
    for (cols, footer) in [
        (36u16, "  Enter — вставить · Esc — закрыть"),
        (30, "  Enter — вставить · Esc"),
        (20, "  Enter · Esc"),
    ] {
        let home = TempDir::new().unwrap();
        let ps1 = prompt(12);
        let mut t = start_llm_rows(&home, &m, &ps1, cols, 40);
        t.send("echo before-overlay\r");
        t.send("sh -c 'cat' a-long-argument-to-make-the-header-long\r");
        let before = t.scr.lines();
        let (cx, cy) = (t.scr.cx, t.scr.cy);
        t.send("\x11");
        let c = cols as usize;
        let head = t.scr.row_text(0);
        assert!(
            head.starts_with("  sh") && head.chars().count() < c,
            "{head:?}\n{}",
            t.scr.dump()
        );
        assert!(
            head.ends_with(" · вывода нет") && head.contains('…'),
            "{head:?}"
        );
        assert_eq!(t.scr.row_text(1), "?");
        t.send("why does this a very long question wrap");
        assert!(
            t.scr.row_text(1).starts_with("? why does"),
            "{}",
            t.scr.dump()
        );
        t.send("\r");
        t.wait_for("INSERTED");
        t.pump_for(Duration::from_millis(600));
        t.assert_clean();
        let last = t.scr.last_text_row();
        assert_eq!(t.scr.row_text(last), footer, "{}", t.scr.dump());
        assert!(t.scr.is_dim(2, last));
        let first_bar = t.scr.find_row("│ ").unwrap();
        for y in 0..=last {
            assert!(!t.scr.row_wrapped(y), "row {y} wrapped\n{}", t.scr.dump());
            if (first_bar..last).contains(&y) {
                assert!(
                    t.scr.row_text(y).starts_with("│ "),
                    "row {y}\n{}",
                    t.scr.dump()
                );
            }
        }
        t.send("\x1b"); // close: the app screen is back exactly as it was
        assert_eq!(t.scr.lines(), before, "{}", t.scr.dump());
        assert_eq!((t.scr.cx, t.scr.cy), (cx, cy));
        t.send("\x04");
        t.send("exit\r");
    }
}

// ---- SIGWINCH -----------------------------------------------------------------

fn resize_wide_to_narrow_and_back_while_editing_a_long_line() {
    let hist =
        "echo aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee ffffffffff gggggggggg\n";
    for n in [12usize, 34] {
        let home = TempDir::new().unwrap();
        let ps1 = prompt(n);
        let mut t = start(&home, &ps1, hist, 80);
        let typed = "echo aaaaaaaaaa bbbbbbbbbb cccccccccc dddddd";
        t.send(typed);
        let ghost = &hist.trim_end()[typed.len()..];
        view(&t, &ps1, typed, ghost, typed.len());
        for (cols, rows) in [(36u16, H), (20, H), (36, H), (80, H), (20, H), (80, H)] {
            t.resize(cols, rows);
            t.pump_for(Duration::from_millis(500));
            view(&t, &ps1, typed, ghost, typed.len());
        }
        t.send("\x01"); // home, then edit in the middle at a narrow width
        view(&t, &ps1, typed, "", 0); // the suggestion only shows at the end of the line
        t.resize(36, H);
        view(&t, &ps1, typed, "", 0);
        t.send("X");
        let typed2 = format!("X{typed}");
        view(&t, &ps1, &typed2, "", 1);
        t.send("\x05\x15");
        view(&t, &ps1, "", "", 0);
        t.assert_clean();
        t.send("exit\r");
    }
}

// ---- prompts with a newline, wide characters and colours ----------------------

const TWO_LINE: &str = "u@h\n$ ";

fn count_lines(t: &Term, text: &str) -> usize {
    t.scr
        .lines_no_marker()
        .iter()
        .filter(|l| l.as_str() == text)
        .count()
}

fn two_line_prompt_keeps_its_first_line_under_notices() {
    let m = Mock::start(|_, c| mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#));
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        let mut t = start_llm(&home, &m, TWO_LINE, cols);
        t.send("sh -c 'sleep 1' long-argument-to-make-the-notice-long &\r");
        t.wait_for("✓");
        t.pump_for(Duration::from_millis(800));
        let l = t.scr.lines_no_marker();
        let k = l.iter().position(|r| r.contains('✓')).unwrap();
        // first prompt (with the command) and the repainted one keep their `u@h`
        assert_eq!(count_lines(&t, "u@h"), 2, "{}", t.scr.dump());
        assert_eq!(&l[k + 1..], ["$"], "{}", t.scr.dump());
        t.assert_clean();
    }
}

fn two_line_prompt_update_notice() {
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        let m = Mock::start(|_, c| mock::json(c, 200, "{}"));
        let dir = home.path().join(".config/aish");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
        let cmd = command(&home, TWO_LINE, "");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(
            home.path().join("state/aish/update-check"),
            format!("{now}\n99.0.0\n0\n"),
        )
        .unwrap();
        let t = start_cmd(cmd, cols, "↑ aish");
        let l = t.scr.lines_no_marker();
        assert_eq!(l.len(), 3, "{}", t.scr.dump());
        assert_eq!(l[0], "u@h", "{}", t.scr.dump());
        assert!(l[1].starts_with(" ↑ aish"), "{}", t.scr.dump());
        assert_eq!(l[2], "$", "{}", t.scr.dump());
    }
}

fn two_line_prompt_answer_reruns_prompt_command() {
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        std::fs::write(
            home.path().join("extra_rc"),
            "PROMPT_COMMAND='echo x >> \"$HOME/pc.log\"'\n",
        )
        .unwrap();
        let m = answer_mock();
        let mut t = start_llm_rows(&home, &m, TWO_LINE, cols, 40);
        let runs = || {
            std::fs::read_to_string(home.path().join("pc.log"))
                .unwrap()
                .lines()
                .count()
        };
        let before = runs();
        t.send("? say hi\r");
        t.wait_for("Try:");
        t.pump_for(Duration::from_millis(800));
        assert_eq!(
            runs(),
            before + 1,
            "PROMPT_COMMAND after the answer\n{}",
            t.scr.dump()
        );
        let l = t.scr.lines_no_marker();
        let k = l.iter().rposition(|r| r == "u@h").unwrap();
        assert!(l[k + 1].starts_with("$ echo one-two"), "{}", t.scr.dump());
        assert!(k > 2, "{}", t.scr.dump());
        assert!(
            l.iter().any(|r| r.starts_with("│ Try:")),
            "{}",
            t.scr.dump()
        );
        t.assert_clean();
    }
}

fn stray_background_output_then_a_notice_keeps_the_output() {
    let m = Mock::start(|_, c| mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#));
    for (cols, ps1) in [(80u16, TWO_LINE), (36, TWO_LINE), (36, "u@h:~$ ")] {
        let home = TempDir::new().unwrap();
        let mut t = start_llm(&home, &m, ps1, cols);
        t.send("(sleep 1; echo STRAY-OUT; sleep 1) &\r");
        t.wait_for("STRAY-OUT");
        t.wait_for("✓");
        t.pump_for(Duration::from_millis(600));
        let l = t.scr.lines_no_marker();
        let s = l
            .iter()
            .position(|r| r.contains("STRAY-OUT"))
            .unwrap_or_else(|| panic!("{}", t.scr.dump()));
        let k = l.iter().position(|r| r.contains('✓')).unwrap();
        assert!(s < k, "{}", t.scr.dump());
        assert!(
            l.iter().filter(|r| r.contains("STRAY-OUT")).count() >= 2,
            "{}",
            t.scr.dump()
        ); // the job line quotes it too
        assert_eq!(
            l.last().unwrap(),
            ps1.rsplit('\n').next().unwrap().trim_end(),
            "{}",
            t.scr.dump()
        );
        t.assert_clean();
    }
}

fn wide_characters_and_colours_in_the_prompt() {
    let m = Mock::start(|_, c| mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#));
    // 24 columns, and 40 columns (wraps at 36)
    for (cjk, cols) in [(12usize, 36u16), (19, 36)] {
        let home = TempDir::new().unwrap();
        std::fs::write(home.path().join("utf8"), "").unwrap();
        let ps1 = format!("\\[\\e[1;32m\\]{}\\[\\e[0m\\]$ ", "日".repeat(cjk));
        let mut t = start_llm(&home, &m, &ps1, cols);
        let width = cjk * 2 + 2;
        t.send("abc");
        let (cx, cy) = (t.scr.cx, t.scr.cy);
        assert_eq!(
            (cx, cy),
            ((width + 3) % 36, (width + 3) / 36),
            "{}",
            t.scr.dump()
        );
        t.send("\x15");
        t.send("sh -c 'sleep 1' a &\r");
        t.wait_for("✓");
        t.pump_for(Duration::from_millis(800));
        let l = t.scr.lines_no_marker();
        let k = l.iter().position(|r| r.contains('✓')).unwrap();
        assert!(l[k + 1].starts_with('日'), "{}", t.scr.dump());
        assert!(l.iter().filter(|r| r.contains('日')).count() >= 2);
        // (the prompt's own rows are wrapped by the terminal; the notice is not)
        assert!(!t.scr.row_wrapped(k), "{}", t.scr.dump());
        t.assert_clean();
    }
}

// ---- a notice that arrives after the prompt is up -------------------------------

fn late_notice_check(t: &Term, mark: &str, ps1: &str) {
    let l = t.scr.lines_no_marker();
    let last = ps1.rsplit('\n').next().unwrap().trim_end();
    assert_eq!(l.last().map(String::as_str), Some(last), "{}", t.scr.dump());
    let k = l
        .iter()
        .position(|r| r.contains(mark))
        .unwrap_or_else(|| panic!("{}", t.scr.dump()));
    assert_eq!(
        k + 1,
        l.len() - 1,
        "notice right above the last prompt line\n{}",
        t.scr.dump()
    );
    assert!(l[k].chars().count() < t.scr.cols, "{}", t.scr.dump());
    assert_eq!(l.iter().filter(|r| r.contains(mark)).count(), 1);
    if ps1.contains('\n') {
        assert_eq!(l[0], "u@h", "{}", t.scr.dump());
    }
    assert!(!t.scr.row_wrapped(k), "{}", t.scr.dump());
}

fn late_first_run() {
    let m = Mock::start(|_, c| {
        // the probe gives up after 300 ms, the editor takes the prompt within ~10 ms
        std::thread::sleep(Duration::from_millis(200));
        mock::json(
            c,
            200,
            r#"{"data":[{"id":"qwen3:32b"},{"id":"qwen2.5-coder:1.5b"}]}"#,
        )
    });
    for (cols, ps1) in [
        (36u16, prompt(12)),
        (80, TWO_LINE.to_string()),
        (36, TWO_LINE.to_string()),
    ] {
        let home = TempDir::new().unwrap();
        let mut cmd = command(&home, &ps1, "");
        cmd.env("AISH_PROBE_OLLAMA", m.url())
            .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
        let mut t = Term::new(spawn_sized(cmd, cols, H), cols, H);
        t.wait_for(&visible(&ps1).trim_end().chars().take(8).collect::<String>());
        // the prompt is up and idle first; the probe answers later
        t.wait_for("aish ·");
        t.pump_for(Duration::from_millis(800));
        late_notice_check(&t, "aish ·", &ps1);
        t.assert_clean();
    }
}

fn late_update_notice() {
    let m = Mock::start(|r, c| {
        if r.path == "/latest" {
            std::thread::sleep(Duration::from_millis(900));
            mock::json(c, 200, r#"{"tag_name":"v99.0.0","assets":[]}"#);
        } else {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        }
    });
    for (cols, ps1) in [(36u16, prompt(12)), (36, TWO_LINE.to_string())] {
        let home = TempDir::new().unwrap();
        let dir = home.path().join(".config/aish");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
        let mut cmd = command(&home, &ps1, "");
        let cache = home.path().join("state/aish/update-check");
        std::fs::remove_file(&cache).unwrap(); // a check is due
        cmd.env(
            "AISH_TEST_UPDATE_URL",
            format!("http://127.0.0.1:{}/latest", m.port),
        );
        let mut t = Term::new(spawn_sized(cmd, cols, H), cols, H);
        t.wait_for(&visible(&ps1).trim_end().chars().take(8).collect::<String>());
        // not yet announced, so not marked as notified either
        assert!(!t.scr.everything().contains("↑ aish"));
        t.wait_for("↑ aish");
        t.pump_for(Duration::from_millis(800));
        late_notice_check(&t, "↑ aish", &ps1);
        let c = std::fs::read_to_string(&cache).unwrap();
        let notified: u64 = c.lines().nth(2).unwrap().parse().unwrap();
        assert!(c.contains("99.0.0") && notified > 0, "{c:?}");
        t.assert_clean();
    }
}

macro_rules! both {
    ($($n:ident),* $(,)?) => {
        mod utf8 {
            $( #[test] fn $n() { super::UTF8.with(|u| u.set(true)); super::$n() } )*
        }
        mod c_locale {
            $( #[test] fn $n() { super::UTF8.with(|u| u.set(false)); super::$n() } )*
        }
    };
}

both!(
    late_first_run,
    late_update_notice,
    long_prompt_typing_ghost_and_editing_stay_intact,
    tab_pager_across_the_wrap,
    ctrl_r_search_across_the_wrap,
    ghost_longer_than_the_line_wraps_cleanly_and_leaves_no_residue,
    update_notice_is_cut_to_the_width,
    first_run_line_is_cut_to_the_width,
    job_notice_with_a_long_command_is_one_row,
    question_answer_keeps_the_bar_and_fits_the_width,
    overlay_header_input_answer_and_footer_fit_and_the_screen_comes_back,
    resize_wide_to_narrow_and_back_while_editing_a_long_line,
    two_line_prompt_keeps_its_first_line_under_notices,
    two_line_prompt_update_notice,
    two_line_prompt_answer_reruns_prompt_command,
    stray_background_output_then_a_notice_keeps_the_output,
    wide_characters_and_colours_in_the_prompt
);
