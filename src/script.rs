//! The configuration file: Lua at `$XDG_CONFIG_HOME/tracon/init.lua`, run
//! by the daemon when it starts and on `tracon reload`, never in a hook's
//! process. `require("tracon")` gives the module in `script/tracon.lua`,
//! whose `setup` takes the settings the tmux options also set, and functions
//! in place of the built-in status function and label tables. A file that
//! fails leaves the defaults and is reported by `tracon check`; a function
//! that fails is reported the same way, and the built-in one answers.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mlua::serde::SerializeOptions;
use mlua::{Function, Lua, LuaSerdeExt, Table, Value};

use crate::config::{Config, Functions};
use crate::format::Style;
use crate::label;
use crate::list;
use crate::protocol::ConfigFile;
use crate::screen::Sighting;
use crate::state::Record;
use crate::status::{self, Status, parse_order};
use crate::summary;

const MODULE: &str = include_str!("script/tracon.lua");
pub const FILE: &str = "init.lua";

/// A failure to report, as one line.
type Fault = Arc<Mutex<Option<String>>>;

/// What `setup` collects, kept in the Lua state while the file runs and for
/// the built-in functions the module exposes.
#[derive(Default)]
struct Setup {
    config: Config,
    status: Option<Function>,
    labels: Option<Function>,
    icons: Option<Function>,
    styles: Option<Function>,
}

/// The configuration file, as last run.
pub struct Script {
    path: Option<PathBuf>,
    found: bool,
    config: Config,
    /// What stopped the file.
    error: Option<String>,
    /// The last failure of one of its functions.
    fault: Fault,
}

impl Script {
    /// Where the file is looked for: `$XDG_CONFIG_HOME/tracon/init.lua`,
    /// else `~/.config/tracon/init.lua`; `None` without a home.
    pub fn path() -> Option<PathBuf> {
        let var = |name| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let base =
            var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|home| home.join(".config")))?;
        Some(base.join("tracon").join(FILE))
    }

    /// Run the file at `path`, if there is one. A file that fails is logged
    /// and leaves the defaults.
    pub fn load(path: Option<PathBuf>) -> Script {
        let mut script = Script {
            path,
            found: false,
            config: Config::default(),
            error: None,
            fault: Fault::default(),
        };
        let Some(path) = script.path.clone() else {
            return script;
        };
        let code = match std::fs::read_to_string(&path) {
            Ok(code) => code,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return script,
            Err(error) => {
                script.found = true;
                script.error = Some(format!("{}: {error}", path.display()));
                return script;
            }
        };
        script.found = true;
        match run(&path, &code, Arc::clone(&script.fault)) {
            Ok(config) => script.config = config,
            Err(error) => {
                eprintln!("tracon: {}: {error}", path.display());
                script.error = Some(error);
            }
        }
        script
    }

    /// The configuration the file set up over the defaults.
    pub fn config(&self) -> Config {
        self.config.clone()
    }

    /// The file and its trouble, for the check and `tracon reload`.
    pub fn report(&self) -> Option<ConfigFile> {
        let path = self.path.as_ref()?;
        let fault = lock(&self.fault).clone();
        Some(ConfigFile {
            path: path.display().to_string(),
            found: self.found,
            error: self.error.clone().or(fault),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `code` from `path` in a fresh Lua state, and return what it set up.
fn run(path: &Path, code: &str, fault: Fault) -> Result<Config, String> {
    let lua = Lua::new();
    lua.set_app_data(Setup::default());
    install(&lua, path).map_err(|error| message(&error))?;
    lua.load(code)
        .set_name(format!("@{}", path.display()))
        .exec()
        .map_err(|error| message(&error))?;
    let setup = lua
        .app_data_ref::<Setup>()
        .ok_or_else(|| "the setup went missing".to_owned())?;
    let mut config = setup.config.clone();
    let scripted = Scripted {
        path: path.display().to_string(),
        status: setup.status.clone(),
        labels: setup.labels.clone(),
        icons: setup.icons.clone(),
        styles: setup.styles.clone(),
        fault,
        lua: lua.clone(),
    };
    drop(setup);
    if scripted.defines_any() {
        config.functions = Some(Arc::new(scripted));
    }
    Ok(config)
}

/// Make `require("tracon")` give the module, with the file's directory on
/// the module path for the user's own modules.
fn install(lua: &Lua, path: &Path) -> mlua::Result<()> {
    let native = lua.create_table()?;
    native.set("configure", lua.create_function(configure)?)?;
    native.set(
        "status",
        lua.create_function(|lua, pane: Table| {
            let (record, sighting) = pane_of(lua, &pane)?;
            Ok(status::status(&record, sighting).map(|status| status.to_string()))
        })?,
    )?;
    native.set(
        "label",
        lua.create_function(|lua, s| shown(lua, s, |c| &c.labels))?,
    )?;
    native.set(
        "icon",
        lua.create_function(|lua, s| shown(lua, s, |c| &c.icons))?,
    )?;
    native.set(
        "style",
        lua.create_function(|lua, s| shown(lua, s, |c| &c.styles))?,
    )?;
    let module: Table = lua.load(MODULE).set_name("=tracon").call(native)?;
    let package: Table = lua.globals().get("package")?;
    if let Some(dir) = path.parent() {
        let dir = dir.display();
        let current: String = package.get("path")?;
        package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua;{current}"))?;
    }
    package.get::<Table>("loaded")?.set("tracon", module)?;
    Ok(())
}

/// The built-in `g` for one table, from the tables as set up so far.
fn shown(
    lua: &Lua,
    status: String,
    table: fn(&Config) -> &label::Table,
) -> mlua::Result<Option<String>> {
    let status: Status = status.parse().map_err(mlua::Error::external)?;
    let setup = lua
        .app_data_ref::<Setup>()
        .ok_or_else(|| mlua::Error::runtime("the setup went missing"))?;
    Ok(table(&setup.config).get(&status).map(str::to_owned))
}

/// `setup`'s native half: apply `options`, already checked for their keys
/// and types, or say what is wrong with a value.
fn configure(lua: &Lua, options: Table) -> mlua::Result<(bool, Option<String>)> {
    let mut setup = lua
        .app_data_mut::<Setup>()
        .ok_or_else(|| mlua::Error::runtime("the setup went missing"))?;
    match apply(&mut setup, &options) {
        Ok(()) => Ok((true, None)),
        Err(reason) => Ok((false, Some(reason))),
    }
}

fn apply(setup: &mut Setup, options: &Table) -> Result<(), String> {
    for pair in options.pairs::<String, Value>() {
        let (key, value) = pair.map_err(|error| message(&error))?;
        let config = &mut setup.config;
        match key.as_str() {
            "order" => config.order = rules(&key, &value)?,
            "recent_first" => config.recent_first = rules(&key, &value)?,
            "preview_interval" => {
                let milliseconds = match value {
                    Value::Integer(n) if n > 0 => n as u64,
                    Value::Number(n) if n >= 1.0 => n as u64,
                    _ => return Err(format!("{key} takes a positive number of milliseconds")),
                };
                config.preview_interval = Duration::from_millis(milliseconds);
            }
            "format" => config.format = template(&key, &value, &list::VARIABLES)?,
            "labels" => match value {
                Value::Function(function) => setup.labels = Some(function),
                other => config.labels = entries(&key, config.labels.clone(), &other)?,
            },
            "icons" => match value {
                Value::Function(function) => setup.icons = Some(function),
                other => config.icons = entries(&key, config.icons.clone(), &other)?,
            },
            "styles" => match value {
                Value::Function(function) => setup.styles = Some(function),
                other => {
                    let styles = entries(&key, config.styles.clone(), &other)?;
                    for style in styles.values() {
                        Style::parse(style).map_err(|error| format!("invalid {key}: {error}"))?;
                    }
                    config.styles = styles;
                }
            },
            "status" => match value {
                Value::Function(function) => setup.status = Some(function),
                _ => return Err(format!("{key} takes a function")),
            },
            "summary" => {
                let Value::Table(table) = value else {
                    return Err(format!("{key} takes a table"));
                };
                for pair in table.pairs::<String, Value>() {
                    let (field, value) = pair.map_err(|error| message(&error))?;
                    let name = format!("{key}.{field}");
                    match field.as_str() {
                        "format" => {
                            config.summary.format = template(&name, &value, &summary::VARIABLES)?;
                        }
                        "separator" => config.summary.separator = text(&name, &value)?,
                        _ => return Err(format!("{key} has no key {field}")),
                    }
                }
            }
            _ => return Err(format!("setup has no key {key}")),
        }
    }
    Ok(())
}

fn text(name: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.to_string_lossy()),
        _ => Err(format!("{name} takes a string")),
    }
}

/// An order, as a string of names or a list of them.
fn rules(name: &str, value: &Value) -> Result<Vec<status::Rule>, String> {
    let names = match value {
        Value::Table(list) => {
            let mut names = Vec::new();
            for item in list.sequence_values::<Value>() {
                let item = item.map_err(|error| message(&error))?;
                names.push(text(name, &item)?);
            }
            names.join(" ")
        }
        other => text(name, other)?,
    };
    parse_order(&names).map_err(|error| format!("invalid {name}: {error}"))
}

fn template(
    name: &str,
    value: &Value,
    variables: &[&str],
) -> Result<crate::format::Template, String> {
    let source = text(name, value)?;
    let template = crate::format::Template::parse(&source)
        .map_err(|error| format!("invalid {name}: {error}"))?;
    if let Some(unknown) = template
        .variables()
        .into_iter()
        .find(|variable| !variables.contains(variable))
    {
        return Err(format!("invalid {name}: unknown variable {{{unknown}}}"));
    }
    Ok(template)
}

/// `table` with the entries of a Lua table of status to text added.
fn entries(name: &str, mut table: label::Table, value: &Value) -> Result<label::Table, String> {
    let Value::Table(overrides) = value else {
        return Err(format!("{name} takes a table or a function"));
    };
    for pair in overrides.pairs::<Value, Value>() {
        let (status, text) = pair.map_err(|error| message(&error))?;
        let Value::String(status) = status else {
            return Err(format!("{name} is keyed by status names"));
        };
        let status: Status = status
            .to_string_lossy()
            .parse()
            .map_err(|error| format!("invalid {name}: {error}"))?;
        let Value::String(text) = text else {
            return Err(format!("{name}.{status} takes a string"));
        };
        table.set(status, text.to_string_lossy());
    }
    Ok(table)
}

const PLAIN: SerializeOptions = SerializeOptions::new()
    .serialize_none_to_null(false)
    .serialize_unit_to_null(false);

/// The pane a status function sees: the record's fields, whether the pane
/// was seen, and the sighting.
fn pane_table(lua: &Lua, record: &Record, sighting: Option<Sighting>) -> mlua::Result<Table> {
    let Value::Table(pane) = lua.to_value_with(record, PLAIN)? else {
        return Err(mlua::Error::runtime("a record is a table"));
    };
    pane.set("seen", record.seen())?;
    pane.set("sighting", lua.to_value_with(&sighting, PLAIN)?)?;
    Ok(pane)
}

/// A pane table back into a record and a sighting, for the built-in `f`.
fn pane_of(lua: &Lua, pane: &Table) -> mlua::Result<(Record, Option<Sighting>)> {
    let record: Record = lua.from_value(Value::Table(pane.clone()))?;
    let sighting: Option<Sighting> = lua.from_value(pane.get("sighting")?)?;
    Ok((record, sighting))
}

/// One line for a Lua error: the message, without mlua's prefix or the
/// stack traceback.
fn message(error: &mlua::Error) -> String {
    let text = match error {
        mlua::Error::RuntimeError(text) | mlua::Error::MemoryError(text) => text.clone(),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::CallbackError { cause, .. } => return message(cause),
        other => other.to_string(),
    };
    text.split("\nstack traceback:")
        .next()
        .unwrap_or(&text)
        .trim()
        .to_owned()
}

/// The file's functions, called from the list.
struct Scripted {
    path: String,
    status: Option<Function>,
    labels: Option<Function>,
    icons: Option<Function>,
    styles: Option<Function>,
    fault: Fault,
    /// Keeps the state the functions live in.
    lua: Lua,
}

impl Scripted {
    fn defines_any(&self) -> bool {
        self.status.is_some()
            || self.labels.is_some()
            || self.icons.is_some()
            || self.styles.is_some()
    }

    /// Call `function`, or record what went wrong, once per distinct
    /// failure in the log and always for the check.
    fn call<R: mlua::FromLuaMulti>(
        &self,
        what: &str,
        function: &Function,
        args: impl mlua::IntoLuaMulti,
    ) -> Option<R> {
        match function.call(args) {
            Ok(value) => {
                *lock(&self.fault) = None;
                Some(value)
            }
            Err(error) => {
                self.failed(format!("{what}: {}", message(&error)));
                None
            }
        }
    }

    fn failed(&self, text: String) {
        let mut fault = lock(&self.fault);
        if fault.as_deref() != Some(&text) {
            eprintln!("tracon: {}: {text}", self.path);
        }
        *fault = Some(text);
    }

    fn shown(&self, what: &str, function: Option<&Function>, status: &Status) -> Option<String> {
        self.call::<Option<String>>(what, function?, status.as_str())
            .flatten()
    }
}

impl Functions for Scripted {
    fn status(&self, record: &Record, sighting: Option<Sighting>) -> Option<Status> {
        let builtin = || status::status(record, sighting);
        let Some(function) = &self.status else {
            return builtin();
        };
        let pane = match pane_table(&self.lua, record, sighting) {
            Ok(pane) => pane,
            Err(error) => {
                self.failed(format!("status: {}", message(&error)));
                return builtin();
            }
        };
        match self.call::<Option<String>>("status", function, pane) {
            Some(Some(name)) => match name.parse::<Status>() {
                Ok(status) => Some(status),
                Err(error) => {
                    self.failed(format!("status: {error}"));
                    builtin()
                }
            },
            Some(None) => None,
            None => builtin(),
        }
    }

    fn label(&self, status: &Status) -> Option<String> {
        self.shown("labels", self.labels.as_ref(), status)
    }

    fn icon(&self, status: &Status) -> Option<String> {
        self.shown("icons", self.icons.as_ref(), status)
    }

    fn style(&self, status: &Status) -> Option<String> {
        self.shown("styles", self.styles.as_ref(), status)
    }
}

impl fmt::Debug for Scripted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scripted")
            .field("path", &self.path)
            .field("status", &self.status.is_some())
            .field("labels", &self.labels.is_some())
            .field("icons", &self.icons.is_some())
            .field("styles", &self.styles.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::plain;

    /// A script loaded from `code` in a directory of its own.
    fn load(code: &str) -> (Script, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, code).unwrap();
        (Script::load(Some(path)), dir)
    }

    fn working(message: &str) -> Record {
        Record {
            state: Some("working".parse().unwrap()),
            message: Some(message.to_owned()),
            harness: Some("claude".into()),
            ..Record::default()
        }
    }

    #[test]
    fn setup_sets_settings_and_tables() {
        let (script, _dir) = load(
            r#"
            local tracon = require("tracon")
            tracon.setup({
              order = { "pinned", "waiting" },
              recent_first = "idle",
              preview_interval = 250,
              format = "{icon} {label}",
              labels = { ["waiting.permission"] = "approve?", review = "in review" },
              icons = { waiting = "!" },
              styles = { done = "bold green" },
              summary = { format = "{count} {label}", separator = ", " },
            })
            tracon.setup({ preview_interval = 500 })
            "#,
        );
        let report = script.report().unwrap();
        assert!(report.found && report.error.is_none(), "{report:?}");
        let config = script.config();
        assert_eq!(config.order, parse_order("pinned waiting").unwrap());
        assert_eq!(config.recent_first, parse_order("idle").unwrap());
        assert_eq!(config.preview_interval, Duration::from_millis(500));
        assert_eq!(config.format.source(), "{icon} {label}");
        let permission: Status = "waiting.permission".parse().unwrap();
        assert_eq!(config.label(&permission), "approve?");
        assert_eq!(config.label(&"review".parse().unwrap()), "in review");
        assert_eq!(config.icon(&permission), "!");
        assert_eq!(config.style(&"done".parse().unwrap()), "bold green");
        assert_eq!(config.summary.format.source(), "{count} {label}");
        assert_eq!(config.summary.separator, ", ");
        assert!(config.functions.is_none());
    }

    #[test]
    fn functions_replace_the_built_in_ones_and_fall_back_to_them() {
        let (script, _dir) = load(
            r#"
            local tracon = require("tracon")
            tracon.setup({
              status = function(pane)
                if pane.state == "working" and pane.message:sub(1, 1) == "!" then
                  return "working.shell"
                end
                return tracon.status(pane)
              end,
              labels = function(status)
                if status == "working.shell" then return "in a shell" end
              end,
              icons = { ["working.shell"] = "$" },
              styles = function(status)
                return tracon.style(status) and (tracon.style(status) .. " italic")
              end,
            })
            "#,
        );
        assert!(script.report().unwrap().error.is_none());
        let config = script.config();
        let shell = config.status(&working("! make"), None).unwrap();
        assert_eq!(shell.as_str(), "working.shell");
        assert_eq!(config.label(&shell), "in a shell");
        assert_eq!(config.icon(&shell), "$");
        assert_eq!(config.style(&shell), "blue italic");
        let plain_turn = config.status(&working("fix it"), None).unwrap();
        assert_eq!(plain_turn.as_str(), "working");
        assert_eq!(config.label(&plain_turn), "working");
        // The built-in function sees the sighting and the harness.
        let waiting = Record {
            state: Some("waiting.permission".parse().unwrap()),
            ..working("Bash: ls")
        };
        assert_eq!(
            config
                .status(&waiting, Some(Sighting::Working))
                .unwrap()
                .as_str(),
            "working"
        );
        let silent = Record {
            state: None,
            ..working("")
        };
        assert_eq!(config.status(&silent, None).unwrap().as_str(), "unknown");
        assert_eq!(config.status(&Record::default(), None), None);
    }

    #[test]
    fn a_failing_function_is_reported_and_the_built_in_one_answers() {
        let (script, _dir) = load(
            r#"
            require("tracon").setup({
              status = function(pane) return pane.message.oops.deeper end,
              labels = function(status) return "Label" end,
            })
            "#,
        );
        let config = script.config();
        let status = config.status(&working("x"), None).unwrap();
        assert_eq!(status.as_str(), "working");
        let error = script.report().unwrap().error.unwrap();
        assert!(
            error.starts_with("status: ") && error.contains(":3:"),
            "{error}"
        );
        assert!(!error.contains("traceback"), "{error}");
        assert_eq!(config.label(&status), "Label");
        // A bad status name is a failure too.
        let (script, _dir) =
            load(r#"require("tracon").setup({ status = function() return "Done" end })"#);
        let config = script.config();
        assert_eq!(
            config.status(&working("x"), None).unwrap().as_str(),
            "working"
        );
        assert_eq!(
            script.report().unwrap().error.as_deref(),
            Some("status: invalid status \"Done\"")
        );
    }

    #[test]
    fn mistakes_are_reported_with_their_line() {
        let cases = [
            (
                "require('tracon').setup({ colour = 'red' })",
                "setup has no key colour",
            ),
            (
                "require('tracon').setup({ format = 7 })",
                "setup.format takes a string, not number",
            ),
            (
                "require('tracon').setup('fast')",
                "setup takes a table, not string",
            ),
            (
                "require('tracon').setup({ order = 'waiting Bad' })",
                "invalid order: invalid status \"Bad\"",
            ),
            (
                "require('tracon').setup({ format = '{colour}' })",
                "invalid format: unknown variable {colour}",
            ),
            (
                "require('tracon').setup({ styles = { done = 'blod' } })",
                "invalid styles: invalid style \"blod\"",
            ),
            (
                "require('tracon').setup({ summary = { sep = ' ' } })",
                "setup.summary has no key sep",
            ),
            (
                "require('tracon').setup({ preview_interval = 0 })",
                "preview_interval takes a positive number of milliseconds",
            ),
            ("local x = ", "unexpected symbol near <eof>"),
            ("error('no')", "no"),
        ];
        for (code, expected) in cases {
            let (script, _dir) = load(code);
            let report = script.report().unwrap();
            let error = report.error.unwrap_or_default();
            assert!(
                error.ends_with(&format!(":1: {expected}")),
                "{code}: {error}"
            );
            // The defaults stand.
            assert_eq!(
                script.config().format.source(),
                crate::config::DEFAULT_FORMAT
            );
        }
    }

    #[test]
    fn the_files_directory_is_on_the_module_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mine.lua"), "return { format = '{label}' }").unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "require('tracon').setup(require('mine'))").unwrap();
        let script = Script::load(Some(path));
        assert!(script.report().unwrap().error.is_none());
        let config = script.config();
        let row = config.format.render(&|_| Some("done".to_owned()));
        assert_eq!(plain(&row), "done");
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::load(Some(dir.path().join(FILE)));
        let report = script.report().unwrap();
        assert!(!report.found && report.error.is_none());
        assert!(Script::load(None).report().is_none());
    }
}
