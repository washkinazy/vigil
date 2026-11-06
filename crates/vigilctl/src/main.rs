use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 {
        match args[0].to_str() {
            Some("--help" | "-h") => {
                println!(
                    "vigilctl - Control interface for Vigil\n\n\
                     Usage: vigilctl --help | --version\n\n\
                     Scaffold only: session management and control commands are not implemented."
                );
                return ExitCode::SUCCESS;
            }
            Some("--version" | "-V") => {
                println!("vigilctl {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    eprintln!("vigilctl: scaffold only; use --help or --version. No session actions performed.");
    ExitCode::from(2)
}
