//! `minijob` server binary.

use minijob::cli::{self, Action};
use minijob::log::{self, Logger};
use minijob::web::{self, Outcome};
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Returns the exit status instead of calling `process::exit`, so that the process ends
/// normally (on Windows `process::exit` skips the C runtime's exit handlers).
fn main() -> ExitCode {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_else(|| "minijob".to_string());
    let rest: Vec<String> = args.collect();
    let config = match cli::parse(&program, &rest) {
        Ok(Action::Version) => {
            println!("Version: {}", minijob::VERSION);
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run { config }) => config,
        Err(e) => {
            eprint!("{}", e.stderr);
            return ExitCode::from(u8::try_from(e.code).unwrap_or(1));
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(config));
    // Background tasks (callbacks in flight) are not awaited, as in Go.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    ExitCode::from(code)
}

/// Loads the configuration and runs the server, again after each restart request.
async fn run(path: String) -> u8 {
    loop {
        let cfg = match minijob::config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                println!("{e}");
                return 1;
            }
        };
        let logger = match Logger::build(&cfg.logger) {
            Ok(l) => l,
            Err(e) => {
                println!("Error while initializing logger");
                println!("{e}");
                return 1;
            }
        };
        logger.info(
            "Initialize logger",
            &[log::s("topic", "server"), log::s("method", "InitLogger")],
        );
        logger.info(
            "Configuration loaded",
            &[
                log::s("topic", "server"),
                log::s("method", "LoadConfig"),
                log::s("filename", &path),
            ],
        );
        let end = |logger: &Logger| logger.info("End program", &[log::s("topic", "server")]);
        match web::run(Arc::new(cfg), minijob::VERSION, logger.clone()).await {
            Ok(Outcome::Restart) => continue,
            Ok(Outcome::Stopped) => {
                end(&logger);
                return 0;
            }
            Err(_) => {
                end(&logger);
                return 1;
            }
        }
    }
}
