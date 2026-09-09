//! Load Lua configuration and execute clipboard rules.
//! The config is trusted: it can choose a notification command. Lua library
//! access and resource use are restricted; see `rule_stdlib` and `sandbox`.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use mlua::{
    Function, HookTriggers, Lua, LuaOptions, StdLib, Table, UserData, UserDataMethods, Value,
    Variadic, VmState,
};
use regex::Regex;

use crate::clipboard::{Rewrite, Rewriter};
use crate::selection::{HTML_MIME, SECRET_MIMES, Selection, URL_MIME};
use crate::urlclean::{DEFAULT_JUNK, strip_params};

/// Check the instruction budget every HOOK_EVERY instructions.
/// MAX_TICKS allows roughly ten million instructions per call.
const HOOK_EVERY: u32 = 100_000;
const MAX_TICKS: u64 = 100;

/// Lua heap limit, including the interpreter and loaded rules.
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
    Always,
    /// Only match selections with no successfully read rich payloads.
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
    /// Notification argv. Substitute `{}` within each argument without a shell.
    pub notify_command: Vec<String>,

    /// Skip offers advertising any of these MIME types before reading.
    /// Config replaces the default list; an empty list disables this check.
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
    /// Bumped by the instruction hook; reset before config execution and each handler.
    ticks: Rc<Cell<u64>>,
    rules: Vec<Rule>,
    /// Unresolved config path, so reloads follow replaced symlinks.
    path: PathBuf,
    settings: Settings,
    notify_enabled: bool,
}

impl Engine {
    /// Use absolute XDG_CONFIG_HOME, falling back to $HOME/.config.
    /// Empty or relative XDG_CONFIG_HOME values are ignored.
    pub fn default_path() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(dir) if dir.is_absolute() => dir,
            _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
        };
        Some(base.join("clipmunge").join("config.lua"))
    }

    /// Resolve the given path on each load to follow replaced config symlinks.
    pub fn load(path: &Path) -> Result<Self> {
        let resolved = path
            .canonicalize()
            .with_context(|| format!("resolving {}", path.display()))?;
        let source = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()))?;
        Self::build(&source, path, &resolved)
    }

    /// Build from source without requiring a file or compositor in tests.
    /// Both paths determine the Lua module search directories.
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
        for (i, pair) in collected.sequence_values::<Table>().enumerate() {
            rules.push(build_rule(&pair.lua()?).with_context(|| format!("rule #{}", i + 1))?);
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

    /// Load a replacement engine, preserving runtime notification settings.
    pub fn reload(&self) -> Result<Self> {
        let mut fresh = Self::load(&self.path)?;
        fresh.notify_enabled = self.notify_enabled;
        Ok(fresh)
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

            // Captures follow the incoming selection, starting at group 1.
            // Unmatched optional groups become nil; group 0 is not passed.
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

            // Each handler gets its own budget; a slow rule must not exhaust the next one's.
            self.ticks.set(0);
            let called = self.lua.scope(|scope| {
                let sel = scope.create_userdata_ref(incoming)?;
                rule.handler
                    .call::<Value>((Value::UserData(sel), Variadic::from_iter(args)))
            });
            match called {
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
        mimes.iter().any(|m| {
            self.settings
                .secret_mimes
                .iter()
                .any(|s| s.eq_ignore_ascii_case(m))
        })
    }

    fn notify(&self, text: &str) {
        if !self.notify_enabled {
            return;
        }
        // Limit notification text, which may contain clipboard data.
        const MAX: usize = 200;
        let text: String = text.chars().take(MAX).collect();

        let argv = &self.settings.notify_command;
        let mut cmd = Command::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg.replace("{}", &text));
        }
        // Do not wait for notifications. SIGCHLD = SIG_IGN reaps the children;
        // dropping Child alone would leave zombies.
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

/// Load only the permitted libraries. Clearing globals would leave libraries
/// accessible through package.loaded. Exclude coroutine because the
/// instruction hook covers only its own thread.
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
    if let Err(e) = lua.set_memory_limit(MEMORY_LIMIT) {
        log::warn!("no memory limit on the rule interpreter: {e}");
    }
    Ok(())
}

/// Supported settings. Unknown keys produce a warning.
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
    // An empty list disables the secret MIME check.
    if let Some(mimes) = tbl.get::<Option<Vec<String>>>("secret_mimes").lua()? {
        settings.secret_mimes = mimes;
    }
    Ok(settings)
}

/// Read-only selection borrowed through Lua::scope. Access after the handler
/// returns fails, even if Lua retained the userdata.
impl UserData for Selection {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // Return the original plain text, without the trimming used for matching.
        methods.add_method("text", |_, sel, ()| Ok(sel.text().map(str::to_owned)));

        // Lua strings preserve arbitrary bytes, including non-UTF-8 payloads.
        methods.add_method("get", |lua, sel, mime: String| match sel.get(&mime) {
            Some(bytes) => Ok(Value::String(lua.create_string(bytes)?)),
            None => Ok(Value::Nil),
        });

        // Check presence without copying the payload into Lua.
        methods.add_method("has", |_, sel, mime: String| Ok(sel.has(&mime)));

        methods.add_method("mimes", |lua, sel, ()| {
            lua.create_sequence_from(sel.mimes().map(str::to_owned))
        });
    }
}

fn build_rule(spec: &Table) -> Result<Rule> {
    let name: String = spec
        .get::<Option<String>>("name")
        .lua()?
        .ok_or_else(|| anyhow!("rule has no `name`"))?;
    let pattern: String = spec
        .get::<Option<String>>("match")
        .lua()?
        .ok_or_else(|| anyhow!("rule '{name}' has no `match`"))?;
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

/// Convert a string or MIME-keyed table into a replacement selection.
/// `text` sets the plain-text family; `notify` is sent separately after publish.
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
    // Use stable MIME ordering instead of Lua table iteration order.
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

    let url = lua.create_table()?;
    url.set(
        "strip_params",
        lua.create_function(|lua, (target, list): (String, Option<Vec<String>>)| {
            let patterns =
                list.unwrap_or_else(|| DEFAULT_JUNK.iter().map(|s| s.to_string()).collect());
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

/// Disable load/loadfile/dofile, explicit GC and C loaders; set module search paths.
/// The library allowlist is set separately in `rule_stdlib`.
fn sandbox(lua: &Lua, given: &Path, resolved: &Path) -> mlua::Result<()> {
    let globals = lua.globals();
    // Remove direct code-loading functions and explicit GC control.
    for name in ["dofile", "loadfile", "load", "collectgarbage"] {
        globals.set(name, Value::Nil)?;
    }

    let package: Table = globals.get("package")?;
    // Disable C module loading.
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

/// Search beside both the given and resolved config paths.
/// Dotfiles repositories keep modules beside the target; config managers
/// may publish separate module symlinks beside the given path.
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

    /// Build a rule engine from a Lua source string.
    fn engine(source: &str) -> Result<Engine> {
        // Use a nonexistent directory so tests cannot load local Lua modules.
        let path = Path::new("/nonexistent/clipmunge/config.lua");
        Engine::build(source, path, path)
    }

    /// Extract the error without requiring Engine to implement Debug.
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
          handler = function(_, all) return "seen:" .. all end,
        }
    "#;

    #[test]
    fn reload_preserves_disabled_notifications() {
        let path = Path::new("config.lua.example");
        let mut engine = Engine::load(path).expect("example config should load");
        engine.set_notify(false);

        let reloaded = engine.reload().expect("config should reload");
        assert!(!reloaded.notify_enabled, "reload re-enabled notifications");
        assert_eq!(reloaded.rule_names(), engine.rule_names());
    }

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
                                handler = function(_) return "x" end }"#,
        )
        .expect("config should load");
        assert!(e.rewrite(&plain("not a number")).is_none());
    }

    #[test]
    fn the_first_matching_rule_wins_in_declaration_order() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "first",  match = [[^x$]],
                             handler = function(_) return "one" end }
            clipmunge.rule { name = "second", match = [[^x$]],
                             handler = function(_) return "two" end }
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
                             handler = function(_) return nil end }
            clipmunge.rule { name = "answers",  match = [[^x$]],
                             handler = function(_) return "second" end }
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
                             handler = function(_) error("boom") end }
            clipmunge.rule { name = "survives", match = [[^x$]],
                             handler = function(_) return "still here" end }
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
                             handler = function(_) return 42 end }
            clipmunge.rule { name = "returns-a-string", match = [[^x$]],
                             handler = function(_) return "ok" end }
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
        // Notifications describe a published rewrite; without a payload there is none.
        let mut e = engine(
            r#"clipmunge.rule { name = "chatty", match = [[^x$]],
                                handler = function(_) return { notify = "hi" } end }"#,
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
                 handler = function(_, one, two, three)
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
                                      handler = function(_) return "linked" end }"#;
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
                                handler = function(_) return "x" end }"#,
        );
        assert!(format!("{err:#}").contains("plainonly"), "{err:#}");
    }

    #[test]
    fn a_bad_pattern_fails_at_load_rather_than_on_some_later_copy() {
        let err = load_err(
            r#"clipmunge.rule { name = "unbalanced", match = [[^(x$]],
                                handler = function(_) return "x" end }"#,
        );
        assert!(format!("{err:#}").contains("unbalanced"), "{err:#}");
    }

    #[test]
    fn link_escapes_both_the_href_and_the_text() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "link",
                 match = [[^(.+)$]],
                 handler = function(_, s) return clipmunge.link("https://e.com/?a=1&b=2", s) end,
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

    /// Regression: clearing os/io globals left the libraries in package.loaded.
    #[test]
    fn require_cannot_resurrect_io_or_os() {
        for lib in ["io", "os", "debug"] {
            let src = format!(
                r#"local ok, m = pcall(require, "{lib}")
                   if ok and type(m) == "table" then error("{lib} is reachable") end
                   clipmunge.rule {{ name = "n", match = [[^x$]],
                                     handler = function(_) return "x" end }}"#
            );
            engine(&src).unwrap_or_else(|e| panic!("{lib} escaped the sandbox: {e:#}"));
        }
    }

    /// Regression: Lua table order changed the advertised MIME order between runs.
    #[test]
    fn the_advertised_order_is_canonical_not_lua_table_order() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "link",
                 match = [[^(.+)$]],
                 handler = function(_, s) return clipmunge.link("https://e.com/", s) end,
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

    /// A runaway handler must yield to the next rule within the instruction budget.
    #[test]
    fn a_runaway_handler_is_skipped_rather_than_hanging() {
        let mut e = engine(
            r#"
            clipmunge.rule { name = "spins",  match = [[^x$]],
                             handler = function(_) while true do end end }
            clipmunge.rule { name = "normal", match = [[^x$]],
                             handler = function(_) return "after the spin" end }
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
        // Each handler uses most of its budget, so sharing a counter would fail.
        let mut e = engine(
            r#"
            local function burn()
              local acc = 0
              for i = 1, 300000 do acc = acc + i end
              return acc
            end
            clipmunge.rule { name = "burn1", match = [[^x$]],
                             handler = function(_) burn() return nil end }
            clipmunge.rule { name = "burn2", match = [[^x$]],
                             handler = function(_) return "burnt " .. burn() end }
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
                 handler = function(_) return { text = "y", notify = "did a thing" } end,
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
    fn a_handler_can_read_the_flavours_that_arrived() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "inspect",
                 match = [[^(x)$]],
                 handler = function(incoming, cap)
                   return table.concat({
                     "cap=" .. cap,
                     "text=" .. tostring(incoming:text()),
                     "html=" .. tostring(incoming:get("text/html")),
                     "missing=" .. tostring(incoming:get("text/rtf")),
                     "has-html=" .. tostring(incoming:has("text/html")),
                     "n=" .. #incoming:mimes(),
                   }, " ")
                 end,
               }"#,
        )
        .expect("config should load");

        let mut sel = plain("x");
        sel.set(HTML_MIME, b"<b>x</b>".to_vec());
        let out = e.rewrite(&sel).expect("should match");
        assert_eq!(
            text_of(&out),
            "cap=x text=x html=<b>x</b> missing=nil has-html=true n=6"
        );
    }

    /// A handler must not retain access to borrowed selection data after returning.
    #[test]
    fn the_incoming_selection_does_not_outlive_the_call() {
        let mut e = engine(
            r#"
            local stashed = nil
            clipmunge.rule {
              name = "stash",
              match = [[^first$]],
              handler = function(incoming) stashed = incoming; return "stashed" end,
            }
            clipmunge.rule {
              name = "reuse",
              match = [[^second$]],
              handler = function()
                local ok, err = pcall(function() return stashed:text() end)
                if ok then error("the stale userdata was readable") end
                return "expired"
              end,
            }
            "#,
        )
        .expect("config should load");

        assert_eq!(
            text_of(&e.rewrite(&plain("first")).expect("first")),
            "stashed"
        );
        assert_eq!(
            text_of(&e.rewrite(&plain("second")).expect("second")),
            "expired"
        );
    }

    #[test]
    fn a_handler_cannot_write_to_the_incoming_selection() {
        let mut e = engine(
            r#"clipmunge.rule {
                 name = "tamper",
                 match = [[^x$]],
                 handler = function(incoming)
                   local ok = pcall(function() incoming.text = 42 end)
                   return "writable=" .. tostring(ok)
                 end,
               }"#,
        )
        .expect("config should load");
        let out = e.rewrite(&plain("x")).expect("should match");
        assert_eq!(text_of(&out), "writable=false");
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
    fn the_secret_hint_matches_regardless_of_case() {
        let e = engine(ECHO).expect("config should load");
        assert!(e.is_secret(&mimes(&["text/plain", "X-KDE-PasswordManagerHint"])));
        assert!(e.is_secret(&mimes(&["text/plain", "X-KDE-PASSWORDMANAGERHINT"])));
    }

    #[test]
    fn secret_mimes_replaces_the_default_rather_than_extending_it() {
        let e = engine(
            r#"clipmunge.settings { secret_mimes = { "x-vault-secret" } }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function(_) return "x" end }"#,
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
        let e = engine(
            r#"clipmunge.settings { secret_mimes = {} }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function(_) return "x" end }"#,
        )
        .expect("an empty secret_mimes is a decision, not an error");
        assert!(!e.is_secret(&mimes(&["text/plain", "x-kde-passwordManagerHint"])));
    }

    #[test]
    fn an_unknown_settings_key_is_a_warning_and_not_a_failure() {
        let e = engine(
            r#"clipmunge.settings { notifi_command = { "true" } }
               clipmunge.rule { name = "n", match = [[^x$]],
                                handler = function(_) return "x" end }"#,
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
        load_err(r#"clipmunge.rule { name = "no-match", handler = function(_) return "x" end }"#);
        load_err(r#"clipmunge.rule { name = "no-handler", match = [[^x$]] }"#);
    }

    #[test]
    fn a_rule_without_a_name_fails_the_load() {
        let err =
            load_err(r#"clipmunge.rule { match = [[^x$]], handler = function(_) return "x" end }"#);
        let msg = format!("{err:#}");
        assert!(msg.contains("`name`"), "{msg}");
        // Identify the unnamed rule by declaration order.
        assert!(msg.contains("rule #1"), "{msg}");
    }
}
