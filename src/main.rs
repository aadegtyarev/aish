use std::process::ExitCode;

fn main() -> ExitCode {
    let _log = llmsh::shell::init_logging();
    match llmsh::run() {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            tracing::error!("{e:#}");
            // Terminal is already restored here; one terse line is acceptable
            // because the shell could not even start.
            eprintln!("llmsh: {e:#}");
            ExitCode::from(1)
        }
    }
}
