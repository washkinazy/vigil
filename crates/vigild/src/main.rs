use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 {
        match args[0].to_str() {
            Some("--help" | "-h") => {
                println!(
                    "vigild - Session idle manager daemon\n\n\
                     Usage: vigild --help | --version\n\n\
                     Scaffold only: session management and control commands are not implemented."
                );
                return ExitCode::SUCCESS;
            }
            Some("--version" | "-V") => {
                println!("vigild {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    eprintln!("vigild: scaffold only; use --help or --version. No session actions performed.");
    ExitCode::from(2)
}
