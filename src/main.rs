//! clipmunge - rule-driven Wayland clipboard rewriter.

mod clipboard;
mod config;
mod notify_ready;
mod selection;
mod urlclean;
mod watch;

use std::os::fd::BorrowedFd;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser};

use clipboard::Clipboard;
use config::Engine;
use watch::Watcher;

#[derive(Parser)]
#[command(version, about = "Rule-driven Wayland clipboard rewriter", long_about = None)]
struct Args {
    /// Config to load
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Load the config, report problems, exit
    #[arg(long)]
    check: bool,

    /// Do not reload when the config changes
    #[arg(long)]
    no_reload: bool,

    /// Never run the notify command
    #[arg(long)]
    no_notify: bool,

    /// Log clipboard text previews and rewrites
    #[arg(short, long)]
    debug: bool,
}

/// Build help at runtime to include the default config path.
fn after_help() -> String {
    format!(
        "Default config: {}\n\n\
         --debug logs clipboard text previews, which may include passwords.\n\n\
         RUST_LOG overrides the log level either way.",
        Engine::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<no HOME>".into()),
    )
}

fn run(args: Args) -> Result<()> {
    let path = match args.config {
        Some(p) => p,
        None => Engine::default_path().context("neither XDG_CONFIG_HOME nor HOME is set")?,
    };
    if !path.exists() {
        bail!(
            "no config at {}\n\
             clipmunge does nothing until you give it rules; see config.lua.example",
            path.display()
        );
    }
    // Keep the given path so reloads follow replaced config symlinks.
    let resolved = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;

    let mut engine = Engine::load(&path)?;
    log::info!(
        "loaded {} rule(s) from {}: {}",
        engine.rule_names().len(),
        resolved.display(),
        engine.rule_names().join(", ")
    );
    if args.check {
        return Ok(());
    }

    // Directories, not the file; see watch.rs for why, and for which ones.
    let mut watcher = if args.no_reload {
        None
    } else {
        match Watcher::new(&watch::candidates(&path, &resolved)) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("config reload disabled: {e:#}");
                None
            }
        }
    };

    let mut clipboard = Clipboard::connect()?;
    clipboard.log_contents(args.debug);
    engine.set_notify(!args.no_notify);
    log::info!("watching the clipboard");
    // All fallible startup steps must precede the readiness notification.
    notify_ready::ready();

    loop {
        let timeout = watcher.as_ref().and_then(|w| w.timeout());
        let fds: Vec<BorrowedFd> = watcher.iter().map(|w| w.as_fd()).collect();
        let ready = clipboard.tick(&mut engine, &fds, timeout)?;

        let Some(w) = watcher.as_mut() else { continue };
        if ready.first().copied().unwrap_or(false) {
            w.absorb();
        }
        if !w.take_settled() {
            continue;
        }
        // Keep the previous rules if the new config fails to load.
        match engine.reload() {
            Ok(fresh) => {
                log::info!(
                    "reloaded {} rule(s): {}",
                    fresh.rule_names().len(),
                    fresh.rule_names().join(", ")
                );
                engine = fresh;
            }
            Err(e) => log::error!("config reload failed, keeping the old rules: {e:#}"),
        }
    }
}

/// Let the kernel reap notification children via SIGCHLD = SIG_IGN.
/// No code waits on children; wait calls would fail with ECHILD.
fn ignore_child_signals() {
    // SAFETY: signal(2) with SIG_IGN on SIGCHLD, before any thread or child
    // exists. No handler runs, so there is no async-signal-safety to get
    // wrong.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
}

fn main() -> ExitCode {
    // Parse arguments before configuring the logger because --debug sets its level.
    // Build the command explicitly to add the default config path to help.
    let matches = Args::command().after_help(after_help()).get_matches();
    let args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(e) => e.exit(),
    };

    ignore_child_signals();

    let default = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default)).init();
    if args.debug {
        log::warn!("--debug is on: clipboard text previews may be logged, including passwords");
    }

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clipmunge: {e:#}");
            ExitCode::FAILURE
        }
    }
}
