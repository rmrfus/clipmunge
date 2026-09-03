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

    /// Log every rewrite, before and after
    #[arg(short, long)]
    debug: bool,
}

/// Built at run time rather than written into the derive, because the useful
/// half is the config path this machine would actually use - the answer to
/// "it does nothing, where do I put the rules" without a second command.
fn after_help() -> String {
    format!(
        "Default config: {}\n\n\
         --debug WRITES CLIPBOARD CONTENTS TO THE LOG. Everything you copy while\n\
         it is on ends up in the journal, including whatever a password manager\n\
         puts there. Use it to work on a rule, not as a permanent setting.\n\n\
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
        // Doing nothing quietly is worse than not starting: a clipboard daemon
        // that rewrites nothing looks exactly like a broken one.
        bail!(
            "no config at {}\n\
             clipmunge does nothing until you give it rules; see config.lua.example",
            path.display()
        );
    }
    // The name the user gave is what everything keys off from here: a config
    // manager republishes the file and moves the symlink, so resolving once at
    // startup would pin the daemon to the version it happened to start with.
    // Engine::load resolves on every load for the same reason.
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
    // Everything that can refuse to start has now been tried: the config
    // parsed, the compositor answered, the protocol is there.
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
        // A config with a typo in it must not disarm the daemon: complain and
        // keep running the rules that already work.
        match Engine::load(engine.path()) {
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

/// Let the kernel reap the notify children.
///
/// The daemon never looks at a notification's exit status, so the alternative
/// is keeping a list of `Child` handles alive to call `try_wait` on later -
/// bookkeeping for a number nobody reads. SIG_IGN on SIGCHLD is the POSIX way
/// of saying that, and it is one call at startup instead of a leak per copy.
///
/// Nothing here waits on a child, so the usual objection - that SIG_IGN makes
/// `wait` fail with ECHILD - costs us nothing.
fn ignore_child_signals() {
    // SAFETY: signal(2) with SIG_IGN on SIGCHLD, before any thread or child
    // exists. No handler runs, so there is no async-signal-safety to get
    // wrong.
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
    }
}

fn main() -> ExitCode {
    // Arguments before the logger, because --debug decides its level. Built
    // through the command rather than Args::parse() so after_help can name
    // this machine's config path; a usage error still leaves through clap,
    // which exits 2 before anything of ours is on the stack.
    let matches = Args::command().after_help(after_help()).get_matches();
    let args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(e) => e.exit(),
    };

    ignore_child_signals();

    let default = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default)).init();
    if args.debug {
        log::warn!(
            "--debug is on: every clipboard selection is written to the log in full, \
             including anything a password manager copies"
        );
    }

    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clipmunge: {e:#}");
            ExitCode::FAILURE
        }
    }
}
