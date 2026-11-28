//! A resize while the run approval waits must not let bash repaint its prompt
//! over the approval rows (phone keyboard shown/hidden at 36 columns).

mod common;

use std::time::Duration;

use aish_llm::mock::{self, Mock};
use common::{aish_cmd, mock_config, spawn_sized, write_config, Term};
use tempfile::TempDir;

#[test]
fn resize_during_approval_keeps_the_hint_and_the_command_runs() {
    let home = TempDir::new().unwrap();
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let m = Mock::start(move |r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else if n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            let cmd = "echo ran-$((6*7)) | awk '{print $1}'";
            let args = serde_json::json!({ "command": cmd }).to_string();
            mock::chat_calls_stream(c, "", &[("c1", "run", &args)]);
        } else {
            mock::chat_stream(c, &["final-answer\n"], Duration::ZERO);
        }
    });
    write_config(home.path(), &mock_config(&m.url()));
    let mut cmd = aish_cmd(home.path(), "PS1='user@host:~$ '\n");
    cmd.env("INPUTRC", "/dev/null").env("LC_ALL", "C.UTF-8");
    let mut t = Term::new(spawn_sized(cmd, 36, 20), 36, 20);
    t.wait_for("user@host");
    t.pump_for(Duration::from_millis(500));
    t.send("? free space\r");
    t.wait_for("Esc");
    t.pump_for(Duration::from_millis(500));
    let hint = t.scr.find_row("Esc").unwrap();
    // the phone keyboard hides and shows again
    t.resize(36, 14);
    t.resize(36, 20);
    t.resize(36, 16);
    t.pump_for(Duration::from_millis(500));
    let dump = t.scr.dump();
    let last = t.scr.last_text_row();
    assert!(
        t.scr.row_text(last).contains("Esc") && !t.scr.row_text(last).contains("user@host"),
        "hint row {hint} overwritten:\n{dump}"
    );
    t.pump_for(Duration::from_millis(800));
    t.send("\r");
    t.wait_for("final-answer");
    t.pump_for(Duration::from_millis(800));
    assert!(t.scr.find_row("ran-42").is_some(), "{}", t.scr.dump());
    t.send("stty size\r");
    t.wait_for("16 36");
}
