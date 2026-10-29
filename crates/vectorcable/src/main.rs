//! `vectorcable`: the terminal app over `vbcore`.

mod app;
mod cli;
mod commands;
mod config;
// Also built for tests elsewhere: the FIFOs and doctor rows are pure.
#[cfg(any(target_os = "macos", test))]
mod coreaudio;
#[cfg(target_os = "macos")]
mod keychain;
// Also built for tests elsewhere: the listing parser is pure.
#[cfg(any(target_os = "linux", test))]
mod pipewire;
mod theme;
mod ui;

use std::io::IsTerminal;

use cli::Command;

/// The package version with the short git hash when the build had git.
pub fn version() -> String {
    let hash = env!("VECTORCABLE_GIT_HASH");
    if hash.is_empty() {
        env!("CARGO_PKG_VERSION").to_string()
    } else {
        format!("{} ({hash})", env!("CARGO_PKG_VERSION"))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let result = match &args.command {
        Command::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
        Command::Version => {
            println!(
                "vectorcable {} (VectorBridge protocol {})",
                version(),
                vbcore::protocol::VERSION
            );
            Ok(())
        }
        Command::Up => {
            let plain = args.plain
                || cli::plain_implied(
                    std::io::stdout().is_terminal(),
                    std::env::var("TERM").ok().as_deref(),
                );
            if plain {
                app::run_plain(&args)
            } else {
                app::run_tui(&args)
            }
        }
        Command::Status => commands::status(&args),
        Command::Doctor => commands::doctor(&args),
        Command::Setup(app_name) => commands::setup(&args, app_name),
        Command::Soak => commands::soak(&args),
        Command::InstallDriver | Command::UninstallDriver => {
            println!("Driver install is not available in this build (Phase 2 on macOS; PipeWire devices are made at run time on Linux).");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
