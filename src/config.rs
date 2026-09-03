//! The Lua rule engine.
//!
//! The config is Lua rather than a declarative format because the useful rules
//! are not substitutions. Canonicalising an Amazon link means picking an ASIN
//! out of a path; mapping a bare identifier to the right tracker means a table
//! lookup. Expressed as regex-and-template those are write-only; as five lines
//! of code they are obvious. The one rule everybody wants, dropping tracking
//! parameters, ships in the library instead - see `urlclean`.
//!
//! The config file is trusted, the way a shell rc file is: it can set
//! `notify_command`, and that runs a program with rule output as an argument.
//! What the interpreter does *not* get is `io`, `os` or a C loader, so a rule
//! cannot reach the filesystem, the network or another process at copy time -
//! only the one thing the config declared up front, in a line you can read.
//!
//! Those libraries are never loaded rather than deleted afterwards. Setting a
//! global to nil looks like removal and is not: `luaL_openlibs` also files
//! every library under `package.loaded`, which is the first place `require`
//! looks, so `require("os").execute` walks straight past a nil `os`.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, Variadic, VmState};
use regex::Regex;

use crate::clipboard::{Rewrite, Rewriter};
use crate::selection::{HTML_MIME, Selection, URL_MIME};
use crate::urlclean::{DEFAULT_JUNK, strip_params};

/// The hook fires this often; `MAX_TICKS` of them ends the call. Together they
/// cap one rule at roughly ten million VM instructions, which is tens of
/// milliseconds of Lua and several thousand times what a real rule uses.
///
/// This is not a security boundary - the config is trusted - it is a guard
/// against your own `while true do end`. Without it that typo does not cost a
/// logged error, it costs a clipboard that stops working until somebody
/// notices and kills the daemon.
const HOOK_EVERY: u32 = 100_000;
const MAX_TICKS: u64 = 100;

/// Same idea for `string.rep("x", 2^30)`. The interpreter plus a loaded rule
/// set is a few hundred KB, so this is room to be wrong in, not a budget to
/// plan against.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// mlua's error type is neither Send nor Sync, so `?` cannot turn it into an
/// anyhow::Error. Flatten it to its message at the boundary.
trait LuaCtx<T> {
    fn lua(self) -> Result<T>;
}

impl<T> LuaCtx<T> for mlua::Result<T> {
    fn lua(self) -> Result<T> {
        self.map_err(|e| anyhow!("{e}"))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum When {
    /// Fire whenever the pattern matches.
    Always,
    /// Only when nobody has attached a rich flavour. A browser copying a link
    /// already knows the href; our guess should not overwrite it.
    PlainOnly,
}

struct Rule {
    name: String,
    pattern: Regex,
    when: When,
    handler: Function,
}

/// Globals a config may set with `clipmunge.settings {}`.
pub struct Settings {
    /// argv, not a shell string. `{}` is replaced by the rule's notify text as
    /// one whole argument, so a copied `"; rm -rf ~` is just characters. A
    /// template pasted through a shell would be a command injection with the
    /// clipboard as its input, which is about the worst possible source.
    pub notify_command: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_command: ["notify-send", "-a", "clipmunge", "clipmunge", "{}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

pub struct Engine {
    lua: Lua,
    /// Bumped by the instruction hook, zeroed before every call into Lua.
    ticks: Rc<Cell<u64>>,
    rules: Vec<Rule>,
    /// The path as the user named it, not the resolved one: a config manager
    /// swaps the symlink, so resolving has to happen again on every load.
    path: PathBuf,
    settings: Settings,
    notify_enabled: bool,
}

impl Engine {
    /// Default config location, honouring XDG.
    ///
    /// `XDG_CONFIG_HOME` counts only when it is set, non-empty *and* absolute,
    /// which is what the basedir spec says and is not pedantry. Taking it
    /// naively, `XDG_CONFIG_HOME=""` yields `PathBuf::from("").join(...)` - a
    /// path relative to the working directory. Under systemd that directory is
    /// whatever `WorkingDirectory` says, so the daemon would look for
    /// `clipmunge/config.lua` relative to it and refuse to start with a path
    /// nobody recognises.
    pub fn default_path() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        Some(base.join("clipmunge").join("config.lua"))
    }

    /// `path` is the config as the user named it. It is resolved here rather
    /// than once at startup, because a config manager - home-manager, stow,
    /// chezmoi - publishes a new file and moves the symlink, so the name is
    /// the only thing that stays put across a reload.
    pub fn load(path: &Path) -> Result<Self> {
        let resolved = path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?;
        let source = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()))?;

        let lua = Lua::new_with(rule_stdlib(), LuaOptions::default()).lua()?;
        let ticks = Rc::new(Cell::new(0u64));
        install_limits(&lua, &ticks).lua()?;

        let collected = lua.create_table().lua()?;
        let settings_tbl = lua.create_table().lua()?;
        install_api(&lua, &collected, &settings_tbl, path, &resolved).lua()?;

        ticks.set(0);
        lua.load(&source)
            .set_name(resolved.to_string_lossy().as_ref())
            .exec()
            .map_err(|e| anyhow!("evaluating {}: {e}", resolved.display()))?;

        let mut rules = Vec::new();
        for pair in collected.sequence_values::<Table>() {
            rules.push(build_rule(&pair.lua()?)?);
        }
        if rules.is_empty() {
            log::warn!("{} defines no rules", resolved.display());
        }

        let settings = read_settings(&settings_tbl)?;

        Ok(Self {
            lua,
            ticks,
            rules,
            path: path.to_path_buf(),
            settings,
            notify_enabled: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_notify(&mut self, yes: bool) {
        self.notify_enabled = yes;
    }

    pub fn rule_names(&self) -> Vec<&str> {
        self.rules.iter().map(|r| r.name.as_str()).collect()
    }
}

impl Rewriter for Engine {
    fn rewrite(&mut self, incoming: &Selection) -> Option<Rewrite> {
        let text = incoming.text()?.trim();
        if text.is_empty() {
            return None;
        }

        for rule in &self.rules {
            if rule.when == When::PlainOnly && !incoming.is_plain_only() {
                continue;
            }
            let Some(caps) = rule.pattern.captures(text) else {
                continue;
            };

            // Capture groups become the handler's arguments, group 1 first: a
            // rule that wants the whole match can capture it itself. A group
            // that did not participate arrives as nil rather than "".
            let mut args = Vec::with_capacity(caps.len().saturating_sub(1));
            for group in caps.iter().skip(1) {
                let value = match group {
                    Some(m) => match self.lua.create_string(m.as_str()) {
                        Ok(s) => Value::String(s),
                        Err(e) => {
                            log::warn!("rule '{}': {e}", rule.name);
                            return None;
                        }
                    },
                    None => Value::Nil,
                };
                args.push(value);
            }

            // Each rule gets the whole budget; one slow rule must not starve
            // the next one on the same copy.
            self.ticks.set(0);
            match rule.handler.call::<Value>(Variadic::from_iter(args)) {
                Ok(Value::Nil) => continue,
                Ok(value) => match to_rewrite(value) {
                    Ok(out) => {
                        log::debug!("rule '{}' matched", rule.name);
                        return Some(out);
                    }
                    Err(e) => {
                        log::warn!("rule '{}' returned something unusable: {e:#}", rule.name);
                        continue;
                    }
                },
                Err(e) => {
                    // One broken rule must not take the clipboard down with it.
                    log::warn!("rule '{}' failed: {e}", rule.name);
                    continue;
                }
            }
        }
        None
    }

    fn notify(&self, text: &str) {
        if !self.notify_enabled {
            return;
        }
        // A rule can put anything in here, including whatever was on the
        // clipboard, so cap it before it reaches a notification daemon that
        // may well keep a history.
        const MAX: usize = 200;
        let text: String = text.chars().take(MAX).collect();

        let argv = &self.settings.notify_command;
        let mut cmd = Command::new(&argv[0]);
        for arg in &argv[1..] {
            // Substitution is per whole argument. No shell is involved, so
            // quotes and semicolons in the text stay characters.
            cmd.arg(arg.replace("{}", &text));
        }
        // Fire and forget: a notification daemon that hangs must not take the
        // clipboard with it, and we never read the child's output.
        //
        // Nothing waits on the child, and nothing has to: SIGCHLD is set to
        // SIG_IGN at startup, so the kernel reaps it. Dropping the `Child`
        // does not - std says so outright - and a `try_wait` here catches a
        // process that has not had time to exit yet, which is all of them.
        // That combination leaves one zombie per notification for the life of
        // the daemon; measured, twenty spawns gave twenty.
        match cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => {}
            Err(e) => log::warn!("notify: running {}: {e}", argv[0]),
        }
    }
}

/// The libraries a rule set gets. `coroutine` is left out on purpose as well
/// as the obvious ones: a hook set with `Lua::set_hook` covers the current
/// thread, and a coroutine is a thread of its own, so a loop inside one would
/// run past the instruction budget.
fn rule_stdlib() -> StdLib {
    // The base library (print, pairs, pcall, setmetatable, ...) is always
    // loaded by mlua and is not a flag here.
    StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::PACKAGE
}

fn install_limits(lua: &Lua, ticks: &Rc<Cell<u64>>) -> mlua::Result<()> {
    let ticks = Rc::clone(ticks);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_EVERY),
        move |_, _| {
            let n = ticks.get() + 1;
            ticks.set(n);
            if n > MAX_TICKS {
                Err(mlua::Error::runtime(
                    "ran past the instruction budget - an endless loop?",
                ))
            } else {
                Ok(VmState::Continue)
            }
        },
    )?;
    // Absent only when the Lua state is managed externally, which ours is not.
    // Worth a line rather than a silent pass: without it the cap is a comment.
    if let Err(e) = lua.set_memory_limit(MEMORY_LIMIT) {
        log::warn!("no memory limit on the rule interpreter: {e}");
    }
    Ok(())
}

/// Everything `clipmunge.settings` understands. An unknown key is almost
/// always a typo, and silently ignoring it is how a config ends up not doing
/// what it plainly says.
const SETTING_KEYS: &[&str] = &["notify_command"];

fn read_settings(tbl: &Table) -> Result<Settings> {
    let mut settings = Settings::default();

    for pair in tbl.clone().pairs::<Value, Value>() {
        let (key, _) = pair.lua()?;
        match key
            .as_string()
            .and_then(|s| s.to_str().ok().map(|v| v.to_owned()))
        {
            Some(name) if SETTING_KEYS.contains(&name.as_str()) => {}
            Some(name) => log::warn!(
                "clipmunge.settings: unknown key '{name}' ignored (known: {})",
                SETTING_KEYS.join(", ")
            ),
            None => log::warn!("clipmunge.settings: non-string key ignored"),
        }
    }

    if let Some(cmd) = tbl.get::<Option<Vec<String>>>("notify_command").lua()? {
        if cmd.is_empty() {
            bail!("notify_command must not be empty");
        }
        settings.notify_command = cmd;
    }
    Ok(settings)
}

fn build_rule(spec: &Table) -> Result<Rule> {
    let name: String = spec
        .get::<Option<String>>("name")
        .lua()?
        .unwrap_or_else(|| "?".into());
    let pattern: String = spec
        .get::<Option<String>>("match")
        .lua()?
        .ok_or_else(|| anyhow!("rule '{name}' has no `match`"))?;
    // Compiled now, not on first copy: a typo has to fail while you are
    // looking at the config, not three hours later on the wrong clipboard.
    let pattern = Regex::new(&pattern).with_context(|| format!("rule '{name}': bad pattern"))?;

    let when = match spec.get::<Option<String>>("when").lua()?.as_deref() {
        None | Some("always") => When::Always,
        Some("plain-only") => When::PlainOnly,
        Some(other) => bail!("rule '{name}': unknown `when` value '{other}'"),
    };

    let handler: Function = spec
        .get::<Option<Function>>("handler")
        .lua()?
        .ok_or_else(|| anyhow!("rule '{name}' has no `handler`"))?;

    Ok(Rule {
        name,
        pattern,
        when,
        handler,
    })
}

/// A handler may return a bare string (replace the text) or a table keyed by
/// MIME type (replace exactly those flavours). Two keys are not MIME types:
/// "text" is shorthand for the whole text/plain family, and "notify" is a
/// message for the user rather than for the clipboard.
///
/// Note that the rule only *describes* the notification. Sending it is the
/// daemon's business, which is what keeps a handler a pure function and keeps
/// the sandbox statement free of exceptions.
fn to_rewrite(value: Value) -> Result<Rewrite> {
    let mut sel = Selection::new();
    let mut notify = None;

    match value {
        Value::String(s) => {
            sel.set_text(&s.to_str().lua()?);
        }
        Value::Table(t) => {
            for pair in t.pairs::<String, mlua::LuaString>() {
                let (key, data) = pair.lua()?;
                match key.as_str() {
                    "text" => {
                        sel.set_text(&data.to_str().lua()?);
                    }
                    "notify" => notify = Some(data.to_str().lua()?.to_string()),
                    mime => {
                        sel.set(mime, data.as_bytes().to_vec());
                    }
                }
            }
            if sel.is_empty() {
                bail!("nothing but `notify`: a rule has to change the clipboard too");
            }
        }
        other => bail!("expected a string or a table, got {}", other.type_name()),
    }
    // A Lua table has no order worth the name; give the flavours one before
    // they reach the wire. See Selection::canonical_order.
    sel.canonical_order();
    Ok(Rewrite {
        selection: sel,
        notify,
    })
}

fn install_api(
    lua: &Lua,
    collected: &Table,
    settings: &Table,
    given: &Path,
    resolved: &Path,
) -> mlua::Result<()> {
    sandbox(lua, given, resolved)?;

    let api = lua.create_table()?;

    let sink = collected.clone();
    api.set(
        "rule",
        lua.create_function(move |_, spec: Table| {
            sink.push(spec)?;
            Ok(())
        })?,
    )?;

    let store = settings.clone();
    api.set(
        "settings",
        lua.create_function(move |_, given: Table| {
            for pair in given.pairs::<Value, Value>() {
                let (k, v) = pair?;
                store.set(k, v)?;
            }
            Ok(())
        })?,
    )?;

    api.set(
        "html_escape",
        lua.create_function(|_, s: String| Ok(html_escape(&s)))?,
    )?;

    // The common shape, with escaping done for you. Building the table by hand
    // is allowed, and then the escaping is your problem.
    api.set(
        "link",
        lua.create_function(|lua, (url, text): (String, Option<String>)| {
            let text = text.unwrap_or_else(|| url.clone());
            let t = lua.create_table()?;
            t.set("text", text.clone())?;
            t.set(
                HTML_MIME,
                format!(
                    r#"<a href="{}">{}</a>"#,
                    html_escape(&url),
                    html_escape(&text)
                ),
            )?;
            t.set(URL_MIME, url)?;
            Ok(t)
        })?,
    )?;

    // clipmunge.url.*
    let url = lua.create_table()?;
    url.set(
        "strip_params",
        lua.create_function(|lua, (target, list): (String, Option<Vec<String>>)| {
            let patterns =
                list.unwrap_or_else(|| DEFAULT_JUNK.iter().map(|s| s.to_string()).collect());
            // nil for "nothing to do", so a rule can hand that straight back
            // and be idempotent without thinking about it.
            match strip_params(&target, &patterns) {
                Some((clean, dropped)) => {
                    Ok((Some(clean), Some(lua.create_sequence_from(dropped)?)))
                }
                None => Ok((None, None)),
            }
        })?,
    )?;
    url.set(
        "default_junk",
        lua.create_sequence_from(DEFAULT_JUNK.iter().map(|s| s.to_string()))?,
    )?;
    api.set("url", url)?;

    lua.globals().set("clipmunge", api)?;
    Ok(())
}

/// Close the doors the base and `package` libraries leave open, and point
/// `require` at the config's own directory so a rule set can be split across
/// files.
///
/// `io` and `os` are not handled here at all - see `rule_stdlib`, they were
/// never loaded. What is left to do is the code-loading half of the base
/// library, and the C loader that comes with `package`.
fn sandbox(lua: &Lua, given: &Path, resolved: &Path) -> mlua::Result<()> {
    let globals = lua.globals();
    // Ways to run code that did not come out of the config file.
    for name in ["dofile", "loadfile", "load", "collectgarbage"] {
        globals.set(name, Value::Nil)?;
    }

    let package: Table = globals.get("package")?;
    // No C loaders: loadlib would hand a rule the whole libc.
    package.set("cpath", "")?;
    package.set("loadlib", Value::Nil)?;
    package.set("path", require_path(given, resolved))?;

    let searchers: Table = package.get("searchers")?;
    // Keep [1] (preload) and [2] (the Lua file searcher); drop the C searcher
    // and the all-in-one loader.
    while searchers.raw_len() > 2 {
        searchers.raw_remove(searchers.raw_len())?;
    }
    Ok(())
}

/// Both directories the config can be said to live in, because the two layouts
/// people actually use put the neighbouring files in different places.
///
/// Edited in place - a dotfiles repo with a symlink into it - and the siblings
/// are next to the *resolved* file. Published by a config manager, and
/// `~/.config/clipmunge` is a real directory of symlinks, so the siblings are
/// next to the *given* name. Listing both costs a failed `stat` per miss and
/// removes a class of "works on my laptop" from the tracker.
fn require_path(given: &Path, resolved: &Path) -> String {
    let mut dirs: Vec<&Path> = Vec::new();
    for p in [given, resolved] {
        // `--config config.lua` has a parent of "", not None.
        let dir = match p.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.iter()
        .map(|d| format!("{}/?.lua;{}/?/init.lua", d.display(), d.display()))
        .collect::<Vec<_>>()
        .join(";")
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
