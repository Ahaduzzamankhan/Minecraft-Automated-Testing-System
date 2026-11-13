//! MATS command line entry point.

mod cli;
mod commands;

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use cli::Cli;

fn main() {
    if let Some(code) = run_update_helper_if_requested() {
        std::process::exit(code);
    }

    let parsed = Cli::parse();
    init_tracing(parsed.verbose, parsed.quiet);

    let runtime = tokio_less_main(parsed);
    match runtime {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(2);
        }
    }
}

fn tokio_less_main(cli: Cli) -> Result<i32> {
    commands::dispatch(cli)
}

fn init_tracing(verbose: bool, quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if quiet {
        "warn"
    } else if verbose {
        "debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mats={default},mats_cli={default}")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}

/// Internal mode used by the Windows update helper.
fn run_update_helper_if_requested() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|arg| arg == "--mats-apply-update") {
        return None;
    }
    let mut wait_pid = None;
    let mut source = None;
    let mut target = None;
    let mut restart = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--wait-pid" => {
                index += 1;
                wait_pid = args.get(index).and_then(|value| value.parse::<u32>().ok());
            }
            "--source" => {
                index += 1;
                source = args.get(index).cloned();
            }
            "--target" => {
                index += 1;
                target = args.get(index).cloned();
            }
            "--restart" => restart = true,
            _ => {}
        }
        index += 1;
    }
    let (Some(pid), Some(source), Some(target)) = (wait_pid, source, target) else {
        eprintln!("update helper: missing arguments");
        return Some(2);
    };
    match updater::apply_update_helper(
        pid,
        std::path::Path::new(&source),
        std::path::Path::new(&target),
        restart,
    ) {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("update helper failed: {error:#}");
            Some(1)
        }
    }
}

/// Flush stdout before exiting so piped output is not lost.
#[allow(dead_code)]
fn flush_stdout() {
    let _ = std::io::stdout().flush();
}
