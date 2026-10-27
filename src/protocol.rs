//! The messages between the daemon and its clients, on both sockets.
//!
//! The request socket carries JSON-RPC 2.0, one request and one response per
//! line. The event socket carries one [`Message`] per line: a `hello`, a
//! `snapshot` of every record, then every change in order, each numbered so
//! a client that reconnects can tell what it missed. Both sides ignore
//! fields they don't know, so either can be newer than the other.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::Locator;
use crate::list::Entry;
use crate::state::Record;

/// Bumped when a message changes shape in a way an older peer can't read.
pub const PROTOCOL: u32 = 1;

/// The environment variables a client sends, by which backends claim the
/// pane a command runs in.
pub type Env = BTreeMap<String, String>;

/// The variables worth sending; a backend reads the ones it knows.
pub const ENV_KEYS: [&str; 3] = ["TMUX", "TMUX_PANE", "TRACON_CLIENT"];

/// The sending process's environment, restricted to [`ENV_KEYS`].
pub fn env() -> Env {
    ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: impl Serialize) -> Self {
        Request {
            jsonrpc: "2.0".to_owned(),
            id: Value::from(id),
            method: method.to_owned(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }

    pub fn params<T: DeserializeOwned>(&self) -> Result<T, RpcError> {
        serde_json::from_value(self.params.clone()).map_err(|error| RpcError {
            code: RpcError::INVALID_PARAMS,
            message: format!("invalid params for {}: {error}", self.method),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn result(id: Value, result: impl Serialize) -> Self {
        Response {
            jsonrpc: "2.0".to_owned(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn error(id: Value, error: RpcError) -> Self {
        Response {
            jsonrpc: "2.0".to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// A command failed; the message says how.
    pub const FAILED: i64 = -32000;
    /// No backend recognises the pane the client runs in.
    pub const UNCLAIMED: i64 = -32001;
    /// The named pane no longer exists.
    pub const PANE_GONE: i64 = -32002;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

// Parameters, one struct per method. Each carries `env` so the daemon can
// tell which pane and backend instance the client runs in.

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hello {
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelloResult {
    pub version: String,
    pub protocol: u32,
    /// The daemon's executable, resolved through symlinks; empty once it
    /// no longer exists.
    #[serde(default)]
    pub binary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub harness: String,
    pub payload: Value,
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventResult {
    /// The pane the event was recorded against, or none when no backend
    /// claimed one.
    pub pane: Option<Locator>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct List {
    #[serde(default)]
    pub env: Env,
}

pub type ListResult = Vec<Entry>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Focus {
    /// A locator, or a bare id resolved against the client's own backend.
    pub pane: String,
    pub client: Option<String>,
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Next {
    pub client: Option<String>,
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NextResult {
    pub focused: Option<Locator>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pin {
    /// A locator or bare id; the client's own pane when absent.
    pub pane: Option<String>,
    pub pinned: bool,
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Seen {
    pub pane: Option<String>,
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capture {
    pub pane: Locator,
    /// Without colours or attributes.
    pub plain: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub env: Env,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettingsResult {
    pub preview_interval_ms: u64,
    /// The row format, as `@tracon-format` or the default gives it.
    #[serde(default)]
    pub format: String,
}

/// A record as the event socket carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub pane: Locator,
    pub record: Record,
}

/// One line on the event socket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Message {
    Hello {
        version: String,
        protocol: u32,
    },
    /// Every record, sent once on connection; `seq` is the last change
    /// applied before it.
    Snapshot {
        seq: u64,
        records: Vec<Stored>,
    },
    Change {
        seq: u64,
        pane: Locator,
        record: Record,
    },
    Removed {
        seq: u64,
        pane: Locator,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_tolerate_unknown_fields() {
        let request = Request::new(
            7,
            "pin",
            Pin {
                pane: Some("%3".into()),
                pinned: true,
                env: Env::new(),
            },
        );
        let text = serde_json::to_string(&request).unwrap();
        let parsed: Request = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.params::<Pin>().unwrap().pane.as_deref(), Some("%3"));

        let newer = r#"{"jsonrpc":"2.0","id":1,"method":"list","params":{"env":{},"extra":1}}"#;
        let parsed: Request = serde_json::from_str(newer).unwrap();
        assert!(parsed.params::<List>().is_ok());

        let message = Message::Removed {
            seq: 3,
            pane: Locator::tmux("%1"),
        };
        let text = serde_json::to_string(&message).unwrap();
        assert!(text.contains(r#""type":"removed""#));
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), message);
    }
}
