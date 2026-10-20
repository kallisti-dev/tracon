//! `tracon check`: diagnose the tmux and Claude Code halves of the setup.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::Result;
use serde_json::Value;

use tracon::backend::tmux::setup;
use tracon::backend::{Backend, TmuxBackend};
use tracon::store::TmuxStore;
use tracon::tmux::Tmux;
use tracon::{Config, State};

use crate::cli::{VERSION, entries};

#[derive(Default)]
struct Report {
    problems: usize,
}

impl Report {
    fn ok(&self, message: impl AsRef<str>) {
        println!("ok       {}", message.as_ref());
    }

    fn info(&self, message: impl AsRef<str>) {
        println!("info     {}", message.as_ref());
    }

    fn problem(&mut self, message: impl AsRef<str>) {
        println!("problem  {}", message.as_ref());
        self.problems += 1;
    }
}

/// Whether a version such as `3.7c` or `next-3.6` is at least `major.minor`.
fn at_least(version: &str, major: u32, minor: u32) -> bool {
    let digits: String = version
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let have = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    have >= (major, minor)
}

pub fn run(
    tmux: &Tmux,
    backend: &TmuxBackend,
    store: &TmuxStore,
    session: Option<&str>,
) -> Result<ExitCode> {
    let mut report = Report::default();
    if !Tmux::inside() {
        report.problem("not running inside tmux, so there is no server to check");
        return Ok(ExitCode::FAILURE);
    }

    let version = tmux.run(["-V"])?;
    let version = version.trim();
    if at_least(version, 3, 2) {
        report.ok(version);
    } else {
        report.problem(format!(
            "{version} is older than tmux 3.2, which has popups"
        ));
    }

    let installed = setup::installed(tmux)?;
    if installed.binary.is_empty() {
        report.problem("tracon is not set up in this tmux server; check that the tmux plugin loads, then reload the configuration");
    } else {
        report.ok(format!(
            "tracon {} is set up from {}",
            installed.version, installed.binary
        ));
        if installed.version != VERSION {
            report.problem(format!(
                "tmux runs tracon {}, but this is {VERSION}; reload the tmux configuration",
                installed.version
            ));
        }
        if !Path::new(&installed.binary).exists() {
            report.problem(format!(
                "{} no longer exists; reload the tmux configuration",
                installed.binary
            ));
        }
        if installed.key_bound {
            report.ok(format!("prefix {} opens the picker", installed.key));
        } else {
            report.problem(format!(
                "prefix {} no longer opens the picker; something rebound it after tracon loaded",
                installed.key
            ));
        }
        if installed.focus_hook {
            report.ok("focusing a finished pane marks it seen");
        } else {
            report.problem("the pane-focus-in hook is missing; reload the tmux configuration");
        }
    }
    if tmux.global_option("focus-events")? != "on" {
        report.problem(
            "focus-events is off, so tmux doesn't report focus and finished panes stay done",
        );
    }
    if let Err(error) = Config::from_tmux(tmux) {
        report.problem(format!("@tracon-order is invalid: {error}"));
    }

    check_claude(&mut report);

    let entries = entries(tmux, backend, store)?;
    let silent: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry
                .state
                .as_ref()
                .is_some_and(|state| state.as_str() == State::UNKNOWN)
        })
        .map(|entry| entry.location.as_str())
        .collect();
    if silent.is_empty() {
        report.ok("every pane running Claude Code has reported its state");
    } else {
        report.problem(format!(
            "Claude Code in {} has reported no state; restart it there, for example with claude --continue",
            silent.join(", ")
        ));
    }

    if let (Some(session), Some(pane)) = (session, backend.locate()) {
        let recorded = tracon::store::Store::load(store)?
            .into_iter()
            .find(|(locator, _)| *locator == pane)
            .and_then(|(_, record)| record.session);
        if recorded.as_deref() == Some(session) {
            report.ok(format!("this pane records session {session}"));
        } else {
            report.problem(format!(
                "this pane records {}, not this session ({session})",
                recorded.map_or("no session".to_owned(), |id| format!("session {id}"))
            ));
        }
    }

    if report.problems == 0 {
        println!("No problems found.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "{} problem(s) found; see docs/troubleshooting.md in the plugin directory.",
            report.problems
        );
        Ok(ExitCode::FAILURE)
    }
}

fn check_claude(report: &mut Report) {
    let Ok(output) = Command::new("claude")
        .args(["plugin", "list", "--json"])
        .output()
    else {
        report.info("claude is not on PATH, so its plugins were not checked");
        return;
    };
    let plugins: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    let found = plugins.as_array().into_iter().flatten().find(|plugin| {
        plugin["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("tracon@"))
            && plugin["enabled"].as_bool().unwrap_or(false)
    });
    match found {
        None => report.problem("Claude Code doesn't load the tracon plugin; install the Claude plugin with the tmux plugin"),
        Some(plugin) => {
            let id = plugin["id"].as_str().unwrap_or("tracon");
            let version = plugin["version"].as_str().unwrap_or("unknown");
            if version == VERSION {
                report.ok(format!("Claude Code loads the plugin as {id}"));
            } else {
                report.problem(format!(
                    "Claude Code loads {id} {version}, but this is tracon {VERSION}; install the same version of both plugins"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(at_least("tmux 3.7c", 3, 2));
        assert!(at_least("tmux next-3.6", 3, 2));
        assert!(at_least("tmux 3.10", 3, 2));
        assert!(!at_least("tmux 3.1c", 3, 2));
        assert!(!at_least("tmux 2.9", 3, 2));
    }
}
