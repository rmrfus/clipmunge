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
use crate::selection::{HTML_MIME, SECRET_MIMES, Selection, URL_MIME};
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

    /// Advertised flavours that make the daemon leave a selection entirely
    /// alone - not rewritten, not read, and so not logged even under
    /// `--debug`. Replaced rather than extended by the config, the way
    /// `clipmunge.url.default_junk` is; an empty list is a deliberate "respect
    /// nothing" and is allowed.
    pub secret_mimes: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_command: ["notify-send", "-a", "clipmunge", "clipmunge", "{}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            secret_mimes: SECRET_MIMES.iter().map(|s| s.to_string()).collect(),
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
        Self::build(&source, path, &resolved)
    }

    /// Everything `load` does once the bytes are in hand.
    ///
    /// Split out so the rule engine can be tested without a file: the two
    /// paths differ only in where the source came from, and the parts worth
    /// testing - the sandbox, rule order, what a handler may return - do not
    /// touch the filesystem at all. `given` and `resolved` still matter here
    /// because they become `package.path`.
    fn build(source: &str, given: &Path, resolved: &Path) -> Result<Self> {
        let lua = Lua::new_with(rule_stdlib(), LuaOptions::default()).lua()?;
        let ticks = Rc::new(Cell::new(0u64));
        install_limits(&lua, &ticks).lua()?;

        let collected = lua.create_table().lua()?;
        let settings_tbl = lua.create_table().lua()?;
        install_api(&lua, &collected, &settings_tbl, given, resolved).lua()?;

        ticks.set(0);
        lua.load(source)
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
            path: given.to_path_buf(),
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

    fn is_secret(&self, mimes: &[String]) -> bool {
        mimes
            .iter()
            .any(|m| self.settings.secret_mimes.iter().any(|s| s == m))
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
const SETTING_KEYS: &[&str] = &["notify_command", "secret_mimes"];

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
    // No emptiness check, unlike notify_command: `secret_mimes = {}` is a
    // config saying "honour no such hint", which is a position a person may
    // hold, and it is visible in the file where somebody can argue with it.
    if let Some(mimes) = tbl.get::<Option<Vec<String>>>("secret_mimes").lua()? {
        settings.secret_mimes = mimes;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{MARKER_MIME, TEXT_MIMES};

    /// The rule engine needs no file and no compositor: a source string goes
    /// in, a `Selection` goes past it, a `Rewrite` comes out.
    fn engine(source: &str) -> Result<Engine> {
        // A directory that does not exist is fine and is the point: nothing
        // here should reach the filesystem, so a `require` would fail loudly
        // rather than picking up whatever happens to sit next to the tests.
        let path = Path::new("/nonexistent/clipmunge/config.lua");
        Engine::build(source, path, path)
    }

    /// `Result::expect_err` wants `T: Debug`, and `Engine` has no business
    /// growing one for a test - it holds a Lua state and the rule set. Drop
    /// the Ok side first instead.
    fn load_err(source: &str) -> anyhow::Error {
        engine(source)
            .err()
            .expect("this config was supposed to fail to load")
    }

    fn plain(text: &str) -> Selection {
        let mut sel = Selection::new();
        sel.set_text(text);
        sel
    }

    fn text_of(r: &Rewrite) -> String {
        String::from_utf8_lossy(r.selection.get("text/plain").unwrap_or_default()).into_owned()
    }

    const ECHO: &str = r#"
        clipmunge.rule {
          name = "echo",
          match = [[^(.+)$]],
          handler = function(all) return "seen:" .. all end,
        }
    "#;

    #[test]
    fn a_matching_rule_replaces_the_text() {
        let mut e = engine(ECHO).expect("config should load");
        let out = e.rewrite(&plain("hello")).expect("echo should match");
        assert_eq!(text_of(&out), "seen:hello");
    }

    #[test]
    fn no_rule_matching_leaves_the_clipboard_alone() {
        let mut e = engine(
            r#"clipmunge.rule { name = "digits", match = [[^\d+$]],
                                handler = function() return "x" end }"#,
        )
        .expect("config should load");
        assert!(e.rewrite(&plain("not a number")).is_none());
    }

    #[test]
    fn the_first_matching_rule_wins_in_declaration_order() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "first",  match = [[^x$]],
                             handler = function() return "one" end }
            clipmunge.rule { name = "second", match = [[^x$]],
                             handler = function() return "two" end }
            "#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("x")).expect("something should match");
        assert_eq!(text_of(&out), "one");
    }

    #[test]
    fn a_handler_returning_nil_declines_and_the_next_rule_is_tried() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "abstains", match = [[^x$]],
                             handler = function() return nil end }
            clipmunge.rule { name = "answers",  match = [[^x$]],
                             handler = function() return "second" end }
            "#,
        )
        .expect("config should load");
        let out = e
            .rewrite(&plain("x"))
            .expect("the second rule should answer");
        assert_eq!(text_of(&out), "second");
    }

    #[test]
    fn a_handler_that_throws_is_skipped_not_fatal() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "explodes", match = [[^x$]],
                             handler = function() error("boom") end }
            clipmunge.rule { name = "survives", match = [[^x$]],
                             handler = function() return "still here" end }
            "#,
        )
        .expect("config should load");
        let out = e
            .rewrite(&plain("x"))
            .expect("the second rule should answer");
        assert_eq!(text_of(&out), "still here");
    }

    #[test]
    fn a_handler_returning_something_unusable_is_skipped() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "returns-a-number", match = [[^x$]],
                             handler = function() return 42 end }
            clipmunge.rule { name = "returns-a-string", match = [[^x$]],
                             handler = function() return "ok" end }
            "#,
        )
        .expect("config should load");
        let out = e
            .rewrite(&plain("x"))
            .expect("the second rule should answer");
        assert_eq!(text_of(&out), "ok");
    }

    #[test]
    fn a_table_of_only_notify_is_rejected() {
        // Nothing would reach the clipboard, so the rule has not done its job.
        let mut e = engine(
            r#"clipmunge.rule { name = "chatty", match = [[^x$]],
                                handler = function() return { notify = "hi" } end }"#,
        )
        .expect("config should load");
        assert!(e.rewrite(&plain("x")).is_none());
    }

    #[test]
    fn capture_groups_arrive_as_arguments_and_a_missing_group_is_nil() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "groups",
                 match = [[^(a)(b)?(c)$]],
                 handler = function(one, two, three)
                   return one .. "/" .. tostring(two) .. "/" .. three
                 end,
               }"#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("ac")).expect("should match");
        assert_eq!(text_of(&out), "a/nil/c");
    }

    #[test]
    fn plain_only_skips_a_rule_when_a_rich_flavour_is_present() {
        let src = r#"clipmunge.rule { name = "linkify", match = [[^x$]], when = "plain-only",
                                      handler = function() return "linked" end }"#;
        let mut e = engine(src).expect("config should load");
        assert!(e.rewrite(&plain("x")).is_some(), "bare text should match");

        let mut rich = plain("x");
        rich.set(HTML_MIME, b"<b>x</b>".to_vec());
        assert!(
            e.rewrite(&rich).is_none(),
            "a selection that already carries text/html is not ours to guess at"
        );
    }

    #[test]
    fn an_unknown_when_value_fails_the_load() {
        let err = load_err(
            r#"clipmunge.rule { name = "typo", match = [[^x$]], when = "plainonly",
                                handler = function() return "x" end }"#,
        );
        assert!(err.to_string().contains("plainonly"), "{err:#}");
    }

    #[test]
    fn a_bad_pattern_fails_at_load_rather_than_on_some_later_copy() {
        let err = load_err(
            r#"clipmunge.rule { name = "unbalanced", match = [[^(x$]],
                                handler = function() return "x" end }"#,
        );
        assert!(err.to_string().contains("unbalanced"), "{err:#}");
    }

    #[test]
    fn link_escapes_both_the_href_and_the_text() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "link",
                 match = [[^(.+)$]],
                 handler = function(s) return clipmunge.link("https://e.com/?a=1&b=2", s) end,
               }"#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("<script>&\"")).expect("should match");
        let html =
            String::from_utf8_lossy(out.selection.get(HTML_MIME).expect("link sets text/html"))
                .into_owned();
        assert_eq!(
            html,
            "<a href=\"https://e.com/?a=1&amp;b=2\">&lt;script&gt;&amp;&quot;</a>"
        );
        // The plain flavour is the text as given, unescaped - it is not markup.
        assert_eq!(text_of(&out), "<script>&\"");
    }

    /// Regression. `require("os")` used to hand back the real library despite
    /// `os` being nil in _G, because luaL_openlibs also files every library
    /// under package.loaded and require looks there first. A rule set could
    /// then run `os.execute`, which the README said was impossible.
    #[test]
    fn require_cannot_resurrect_io_or_os() {
        for lib in ["io", "os", "debug"] {
            let src = format!(
                r#"local ok, m = pcall(require, "{lib}")
                   if ok and type(m) == "table" then error("{lib} is reachable") end
                   clipmunge.rule {{ name = "n", match = [[^x$]],
                                     handler = function() return "x" end }}"#
            );
            engine(&src).unwrap_or_else(|e| panic!("{lib} escaped the sandbox: {e:#}"));
        }
    }

    /// Regression. A handler returns a Lua table and Lua seeds its string hash
    /// per process, so `pairs` order changed between daemon starts - six
    /// starts gave five different advertised orders, and a client that takes
    /// the first flavour it recognises pasted differently after a restart.
    #[test]
    fn the_advertised_order_is_canonical_not_lua_table_order() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "link",
                 match = [[^(.+)$]],
                 handler = function(s) return clipmunge.link("https://e.com/", s) end,
               }"#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("x")).expect("should match");
        let mimes: Vec<&str> = out.selection.mimes().collect();

        let mut want: Vec<&str> = TEXT_MIMES.to_vec();
        want.push(HTML_MIME);
        want.push(URL_MIME);
        assert_eq!(mimes, want);
        assert!(
            !mimes.contains(&MARKER_MIME),
            "the marker is added on publish"
        );
    }

    /// Regression. Without an instruction budget a `while true do end` in a
    /// handler parked the daemon for ever; `--check` on such a config had to
    /// be killed by timeout.
    #[test]
    fn a_runaway_handler_is_skipped_rather_than_hanging() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "spins",  match = [[^x$]],
                             handler = function() while true do end end }
            clipmunge.rule { name = "normal", match = [[^x$]],
                             handler = function() return "after the spin" end }
            "#,
        )
        .expect("config should load");
        let started = std::time::Instant::now();
        let out = e
            .rewrite(&plain("x"))
            .expect("the second rule should answer");
        assert_eq!(text_of(&out), "after the spin");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}, which means the budget did not fire",
            started.elapsed()
        );
    }

    #[test]
    fn a_runaway_at_load_time_fails_the_load() {
        load_err("while true do end");
    }

    #[test]
    fn each_rule_gets_the_whole_budget_rather_than_sharing_one() {
        // Two rules that each burn most of the budget. If the counter were not
        // reset per call the second would be killed by the first one's spend.
        let mut e = engine(
            r#"
            local function burn()
              local acc = 0
              for i = 1, 300000 do acc = acc + i end
              return acc
            end
            clipmunge.rule { name = "burn1", match = [[^x$]],
                             handler = function() burn() return nil end }
            clipmunge.rule { name = "burn2", match = [[^x$]],
                             handler = function() return "burnt " .. burn() end }
            "#,
        )
        .expect("config should load");
        let out = e
            .rewrite(&plain("x"))
            .expect("the second rule should answer");
        assert!(text_of(&out).starts_with("burnt "), "{}", text_of(&out));
    }

    #[test]
    fn an_empty_or_whitespace_selection_is_left_alone() {
        let mut e = engine(ECHO).expect("config should load");
        assert!(e.rewrite(&plain("")).is_none());
        assert!(e.rewrite(&plain("   \n ")).is_none());
        assert!(e.rewrite(&Selection::new()).is_none());
    }

    #[test]
    fn a_notify_field_rides_along_with_the_rewrite() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "tells",
                 match = [[^x$]],
                 handler = function() return { text = "y", notify = "did a thing" } end,
               }"#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("x")).expect("should match");
        assert_eq!(out.notify.as_deref(), Some("did a thing"));
        assert_eq!(text_of(&out), "y");
    }

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_default_secret_hint_is_honoured() {
        let e = engine(ECHO).expect("config should load");
        // The list Firefox actually advertises from about:logins.
        assert!(e.is_secret(&mimes(&[
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "text/plain",
            "x-kde-passwordManagerHint",
        ])));
        // ...and the one it advertises for an ordinary copy, hint absent.
        assert!(!e.is_secret(&mimes(&[
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "COMPOUND_TEXT",
            "TEXT",
            "text/plain",
            "STRING",
            "SAVE_TARGETS",
        ])));
    }

    #[test]
    fn secret_mimes_replaces_the_default_rather_than_extending_it() {
        let e = engine(
            r#"clipmunge.settings { secret_mimes = { "x-vault-secret" } }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function() return "x" end }"#,
        )
        .expect("config should load");
        assert!(e.is_secret(&mimes(&["text/plain", "x-vault-secret"])));
        assert!(
            !e.is_secret(&mimes(&["text/plain", "x-kde-passwordManagerHint"])),
            "a replaced list means the built-in hint is no longer in it"
        );
    }

    #[test]
    fn an_empty_secret_mimes_honours_nothing_and_is_allowed() {
        // Unlike notify_command, which must not be empty: "respect no hint" is
        // a position, and one that is visible in the config file.
        let e = engine(
            r#"clipmunge.settings { secret_mimes = {} }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function() return "x" end }"#,
        )
        .expect("an empty secret_mimes is a decision, not an error");
        assert!(!e.is_secret(&mimes(&["text/plain", "x-kde-passwordManagerHint"])));
    }

    #[test]
    fn an_unknown_settings_key_is_a_warning_and_not_a_failure() {
        // Loud in the log, but a typo'd setting must not take the rules down.
        let e = engine(
            r#"clipmunge.settings { notifi_command = { "true" } }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function() return "x" end }"#,
        )
        .expect("an unknown key warns rather than failing");
        assert_eq!(e.rule_names(), vec!["n"]);
    }

    #[test]
    fn an_empty_notify_command_fails_the_load() {
        load_err(r#"clipmunge.settings { notify_command = {} }"#);
    }

    #[test]
    fn a_rule_without_a_handler_or_a_match_fails_the_load() {
        load_err(r#"clipmunge.rule { name = "no-match", handler = function() return "x" end }"#);
        load_err(r#"clipmunge.rule { name = "no-handler", match = [[^x$]] }"#);
    }
}
