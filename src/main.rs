use std::process::ExitCode;

fn main() -> ExitCode {
    match ferry::cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ferry: {error:#}");
            ExitCode::FAILURE
        }
    }
}
