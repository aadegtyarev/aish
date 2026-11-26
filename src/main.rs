use std::io::IsTerminal;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    // Only these exact subcommands are ours; everything else is bash's.
    if let Some(code) = llmsh::llm::cmd::cli(&args) {
        return ExitCode::from(code as u8);
    }
    // Scripts, `-c`, pipes: nothing to proxy, behave exactly like bash.
    if !args.is_empty() || !std::io::stdin().is_terminal() {
        let e = llmsh::exec_bash(&args, false);
        eprintln!("llmsh: exec bash: {e}");
        return ExitCode::from(127);
    }
    let _log = llmsh::shell::init_logging();
    match llmsh::run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            // Never leave the user without a shell: log and fall back to plain bash.
            tracing::error!("{e:#}");
            drop(_log);
            let e = llmsh::exec_bash(&[], true);
            eprintln!("llmsh: exec bash: {e}");
            ExitCode::from(127)
        }
    }
}
