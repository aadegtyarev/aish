use std::io::IsTerminal;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    // Only these exact subcommands are ours; everything else is bash's.
    if let Some(code) = aish::cli(&args) {
        return ExitCode::from(code as u8);
    }
    // Scripts, `-c`, pipes: nothing to proxy, behave exactly like bash.
    if !args.is_empty() || !std::io::stdin().is_terminal() {
        let e = aish::exec_bash(&args, false);
        eprintln!("aish: exec bash: {e}");
        return ExitCode::from(127);
    }
    aish::init_logging();
    // A panic is a failed start too: unwinding restores the terminal, then bash.
    let res = std::panic::catch_unwind(aish::run)
        .unwrap_or_else(|_| Err(anyhow::anyhow!("aish panicked")));
    match res {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            // Never leave the user without a shell: log and fall back to plain bash.
            tracing::error!("{e:#}");
            let e = aish::exec_bash(&[], true);
            eprintln!("aish: exec bash: {e}");
            ExitCode::from(127)
        }
    }
}
