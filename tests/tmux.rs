//! The binary and both plugins against a real tmux server on a disposable
//! socket, with a daemon and an option mirror of their own in a disposable
//! runtime directory. No command here receives the caller's `TMUX` or
//! runtime directory, so a test run inside tmux never touches that server
//! or the daemon serving it.
//!
//! The packaging check points `TEST_TMUX_PLUGIN` and `TEST_CLAUDE_PLUGIN` at
//! the Nix packages; otherwise the tests assemble both plugins from the
//! source and the freshly built binary.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use tracon::harness::{Claude, Harness};
use tracon::runtime::DIR_VAR;
use tracon::state::Record;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const SESSION: &str = "11111111-2222-3333-4444-555555555555";

fn wait_until<T>(what: &str, timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(25));
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

struct Client {
    process: Child,
    // Held open: a control client detaches at end of input.
    _input: ChildStdin,
}

struct Server {
    dir: TempDir,
    socket: PathBuf,
    /// The daemon's directory, which every command and the tmux server's
    /// environment name.
    runtime: PathBuf,
    tmux_plugin: PathBuf,
    claude_plugin: PathBuf,
    clients: Vec<Client>,
    /// The option mirror, as setup would start it, but a child of the test.
    mirror: Option<Child>,
}

impl Server {
    fn start() -> Self {
        // A short path stays within the sockaddr_un limit on macOS.
        let dir = tempfile::Builder::new()
            .prefix("trc-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("s");
        let runtime = dir.path().join("d");
        fs::create_dir(dir.path().join("home")).unwrap();
        let config = dir.path().join("tmux.conf");
        fs::write(
            &config,
            format!(
                "set -g default-shell /bin/sh\n\
                 set -g status off\n\
                 set -g exit-empty off\n\
                 set -g base-index 0\n\
                 set -g pane-base-index 0\n\
                 set -g automatic-rename off\n\
                 set -g focus-events on\n\
                 set-environment -g {DIR_VAR} {}\n",
                runtime.display()
            ),
        )
        .unwrap();
        let (tmux_plugin, claude_plugin) = Self::plugins(dir.path());
        let mut server = Server {
            dir,
            socket,
            runtime,
            tmux_plugin,
            claude_plugin,
            clients: Vec::new(),
            mirror: None,
        };
        let started = server
            .command("tmux")
            .args(["-S".as_ref(), server.socket.as_os_str()])
            .arg("-f")
            .arg(&config)
            .args(["new-session", "-d", "-s", "work", "-x", "200", "-y", "50"])
            .output()
            .unwrap();
        assert!(started.status.success(), "{started:?}");
        server.mirror = Some(server.spawn_mirror());
        server
    }

    fn spawn_mirror(&self) -> Child {
        fs::create_dir_all(&self.runtime).unwrap();
        let log = fs::File::create(self.runtime.join("mirror.log")).unwrap();
        let mut command = self.command(self.binary());
        self.inside(&mut command, "%0");
        command
            .arg("mirror")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    /// Both plugins, as packaged or assembled like a packaged copy.
    fn plugins(dir: &Path) -> (PathBuf, PathBuf) {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_tracon"));
        let tmux_plugin = std::env::var_os("TEST_TMUX_PLUGIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let plugin = dir.join("tmux-plugin");
                fs::create_dir_all(plugin.join("bin")).unwrap();
                fs::copy(
                    Path::new(ROOT).join("tracon.tmux"),
                    plugin.join("tracon.tmux"),
                )
                .unwrap();
                symlink(&binary, plugin.join("bin/tracon")).unwrap();
                plugin
            });
        let claude_plugin = std::env::var_os("TEST_CLAUDE_PLUGIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let plugin = dir.join("claude-plugin");
                for part in [".claude-plugin", "hooks", "skills", "docs"] {
                    if !Path::new(ROOT).join(part).exists() {
                        continue;
                    }
                    copy_tree(&Path::new(ROOT).join(part), &plugin.join(part));
                }
                fs::create_dir_all(plugin.join("bin")).unwrap();
                symlink(&binary, plugin.join("bin/tracon")).unwrap();
                plugin
            });
        (tmux_plugin, claude_plugin)
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// A command with this server's environment and nobody else's tmux.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("TRACON_CLIENT")
            .env(DIR_VAR, &self.runtime)
            .env("HOME", self.home())
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null());
        command
    }

    fn inside(&self, command: &mut Command, pane: &str) {
        command
            .env("TMUX", format!("{},0,0", self.socket.display()))
            .env("TMUX_PANE", pane);
    }

    fn tmux_output(&self, args: &[&str]) -> Output {
        self.command("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap()
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = self.tmux_output(args);
        assert!(output.status.success(), "tmux {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    /// The tracon the Claude plugin's hooks run.
    fn binary(&self) -> PathBuf {
        self.claude_plugin.join("bin/tracon")
    }

    fn tracon_output(&self, pane: &str, args: &[&str], input: Option<&str>) -> Output {
        let mut command = self.command(self.binary());
        self.inside(&mut command, pane);
        command.args(args);
        let Some(input) = input else {
            return command.output().unwrap();
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn tracon(&self, pane: &str, args: &[&str]) -> String {
        let output = self.tracon_output(pane, args, None);
        assert!(output.status.success(), "tracon {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn hook(&self, pane: &str, mut payload: Value) {
        if payload.get("session_id").is_none() {
            payload["session_id"] = json!(SESSION);
        }
        let output = self.tracon_output(pane, &["hook", "claude"], Some(&payload.to_string()));
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "hook reported: {output:?}");
    }

    fn option(&self, pane: &str, name: &str) -> String {
        self.tmux(&["show-options", "-pqv", "-t", pane, name])
    }

    fn state(&self, pane: &str) -> String {
        self.option(pane, "@tracon_state")
    }

    /// Wait for the mirror to show `expected` as the pane's state.
    fn state_becomes(&self, pane: &str, expected: &str) {
        self.option_becomes(pane, "@tracon_state", expected);
    }

    fn option_becomes(&self, pane: &str, name: &str, expected: &str) {
        wait_until(
            &format!("{name} of {pane} is {expected:?}"),
            Duration::from_secs(10),
            || (self.option(pane, name) == expected).then_some(()),
        );
    }

    /// The listed entry for `pane`, when it is listed.
    fn entry(&self, pane: &str) -> Option<Value> {
        self.list()
            .into_iter()
            .find(|entry| entry["pane"]["id"] == pane)
    }

    /// The listed status of `pane`, empty when it has none or isn't listed.
    fn status(&self, pane: &str) -> String {
        self.entry(pane)
            .and_then(|entry| entry["status"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// Wait for the list to show `expected` as the pane's status.
    fn status_becomes(&self, pane: &str, expected: &str) {
        wait_until(
            &format!("status of {pane} is {expected:?}"),
            Duration::from_secs(10),
            || (self.status(pane) == expected).then_some(()),
        );
    }

    /// Give `pane` a record before the daemon starts, through its journal.
    fn seed(&self, pane: &str, mut record: Value, since: u64) {
        assert!(
            !self.runtime.join("rpc.sock").exists(),
            "seed before the daemon starts"
        );
        fs::create_dir_all(&self.runtime).unwrap();
        record["since"] = json!(since);
        let line = json!({
            "pane": { "backend": "tmux", "id": pane, "server": self.socket },
            "record": record,
        });
        let mut journal = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.runtime.join("journal.jsonl"))
            .unwrap();
        writeln!(journal, "{line}").unwrap();
    }

    /// The event socket, read past its greeting and snapshot.
    fn events(&self) -> BufReader<UnixStream> {
        let stream = UnixStream::connect(self.runtime.join("events.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        for expected in ["hello", "snapshot"] {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(message["type"], expected, "{line}");
        }
        reader
    }

    fn list(&self) -> Vec<Value> {
        serde_json::from_str(&self.tracon("%0", &["list", "--json"])).unwrap()
    }

    /// Each listed pane with its status, in list order.
    fn listed(&self) -> Vec<(String, String)> {
        self.list()
            .iter()
            .map(|entry| {
                (
                    entry["pane"]["id"].as_str().unwrap().to_owned(),
                    entry["status"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect()
    }

    fn new_window(&self, target: &str) -> String {
        self.tmux(&["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", target])
    }

    fn new_session(&self, name: &str) -> String {
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-P",
            "-F",
            "#{pane_id}",
            "-x",
            "200",
            "-y",
            "50",
        ])
    }

    /// Attach a control-mode client, which needs no terminal, and return its
    /// name.
    fn attach(&mut self, session: &str) -> String {
        let before = self.tmux(&["list-clients", "-F", "#{client_name}"]);
        let mut process = self
            .command("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["-C", "attach", "-t", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = process.stdin.take().unwrap();
        self.clients.push(Client {
            process,
            _input: input,
        });
        wait_until("a client attaches", Duration::from_secs(10), || {
            let clients = self.tmux(&["list-clients", "-F", "#{client_name}"]);
            clients
                .lines()
                .find(|client| !before.lines().any(|old| old == *client))
                .map(str::to_owned)
        })
    }

    /// The prefix table's binding for `key`, as `list-keys` prints it.
    fn binding(&self, key: &str) -> String {
        let table = self.tmux(&["list-keys", "-T", "prefix"]);
        table
            .lines()
            .find(|line| line.split_whitespace().skip_while(|w| *w != "-T").nth(2) == Some(key))
            .unwrap_or_default()
            .to_owned()
    }

    fn shows(&self, client: &str) -> String {
        self.tmux(&[
            "display-message",
            "-p",
            "-c",
            client,
            "#{session_name} #{pane_id}",
        ])
    }

    fn setup(&self) {
        let output = self
            .command("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["run-shell"])
            .arg(self.tmux_plugin.join("tracon.tmux"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(self.tmux(&["show-options", "-gqv", "@tracon-error"]), "");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Only ever this test's own daemon and socket.
        assert!(self.runtime.starts_with(self.dir.path()));
        assert!(self.socket.starts_with(self.dir.path()));
        if let Some(mirror) = &mut self.mirror {
            let _ = mirror.kill();
            let _ = mirror.wait();
        }
        let _ = self
            .command(self.binary())
            .args(["daemon", "--stop"])
            .output();
        let _ = self.tmux_output(&["kill-server"]);
        // Shown for a failed test, where their complaints explain it.
        for log in ["daemon.log", "mirror.log"] {
            if let Ok(text) = fs::read_to_string(self.runtime.join(log))
                && !text.is_empty()
            {
                eprintln!("{log}:\n{text}");
            }
        }
        for client in &mut self.clients {
            let _ = client.process.kill();
            let _ = client.process.wait();
        }
    }
}

fn event(name: &str, extra: Value) -> Value {
    let mut payload = json!({ "hook_event_name": name });
    for (key, value) in extra.as_object().unwrap() {
        payload[key] = value.clone();
    }
    payload
}

fn bash(agent: Option<&str>) -> Value {
    let mut tool = json!({ "tool_name": "Bash", "tool_input": { "command": "ls -la > x" } });
    if let Some(agent) = agent {
        tool["agent_id"] = json!(agent);
    }
    tool
}

#[test]
fn hooks_record_state_and_the_mirror_follows() {
    let server = Server::start();
    server.hook("%0", event("SessionStart", json!({ "source": "startup" })));
    server.state_becomes("%0", "ready");
    assert_eq!(server.option("%0", "@tracon_harness"), "claude");
    assert_eq!(server.option("%0", "@tracon_session"), SESSION);
    assert_eq!(server.status("%0"), "idle");

    server.hook("%0", event("PermissionRequest", bash(None)));
    server.state_becomes("%0", "waiting.permission");
    assert_eq!(server.option("%0", "@tracon_message"), "Bash: ls -la > x");
    let entry = &server.list()[0];
    assert_eq!(entry["pane"]["backend"], "tmux");
    assert_eq!(entry["pane"]["id"], "%0");
    assert_eq!(
        entry["pane"]["server"],
        server.socket.to_string_lossy().as_ref()
    );
    assert_eq!(entry["location"], "work:0.0");
    assert_eq!(entry["state"], "waiting.permission");
    assert_eq!(entry["status"], "waiting.permission");
    assert_eq!(entry["label"], "needs approval");
    assert_eq!(entry["icon"], "?");
    assert_eq!(entry["harness"], "claude");

    server.hook(
        "%0",
        event("Stop", json!({ "background_tasks": [{ "id": "a" }] })),
    );
    server.state_becomes("%0", "ready");
    server.option_becomes("%0", "@tracon_outcome", "finished");
    assert_eq!(server.option("%0", "@tracon_background"), "1");
    let entry = server.entry("%0").unwrap();
    assert_eq!(entry["status"], "done");
    assert_eq!(entry["background"], 1);
    let row = server.tracon("%0", &["list"]);
    assert!(row.contains("✓ done"), "{row}");
    assert!(row.contains("work:0.0"), "{row}");
    assert!(row.trim_end().ends_with("· 1 in background"), "{row}");
}

#[test]
fn hooks_match_the_reference_semantics() {
    let server = Server::start();
    let sub = "ae48a9ce1c4008eda";
    let quoted = "it's ; #{pane_id} $HOME ~ \"q\" \\ end;";
    let events = [
        event("SessionStart", json!({})),
        event("UserPromptSubmit", json!({ "prompt": quoted })),
        event("PermissionRequest", bash(None)),
        event(
            "Notification",
            json!({ "notification_type": "permission_prompt", "message": "late" }),
        ),
        event("PostToolUse", bash(None)),
        event(
            "Stop",
            json!({ "background_tasks": [{ "id": sub }], "last_assistant_message": quoted }),
        ),
        event("PermissionRequest", bash(Some(sub))),
        event("PostToolUse", bash(None)),
        event("Stop", json!({ "background_tasks": [] })),
        event("PostToolUse", bash(Some("other"))),
        event("PostToolUse", bash(Some(sub))),
        event("SubagentStop", json!({ "agent_id": sub })),
        event(
            "PreToolUse",
            json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": [{ "question": "Red?" }] } }),
        ),
        event(
            "Notification",
            json!({ "notification_type": "idle_prompt" }),
        ),
        event("Stop", json!({})),
        event(
            "Notification",
            json!({ "notification_type": "idle_prompt" }),
        ),
        event("StopFailure", json!({ "error": "rate_limit" })),
        event("SessionEnd", json!({ "session_id": "old" })),
        event("SessionEnd", json!({})),
    ];
    let mut reference = Record::default();
    for (step, payload) in events.into_iter().enumerate() {
        let mut payload = payload;
        if payload.get("session_id").is_none() {
            payload["session_id"] = json!(SESSION);
        }
        let since_before = reference.since;
        if let Some(change) = Claude.interpret(&payload).unwrap() {
            change.apply_to(&mut reference, 1_000_000 + step as u64);
        }
        let stored_before = server.entry("%0").and_then(|entry| entry["since"].as_u64());
        server.hook("%0", payload.clone());
        let entry = server.entry("%0").unwrap_or(Value::Null);
        let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
        let state = reference
            .state
            .as_ref()
            .map_or(Value::Null, |state| Value::String(state.to_string()));
        assert_eq!(entry["state"], state, "step {step}: {payload}");
        let outcome = reference.outcome.map_or(Value::Null, |outcome| {
            Value::String(outcome.as_str().to_owned())
        });
        assert_eq!(entry["outcome"], outcome, "step {step}");
        if reference.state.is_some() {
            assert_eq!(entry["background"], reference.background, "step {step}");
        }
        assert_eq!(entry["message"], text(&reference.message), "step {step}");
        assert_eq!(entry["agent"], text(&reference.agent), "step {step}");
        assert_eq!(entry["session"], text(&reference.session), "step {step}");
        let stored = entry["since"].as_u64();
        assert_eq!(stored.is_none(), reference.since.is_none(), "step {step}");
        if since_before.is_some() && since_before == reference.since {
            assert_eq!(stored, stored_before, "step {step} reset since");
        }
    }
    // The mirror follows, to the last SessionEnd's clearing.
    server.state_becomes("%0", "");
    assert_eq!(server.option("%0", "@tracon_session"), "");
}

#[test]
fn hooks_succeed_quietly_outside_tmux_and_report_bad_input() {
    let server = Server::start();
    let mut outside = server.command(server.binary());
    let output = outside.args(["hook", "claude"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let output = server.tracon_output("%0", &["hook", "claude"], Some("not json"));
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("parsing the hook payload"));
    let output = server.tracon_output("%0", &["hook", "nobody"], Some("{}"));
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown harness"));
    assert_eq!(server.state("%0"), "");
}

#[test]
fn panes_are_ordered_by_priority_then_wait_and_pins_apply() {
    let server = Server::start();
    let panes: Vec<String> = (0..4).map(|_| server.new_window("work")).collect();
    server.seed("%0", json!({ "state": "ready" }), 1);
    server.seed(
        &panes[0],
        json!({ "state": "ready", "outcome": "finished" }),
        20,
    );
    server.seed(&panes[1], json!({ "state": "waiting.question" }), 30);
    server.seed(&panes[2], json!({ "state": "waiting.permission" }), 10);
    server.seed(&panes[3], json!({ "state": "working" }), 5);
    let pane = |id: &str, status: &str| (id.to_owned(), status.to_owned());
    assert_eq!(
        server.listed(),
        [
            pane(&panes[2], "waiting.permission"),
            pane(&panes[1], "waiting.question"),
            pane(&panes[0], "done"),
            pane(&panes[3], "working"),
            pane("%0", "idle"),
        ]
    );

    server.tracon(&panes[3], &["pin"]);
    server.option_becomes(&panes[3], "@tracon_pinned", "1");
    assert_eq!(server.listed()[2], pane(&panes[3], "working"));
    assert_eq!(server.list()[2]["pinned"], true);

    server.tmux(&["set-option", "-g", "@tracon-order", "working waiting"]);
    assert_eq!(server.listed()[0], pane(&panes[3], "working"));
    server.tracon("%0", &["unpin", &panes[3]]);
    server.option_becomes(&panes[3], "@tracon_pinned", "");

    server.tmux(&["set-option", "-g", "@tracon-order", "waiting Bad"]);
    let output = server.tracon_output("%0", &["list"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("invalid @tracon-order: invalid status \"Bad\"")
    );
}

#[test]
fn labels_icons_and_the_format_are_options() {
    let server = Server::start();
    server.hook("%0", event("PermissionRequest", bash(None)));
    // An entry covers the statuses beneath it that have none of their own.
    server.tmux(&[
        "set-option",
        "-g",
        "@tracon-labels",
        "waiting.permission=approve?,done=unread",
    ]);
    server.tmux(&["set-option", "-g", "@tracon-icons", "waiting=!"]);
    server.tmux(&[
        "set-option",
        "-g",
        "@tracon-format",
        "{icon} {label} \\({status}\\) {location}",
    ]);
    let entry = server.entry("%0").unwrap();
    assert_eq!(entry["label"], "approve?");
    assert_eq!(entry["icon"], "!");
    assert_eq!(
        server.tracon("%0", &["list"]),
        "! approve? (waiting.permission) work:0.0\n"
    );

    server.tmux(&["set-option", "-g", "@tracon-format", "{icon} {colour}"]);
    let output = server.tracon_output("%0", &["list"], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("invalid @tracon-format: unknown variable {colour}"),
        "{output:?}"
    );
}

#[test]
fn a_pinned_pane_without_an_agent_is_listed() {
    let server = Server::start();
    server.tracon("%0", &["pin", "tmux:%0"]);
    let list = server.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["state"], Value::Null);
    assert_eq!(list[0]["status"], Value::Null);
    assert_eq!(list[0]["label"], "");
}

#[test]
fn focus_moves_a_client_across_sessions() {
    let mut server = Server::start();
    server.new_session("other");
    let target = server.new_window("other");
    let client = server.attach("work");
    assert_eq!(server.shows(&client), "work %0");

    server.tracon("%0", &["focus", &target, "--client", &client]);
    assert_eq!(server.shows(&client), format!("other {target}"));
    // Without a client, the most recently active one.
    server.tracon(&target, &["focus", "%0"]);
    assert_eq!(server.shows(&client), "work %0");

    let output = server.tracon_output("%0", &["focus", "%99"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no longer exists"));
}

#[test]
fn next_skips_the_pane_the_client_shows() {
    let mut server = Server::start();
    let other = server.new_window("work");
    let client = server.attach("work");
    server.hook("%0", event("PermissionRequest", bash(None)));
    server.hook(&other, event("Stop", json!({})));
    server.tracon("%0", &["next", "--client", &client]);
    assert_eq!(server.shows(&client), format!("work {other}"));
    server.tracon("%0", &["next", "--client", &client]);
    assert_eq!(server.shows(&client), "work %0");
}

#[test]
fn focusing_a_finished_pane_marks_it_seen() {
    let mut server = Server::start();
    server.setup();
    let other = server.new_session("other");
    let client = server.attach("other");
    server.hook(
        "%0",
        event("Stop", json!({ "last_assistant_message": "All done" })),
    );
    server.hook(&other, event("PermissionRequest", bash(None)));
    server.state_becomes("%0", "ready");
    assert_eq!(server.status("%0"), "done");

    server.tracon("%0", &["focus", "%0", "--client", &client]);
    server.status_becomes("%0", "idle");
    // Being seen changes nothing the hooks recorded.
    assert_eq!(server.state("%0"), "ready");
    assert_eq!(server.option("%0", "@tracon_outcome"), "finished");
    assert_eq!(server.option("%0", "@tracon_message"), "All done");
    // Seeing a pane doesn't answer its prompt.
    server.tracon("%0", &["focus", &other, "--client", &client]);
    server.tracon("%0", &["seen", &other]);
    assert_eq!(
        server.entry(&other).unwrap()["status"],
        "waiting.permission"
    );
}

#[test]
fn stale_panes_are_listed_from_what_their_screen_shows() {
    let server = Server::start();
    let refused = server.new_window("work");
    let cancelled = server.new_window("work");
    for pane in ["%0", &refused] {
        server.hook(pane, event("PermissionRequest", bash(None)));
    }
    server.hook(
        &cancelled,
        event("UserPromptSubmit", json!({ "prompt": "a" })),
    );
    // `✶ Kerfuffling… (21s · ↓ 109 tokens)`, coloured as Claude Code draws
    // it. Octal escapes keep the typed line ASCII, which the shell reads
    // intact without a UTF-8 locale, as in the Nix sandbox.
    server.tmux(&[
        "send-keys",
        "-t",
        "%0",
        r"printf '\033[38;5;174m\342\234\266 Kerfuffling\342\200\246\033[0m (21s \302\267 \342\206\223 109 tokens)\n'",
        "Enter",
    ]);
    for (pane, screen) in [
        (&refused, "claude-2.1.300/refused-esc.txt"),
        (&cancelled, "claude-2.1.309/cancelled-esc.txt"),
    ] {
        let screen = Path::new(ROOT).join("tests/screens").join(screen);
        server.tmux(&[
            "send-keys",
            "-t",
            pane,
            &format!("cat '{}'", screen.display()),
            "Enter",
        ]);
    }
    let pane = |id: &str, status: &str| (id.to_owned(), status.to_owned());
    let expected = [
        pane("%0", "working"),
        pane(&refused, "idle.interrupted"),
        pane(&cancelled, "idle.interrupted"),
    ];
    wait_until(
        "the panes are listed from their screens",
        Duration::from_secs(10),
        || {
            let mut listed = server.listed();
            listed.sort();
            (listed == expected).then_some(())
        },
    );
    let list = server.list();
    let sighting = |id: &str| {
        let entry = list.iter().find(|entry| entry["pane"]["id"] == id).unwrap();
        entry["sighting"].as_str().unwrap().to_owned()
    };
    assert_eq!(sighting("%0"), "working");
    assert_eq!(sighting(&refused), "interrupted");
    assert_eq!(sighting(&cancelled), "finished");
    server.state_becomes("%0", "waiting.permission");
    server.state_becomes(&cancelled, "working");
}

#[test]
fn a_turn_that_ends_in_view_is_seen() {
    let mut server = Server::start();
    let hidden = server.new_window("work");
    server.attach("work");
    let finished = event("Stop", json!({ "last_assistant_message": "All done" }));
    server.hook("%0", finished.clone());
    server.hook(&hidden, finished);
    server.status_becomes("%0", "idle");
    server.option_becomes("%0", "@tracon_message", "All done");
    assert!(!server.option("%0", "@tracon_viewed").is_empty());
    server.status_becomes(&hidden, "done");
    assert_eq!(server.option(&hidden, "@tracon_viewed"), "");
}

#[test]
fn a_pane_viewed_last_leads_the_idle_group() {
    let mut server = Server::start();
    let panes: Vec<String> = (0..3).map(|_| server.new_window("work")).collect();
    server.seed(&panes[0], json!({ "state": "ready" }), 10);
    server.seed(&panes[1], json!({ "state": "ready" }), 20);
    server.setup();
    server.hook(&panes[2], event("Stop", json!({})));
    let client = server.attach("work");
    server.tracon("%0", &["focus", &panes[2], "--client", &client]);
    server.status_becomes(&panes[2], "idle");
    server.option_becomes(&panes[2], "@tracon_state", "ready");
    assert!(!server.option(&panes[2], "@tracon_viewed").is_empty());
    server.tracon("%0", &["focus", "%0", "--client", &client]);
    // Panes never viewed follow, newest state first.
    let idle = |pane: &String| (pane.clone(), "idle".to_owned());
    assert_eq!(
        server.listed(),
        [idle(&panes[2]), idle(&panes[1]), idle(&panes[0])]
    );
    // Groups outside @tracon-recent-first keep the longest in state first.
    server.tmux(&["set-option", "-g", "@tracon-recent-first", "working"]);
    assert_eq!(
        server.listed(),
        [idle(&panes[0]), idle(&panes[1]), idle(&panes[2])]
    );
}

#[test]
fn the_daemon_starts_on_demand_and_replays_its_journal() {
    let server = Server::start();
    let gone = server.new_window("work");
    assert!(!server.runtime.join("rpc.sock").exists());
    server.hook("%0", event("SessionStart", json!({})));
    server.hook(&gone, event("SessionStart", json!({})));
    assert!(server.runtime.join("rpc.sock").exists());
    server.state_becomes("%0", "ready");

    server.tracon("%0", &["daemon", "--stop"]);
    // Answered once the directory is free for the next daemon.
    assert!(!server.runtime.join("rpc.sock").exists());
    // Panes that die while no daemon runs are forgotten on restart, and the
    // mirror of the rest is written afresh.
    server.tmux(&["kill-pane", "-t", &gone]);
    server.tmux(&["set-option", "-pu", "-t", "%0", "@tracon_state"]);
    let pane = |id: &str, status: &str| (id.to_owned(), status.to_owned());
    assert_eq!(server.listed(), [pane("%0", "idle")]);
    server.state_becomes("%0", "ready");
    let journal = fs::read_to_string(server.runtime.join("journal.jsonl")).unwrap();
    assert!(journal.contains(r#""id":"%0""#), "{journal}");
    assert!(!journal.contains(&format!(r#""id":"{gone}""#)), "{journal}");
}

#[test]
fn the_event_socket_streams_every_change() {
    let server = Server::start();
    server.hook("%0", event("SessionStart", json!({})));
    let mut events = server.events();
    server.hook("%0", event("PermissionRequest", bash(None)));
    let mut line = String::new();
    events.read_line(&mut line).unwrap();
    let message: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(message["type"], "change", "{line}");
    assert_eq!(message["pane"]["id"], "%0");
    assert_eq!(message["record"]["state"], "waiting.permission");
    assert_eq!(message["seq"], 2);

    server.hook("%0", event("SessionEnd", json!({})));
    line.clear();
    events.read_line(&mut line).unwrap();
    let message: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(message["type"], "removed", "{line}");
    assert_eq!(message["seq"], 3);
}

#[test]
fn setup_is_idempotent_and_reversible() {
    let server = Server::start();
    let foreign = "display-message foreign";
    server.tmux(&["set-hook", "-g", "pane-focus-in[80]", foreign]);
    server.setup();
    server.setup();
    let hooks = server.tmux(&["show-hooks", "-g", "pane-focus-in"]);
    let ours: Vec<_> = hooks
        .lines()
        .filter(|line| line.contains("tracon seen"))
        .collect();
    assert_eq!(ours.len(), 1, "{hooks}");
    assert!(ours[0].starts_with("pane-focus-in[81]"), "{hooks}");
    assert!(
        hooks.contains("pane-focus-in[80] display-message foreign"),
        "{hooks}"
    );
    assert!(server.binding("a").contains("tracon popup"));
    assert_eq!(
        server.tmux(&["show-options", "-gqv", "@tracon-dir"]),
        server.tmux_plugin.to_string_lossy()
    );

    server.tmux(&["set-option", "-g", "@tracon-key", "T"]);
    server.setup();
    assert!(server.binding("T").contains("tracon popup"));
    assert_eq!(server.binding("a"), "");

    server.tracon("%0", &["setup", "--remove"]);
    let hooks = server.tmux(&["show-hooks", "-g", "pane-focus-in"]);
    assert!(!hooks.contains("tracon"), "{hooks}");
    assert!(hooks.contains("foreign"), "{hooks}");
    assert_eq!(server.binding("T"), "");
    assert_eq!(server.tmux(&["show-options", "-gqv", "@tracon-bin"]), "");
}

#[test]
fn a_failed_setup_is_reported_in_tmux() {
    let server = Server::start();
    let broken = server.dir.path().join("broken");
    fs::create_dir(&broken).unwrap();
    // The source script: the packaged one is a wrapper that runs the script
    // from its own store path.
    fs::copy(
        Path::new(ROOT).join("tracon.tmux"),
        broken.join("tracon.tmux"),
    )
    .unwrap();
    let output = server
        .command("tmux")
        .arg("-S")
        .arg(&server.socket)
        .arg("run-shell")
        .arg(broken.join("tracon.tmux"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = server.tmux(&["show-options", "-gqv", "@tracon-error"]);
    assert!(error.contains("bin/tracon is missing"), "{error}");
}

#[test]
fn every_registered_claude_hook_runs() {
    let server = Server::start();
    let hooks: Value = serde_json::from_str(
        &fs::read_to_string(server.claude_plugin.join("hooks/hooks.json")).unwrap(),
    )
    .unwrap();
    let run = |name: &str, command: &str| {
        let payload = event(
            name,
            json!({ "session_id": SESSION, "tool_name": "AskUserQuestion" }),
        );
        let mut shell = server.command("/bin/sh");
        server.inside(&mut shell, "%0");
        let mut child = shell
            .args(["-c", command])
            .env("CLAUDE_PLUGIN_ROOT", &server.claude_plugin)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{name}: {output:?}"
        );
    };
    let mut events = 0;
    let mut session_end = None;
    for (name, groups) in hooks["hooks"].as_object().unwrap() {
        for hook in groups[0]["hooks"].as_array().unwrap() {
            let command = hook["command"].as_str().unwrap();
            run(name, command);
            if name == "SessionEnd" {
                session_end = Some(command.to_owned());
            }
            events += 1;
        }
    }
    assert!(events >= 14, "{events} hooks");
    run("SessionStart", groups_command(&hooks, "SessionStart"));
    server.state_becomes("%0", "ready");
    run("SessionEnd", &session_end.expect("a SessionEnd hook"));
    server.state_becomes("%0", "");
}

fn groups_command<'a>(hooks: &'a Value, event: &str) -> &'a str {
    hooks["hooks"][event][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
}

#[test]
fn the_picker_lists_previews_and_focuses() {
    let mut server = Server::start();
    let agent = server.new_session("agents");
    server.tmux(&["send-keys", "-t", &agent, "echo preview-marker", "Enter"]);
    server.hook(&agent, event("PermissionRequest", bash(None)));
    let client = server.attach("work");
    let picker = server.new_window("work");
    server.tmux(&[
        "respawn-pane",
        "-k",
        "-t",
        &picker,
        &format!("TRACON_CLIENT={client} {} pick", server.binary().display()),
    ]);
    let screen = wait_until("the picker draws", Duration::from_secs(10), || {
        let screen = server.tmux(&["capture-pane", "-p", "-t", &picker]);
        (screen.contains("? needs approval") && screen.contains("preview-marker")).then_some(screen)
    });
    assert!(screen.contains("agents:0.0"), "{screen}");

    server.tmux(&["send-keys", "-t", &picker, "p"]);
    wait_until("the pane is pinned", Duration::from_secs(10), || {
        (server.option(&agent, "@tracon_pinned") == "1").then_some(())
    });
    server.tmux(&["send-keys", "-t", &picker, "/nomatch"]);
    wait_until("the filter hides the pane", Duration::from_secs(10), || {
        let screen = server.tmux(&["capture-pane", "-p", "-t", &picker]);
        (screen.contains("/nomatch") && !screen.contains("agents:0.0")).then_some(())
    });
    server.tmux(&["send-keys", "-t", &picker, "Escape"]);
    wait_until(
        "clearing the filter shows it",
        Duration::from_secs(10),
        || {
            let screen = server.tmux(&["capture-pane", "-p", "-t", &picker]);
            screen.contains("agents:0.0").then_some(())
        },
    );
    server.tmux(&["send-keys", "-t", &picker, "Enter"]);
    wait_until("the client moves", Duration::from_secs(10), || {
        (server.shows(&client) == format!("agents {agent}")).then_some(())
    });
}

#[test]
fn the_picker_fits_a_short_pane() {
    let server = Server::start();
    server.hook("%0", event("PermissionRequest", bash(None)));
    let picker = server.tmux(&[
        "split-window",
        "-d",
        "-l",
        "6",
        "-t",
        "%0",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("{} pick", server.binary().display()),
    ]);
    wait_until("the picker draws", Duration::from_secs(10), || {
        let screen = server.tmux(&["capture-pane", "-p", "-t", &picker]);
        screen.contains("needs approval").then_some(())
    });
}

#[test]
fn popup_reports_an_empty_list_and_needs_a_client() {
    let mut server = Server::start();
    let output = server.tracon_output("%0", &["popup"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no tmux client is attached"));
    let client = server.attach("work");
    server.tracon("%0", &["popup", &client]);
}

/// The hooks module forwards mod events through the same command, under
/// `event`, for what the settings hooks miss.
#[test]
fn mod_events_report_what_the_hooks_miss() {
    let server = Server::start();
    server.hook("%0", event("PermissionRequest", bash(None)));
    assert_eq!(server.status("%0"), "waiting.permission");
    // The tool's row started running: approved.
    server.hook(
        "%0",
        json!({ "event": "ui.render", "running": true, "tool": "Bash" }),
    );
    assert_eq!(server.status("%0"), "working");
    // Ctrl-C while it ran: the note, then the turn's end.
    server.hook(
        "%0",
        json!({ "event": "session.append", "door": "note", "text": "[Request interrupted by user for tool use]" }),
    );
    server.hook(
        "%0",
        json!({ "event": "turn.complete", "reason": "aborted" }),
    );
    let entry = server.entry("%0").unwrap();
    assert_eq!(entry["status"], "idle.interrupted");
    assert_eq!(entry["outcome"], "interrupted");
    assert_eq!(entry["label"], "interrupted");
    assert_eq!(entry["message"], "Bash: ls -la > x");
    server.option_becomes("%0", "@tracon_outcome", "interrupted");
    // A shell command, which no prompt event reports.
    server.hook(
        "%0",
        json!({ "event": "prompt.change", "origin": "clear", "before": "! make check" }),
    );
    let entry = server.entry("%0").unwrap();
    assert_eq!(entry["status"], "working");
    assert_eq!(entry["message"], "! make check");
}

/// A daemon left by an earlier installation runs old code until setup finds
/// it running from another binary and replaces it, keeping the records.
#[test]
fn setup_replaces_a_daemon_from_another_binary() {
    let server = Server::start();
    let old = server.dir.path().join("old-tracon");
    fs::copy(env!("CARGO_BIN_EXE_tracon"), &old).unwrap();
    let output = server.command(&old).arg("list").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    server.hook("%0", event("SessionStart", json!({})));
    server.state_becomes("%0", "ready");
    let runs_from = |binary: &Path| {
        format!(
            "ok       the daemon runs tracon {} from {}",
            env!("CARGO_PKG_VERSION"),
            fs::canonicalize(binary).unwrap().display()
        )
    };
    let output = server.tracon_output("%0", &["check"], None);
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(report.contains(&runs_from(&old)), "{report}");

    server.setup();
    let output = server.tracon_output("%0", &["check"], None);
    let report = String::from_utf8_lossy(&output.stdout);
    let current = server.tmux_plugin.join("bin/tracon");
    assert!(report.contains(&runs_from(&current)), "{report}");
    assert_eq!(server.listed(), [("%0".to_owned(), "idle".to_owned())]);
}

#[test]
fn check_reports_a_working_setup() {
    let server = Server::start();
    server.setup();
    server.hook("%0", event("SessionStart", json!({})));
    let output = server.tracon_output("%0", &["check", "--session", SESSION], None);
    let report = String::from_utf8_lossy(&output.stdout);
    for line in [
        "ok       tracon",
        "ok       prefix a opens the picker",
        "ok       focusing a finished pane marks it seen",
        &format!("ok       this pane records session {SESSION}"),
    ] {
        assert!(report.contains(line), "missing {line:?} in\n{report}");
    }
    // Claude Code isn't installed here, or doesn't load the plugin.
    assert!(!report.contains("problem  tmux"), "{report}");

    server.tmux(&["set-option", "-g", "@tracon-preview-interval", "fast"]);
    let output = server.tracon_output("%0", &["check"], None);
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        report.contains("a tracon option is invalid: invalid @tracon-preview-interval \"fast\""),
        "{report}"
    );
}

/// Real Claude Code, without network access: requests go to a closed local
/// port, and session hooks run before the first request fails.
#[test]
fn real_claude_reports_its_session() {
    let Some(claude) = std::env::var_os("CLAUDE_BINARY") else {
        eprintln!("skipped: CLAUDE_BINARY is supplied by the agents check");
        return;
    };
    let server = Server::start();
    let config = server.home().join("claude config");
    fs::create_dir_all(config.join("skills")).unwrap();
    // As Home Manager's programs.claude-code.plugins installs it.
    symlink(&server.claude_plugin, config.join("skills/tracon")).unwrap();
    for (name, value) in [
        ("CLAUDE_CONFIG_DIR", config.to_string_lossy().into_owned()),
        ("ANTHROPIC_API_KEY", "sk-ant-test-not-a-key".to_owned()),
        ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        ("HOME", server.home().to_string_lossy().into_owned()),
    ] {
        server.tmux(&["set-environment", "-g", name, &value]);
    }
    let claude = Path::new(&claude).display().to_string();
    let pane = server.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        &format!("'{claude}' -p hello; sleep 60"),
    ]);
    wait_until(
        "Claude reports its session",
        Duration::from_secs(60),
        || (server.option(&pane, "@tracon_harness") == "claude").then_some(()),
    );
    assert_eq!(server.option(&pane, "@tracon_session").len(), 36);
    // Claude retries the closed port; interrupting it ends the session.
    server.tmux(&["send-keys", "-t", &pane, "C-c"]);
    wait_until(
        "Claude clears its session on exit",
        Duration::from_secs(60),
        || server.state(&pane).is_empty().then_some(()),
    );
}
