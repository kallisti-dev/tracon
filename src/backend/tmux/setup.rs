//! Register tracon with a tmux server: the picker key, the focus hook and
//! the option mirror.
//!
//! Setup is idempotent and reversible, and serialised with `wait-for -L` so
//! concurrent reloads don't interleave. Every registered command checks that
//! the binary still exists, because tmux keeps registrations until the
//! server exits, even after an uninstall deletes the files.

use std::path::Path;

use super::mirror::PID_OPTION;
use crate::Error;
use crate::tmux::{self, Tmux};

pub const KEY_OPTION: &str = "@tracon-key";
pub const DEFAULT_KEY: &str = "a";
const BOUND_KEY: &str = "@tracon-bound-key";
const DIR: &str = "@tracon-dir";
const BIN: &str = "@tracon-bin";
const VERSION: &str = "@tracon-version";
const LOCK: &str = "tracon-setup";

/// The hook that marks a finished pane seen when it gains focus.
pub const FOCUS_HOOK: &str = "pane-focus-in";
/// The preferred index in the hook array; re-sourcing replaces rather than
/// appends because the index is fixed.
const FOCUS_SLOT: u32 = 80;

/// Text every tracon handler contains, to recognise one installed from
/// another path.
const SEEN_MARKER: &str = "tracon seen";
const POPUP_MARKER: &str = "tracon popup";

/// The mirror's process id, when one is recorded and the process runs.
fn mirror_pid(tmux: &Tmux) -> Result<Option<u32>, Error> {
    let pid = tmux.global_option(PID_OPTION)?;
    Ok(pid.trim().parse::<u32>().ok().filter(|&pid| {
        // SAFETY: signal 0 checks for the process without sending anything.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }))
}

/// End the mirror this server recorded, if it still runs.
fn stop_mirror(tmux: &Tmux) -> Result<(), Error> {
    if let Some(pid) = mirror_pid(tmux)? {
        // SAFETY: the pid was recorded by a mirror of this server.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    tmux.run(["set-option", "-gu", PID_OPTION])?;
    Ok(())
}

struct Lock<'a>(&'a Tmux);

impl<'a> Lock<'a> {
    fn take(tmux: &'a Tmux) -> Result<Self, Error> {
        tmux.run(["wait-for", "-L", LOCK])?;
        Ok(Lock(tmux))
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(["wait-for", "-U", LOCK]);
    }
}

/// A shell command for `run-shell` that runs `binary` with `args`, or does
/// nothing once `binary` is gone. `args` may contain formats for run-shell
/// to expand.
fn guarded(binary: &Path, args: &str) -> String {
    let binary = tmux::escape_format(&tmux::shell_quote(&binary.to_string_lossy()));
    format!("[ ! -e {binary} ] || {binary} {args}")
}

/// Entries of a global hook array as (index, command) pairs, from
/// `show-hooks -g NAME` lines such as `pane-focus-in[80] run-shell ...`.
fn hook_entries(tmux: &Tmux, name: &str) -> Result<Vec<(u32, String)>, Error> {
    let output = tmux.run(["show-hooks", "-g", name])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix('[')?;
            let (index, command) = rest.split_once(']')?;
            Some((index.parse().ok()?, command.trim().to_owned()))
        })
        .collect())
}

fn remove_focus_hooks(tmux: &Tmux) -> Result<Vec<u32>, Error> {
    let mut used = Vec::new();
    for (index, command) in hook_entries(tmux, FOCUS_HOOK)? {
        if command.contains(SEEN_MARKER) {
            tmux.run(["set-hook", "-gu", &format!("{FOCUS_HOOK}[{index}]")])?;
        } else {
            used.push(index);
        }
    }
    Ok(used)
}

/// The `list-keys` line binding `key` in the prefix table, or an empty
/// string. Listing one key by name prints nothing in tmux 3.7, so this reads
/// the table, whose lines look like `bind-key -r -T prefix a run-shell ...`.
fn binding(tmux: &Tmux, key: &str) -> Result<String, Error> {
    let output = tmux.run(["list-keys", "-T", "prefix"])?;
    Ok(output
        .lines()
        .find(|line| {
            let mut words = line.split_whitespace().skip_while(|word| *word != "-T");
            words.nth(2) == Some(key)
        })
        .unwrap_or_default()
        .to_owned())
}

fn unbind_if_ours(tmux: &Tmux, key: &str) -> Result<(), Error> {
    if !key.is_empty() && binding(tmux, key)?.contains(POPUP_MARKER) {
        tmux.run(["unbind-key", "-T", "prefix", key])?;
    }
    Ok(())
}

/// Bind the picker key, register the focus hook and record where tracon is.
/// `dir` is the tmux plugin's directory, when setup runs from one.
pub fn install(tmux: &Tmux, binary: &Path, dir: Option<&Path>, version: &str) -> Result<(), Error> {
    let _lock = Lock::take(tmux)?;
    let key = Some(tmux.global_option(KEY_OPTION)?)
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| DEFAULT_KEY.to_owned());
    let previous = tmux.global_option(BOUND_KEY)?;
    if previous != key {
        unbind_if_ours(tmux, &previous)?;
    }
    tmux.run([
        "bind-key",
        "-T",
        "prefix",
        &key,
        "run-shell",
        "-b",
        &guarded(binary, "popup '#{client_name}'"),
    ])?;

    let used = remove_focus_hooks(tmux)?;
    let slot = (FOCUS_SLOT..)
        .find(|slot| !used.contains(slot))
        .unwrap_or(FOCUS_SLOT);
    tmux.run([
        "set-hook",
        "-g",
        &format!("{FOCUS_HOOK}[{slot}]"),
        &format!(
            "run-shell -b {}",
            tmux::quote(&guarded(binary, "seen '#{pane_id}'"))
        ),
    ])?;

    // A mirror from another binary keeps running old code; the lock stops
    // a second mirror of the same one.
    if tmux.global_option(BIN)? != binary.to_string_lossy() {
        stop_mirror(tmux)?;
    }
    tmux.run(["run-shell", "-b", &guarded(binary, "mirror")])?;

    tmux.run(["set-option", "-g", BOUND_KEY, &key])?;
    tmux.run(["set-option", "-g", BIN, &binary.to_string_lossy()])?;
    tmux.run(["set-option", "-g", VERSION, version])?;
    match dir {
        Some(dir) => tmux.run(["set-option", "-g", DIR, &dir.to_string_lossy()])?,
        None => tmux.run(["set-option", "-gu", DIR])?,
    };
    Ok(())
}

/// Undo `install`, leaving bindings and hooks that something else replaced.
pub fn remove(tmux: &Tmux) -> Result<(), Error> {
    let _lock = Lock::take(tmux)?;
    unbind_if_ours(tmux, &tmux.global_option(BOUND_KEY)?)?;
    remove_focus_hooks(tmux)?;
    stop_mirror(tmux)?;
    for option in [BOUND_KEY, BIN, VERSION, DIR] {
        tmux.run(["set-option", "-gu", option])?;
    }
    Ok(())
}

/// What `install` recorded, for the check.
#[derive(Debug, Default)]
pub struct Installed {
    pub binary: String,
    pub version: String,
    pub dir: String,
    pub key: String,
    /// The key's binding still opens the picker.
    pub key_bound: bool,
    pub focus_hook: bool,
    /// The option mirror runs.
    pub mirror: bool,
}

pub fn installed(tmux: &Tmux) -> Result<Installed, Error> {
    let key = tmux.global_option(BOUND_KEY)?;
    Ok(Installed {
        binary: tmux.global_option(BIN)?,
        version: tmux.global_option(VERSION)?,
        dir: tmux.global_option(DIR)?,
        key_bound: !key.is_empty() && binding(tmux, &key)?.contains(POPUP_MARKER),
        key,
        focus_hook: hook_entries(tmux, FOCUS_HOOK)?
            .iter()
            .any(|(_, command)| command.contains(SEEN_MARKER)),
        mirror: mirror_pid(tmux)?.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handlers_are_guarded_and_escaped() {
        assert_eq!(
            guarded(
                Path::new("/nix/store/x-tracon/bin/tracon"),
                "seen '#{pane_id}'"
            ),
            "[ ! -e /nix/store/x-tracon/bin/tracon ] || /nix/store/x-tracon/bin/tracon seen '#{pane_id}'"
        );
        assert_eq!(
            guarded(Path::new("/opt/my #1/tracon"), "x"),
            "[ ! -e '/opt/my ##1/tracon' ] || '/opt/my ##1/tracon' x"
        );
    }
}
