//! Running tmux commands against one server: the one at a socket, or the one
//! `$TMUX` names.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Error;

/// The tmux binary, baked in by the Nix packages and found on `PATH`
/// otherwise.
const PROGRAM: &str = match option_env!("TRACON_TMUX") {
    Some(path) => path,
    None => "tmux",
};

/// Separates fields in `-F` output. tmux prints option values verbatim, and
/// stores keep this character out of the values they write.
pub const SEPARATOR: char = '\u{1f}';

/// The `wait-for` channel signalled after every state change, for scripts
/// that block on it.
pub const CHANNEL: &str = "tracon";

/// A tmux server: the one at `socket`, or without one the server `$TMUX`
/// names, as a process inside tmux sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tmux {
    socket: Option<PathBuf>,
}

impl Tmux {
    /// Whether this process runs inside a tmux pane.
    pub fn inside() -> bool {
        std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
    }

    /// The server this process runs in, from `$TMUX`.
    pub fn from_env() -> Option<Self> {
        socket_in(&std::env::var("TMUX").ok()?).map(Tmux::at)
    }

    pub fn at(socket: impl Into<PathBuf>) -> Self {
        Tmux {
            socket: Some(socket.into()),
        }
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// A tmux command line for this server. `-u` makes the client UTF-8
    /// whatever the locale; otherwise tmux replaces control characters in
    /// its output, the field separator among them.
    pub fn command(&self) -> Command {
        let mut command = Command::new(PROGRAM);
        command.arg("-u");
        if let Some(socket) = &self.socket {
            command.arg("-S").arg(socket);
        }
        command
    }

    /// Run tmux with `args` and return its standard output. The server is
    /// named on the command line or found through `$TMUX`, so this works
    /// without a controlling terminal.
    pub fn run<I, S>(&self, args: I) -> Result<String, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let output = self
            .command()
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| Error::Spawn {
                program: PROGRAM.to_owned(),
                source,
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let command = args
                .first()
                .map(|arg| arg.to_string_lossy().into_owned())
                .unwrap_or_default();
            Err(Error::Tmux {
                command,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    /// The value of a global option, or an empty string when it is unset.
    pub fn global_option(&self, name: &str) -> Result<String, Error> {
        let value = self.run(["show-options", "-gqv", name])?;
        Ok(value.strip_suffix('\n').unwrap_or(&value).to_owned())
    }
}

/// The socket path in a `$TMUX` value, which is `socket,pid,index`.
pub fn socket_in(value: &str) -> Option<PathBuf> {
    let socket = value.split(',').next().unwrap_or("");
    (!socket.is_empty()).then(|| PathBuf::from(socket))
}

/// Quote `value` as one word in tmux's command language, for command lists
/// passed to `if-shell` and key bindings. Single quotes suppress format, `~`
/// and environment expansion and `;` and `#` handling; an embedded single
/// quote closes the string, adds an escaped quote and reopens it.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Escape `#` for commands that expand formats in their arguments, such as
/// `run-shell`, `display-message` and `display-popup`.
pub fn escape_format(value: &str) -> String {
    value.replace('#', "##")
}

/// Quote `value` as one word for `/bin/sh`.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./%:@+=,".contains(c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// Whether `id` has the form of a tmux pane id, such as `%3`.
pub fn valid_pane_id(id: &str) -> bool {
    id.strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(escape_format("#{x}##"), "##{x}####");
        assert_eq!(
            shell_quote("/nix/store/abc-tracon/bin/tracon"),
            "/nix/store/abc-tracon/bin/tracon"
        );
        assert_eq!(shell_quote("a b'c"), r"'a b'\''c'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn the_socket_is_the_first_field_of_tmux() {
        assert_eq!(
            socket_in("/tmp/tmux-501/default,1234,0"),
            Some(PathBuf::from("/tmp/tmux-501/default"))
        );
        assert_eq!(socket_in(""), None);
        assert_eq!(
            Tmux::at("/tmp/s").command().get_args().collect::<Vec<_>>(),
            ["-u", "-S", "/tmp/s"]
        );
        assert_eq!(
            Tmux::default().command().get_args().collect::<Vec<_>>(),
            ["-u"]
        );
    }

    #[test]
    fn pane_ids() {
        assert!(valid_pane_id("%0"));
        assert!(valid_pane_id("%123"));
        for id in ["", "%", "3", "%3a", "@3", "$1"] {
            assert!(!valid_pane_id(id), "{id:?}");
        }
    }
}
