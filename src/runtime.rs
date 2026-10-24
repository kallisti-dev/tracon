//! Where the daemon keeps its sockets, lock and journal: a directory only
//! this user can read.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Overrides the directory, so tests and alternate setups keep to their own
/// daemon.
pub const DIR_VAR: &str = "TRACON_RUNTIME_DIR";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    dir: PathBuf,
}

impl Runtime {
    /// `$TRACON_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/tracon`, else
    /// `/tmp/tracon-<uid>`.
    pub fn from_env() -> Self {
        if let Some(dir) = std::env::var_os(DIR_VAR).filter(|dir| !dir.is_empty()) {
            return Runtime::at(PathBuf::from(dir));
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
            return Runtime::at(PathBuf::from(dir).join("tracon"));
        }
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Runtime::at(PathBuf::from(format!("/tmp/tracon-{uid}")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Runtime { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn rpc_socket(&self) -> PathBuf {
        self.dir.join("rpc.sock")
    }

    pub fn events_socket(&self) -> PathBuf {
        self.dir.join("events.sock")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.dir.join("lock")
    }

    pub fn journal(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    /// What a daemon started on demand writes to standard error.
    pub fn log(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }

    /// Create the directory, readable by this user alone.
    pub fn ensure(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_directory() {
        let runtime = Runtime::at(PathBuf::from("/tmp/t"));
        assert_eq!(runtime.rpc_socket(), PathBuf::from("/tmp/t/rpc.sock"));
        assert_eq!(runtime.events_socket(), PathBuf::from("/tmp/t/events.sock"));
        assert_eq!(runtime.journal(), PathBuf::from("/tmp/t/journal.jsonl"));
        assert_eq!(runtime.log(), PathBuf::from("/tmp/t/daemon.log"));
    }
}
