//! The binary and both plugins against a real tmux server on a disposable
//! socket. No command here receives the caller's `TMUX`, so a test run inside
//! tmux never touches that server.
//!
//! The packaging check points `TEST_TMUX_PLUGIN` and `TEST_CLAUDE_PLUGIN` at
//! the Nix packages; otherwise the tests assemble both plugins from the
//! source and the freshly built binary.

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use tracon::harness::{Claude, Harness};
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
    tmux_plugin: PathBuf,
    claude_plugin: PathBuf,
    clients: Vec<Client>,
}

impl Server {
    fn start() -> Self {
        // A short path stays within the sockaddr_un limit on macOS.
        let dir = tempfile::Builder::new()
            .prefix("trc-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("s");
        fs::create_dir(dir.path().join("home")).unwrap();
        let config = dir.path().join("tmux.conf");
        fs::write(
            &config,
            "set -g default-shell /bin/sh\n\
             set -g status off\n\
             set -g exit-empty off\n\
             set -g base-index 0\n\
             set -g pane-base-index 0\n\
             set -g automatic-rename off\n\
             set -g focus-events on\n",
        )
        .unwrap();
        let (tmux_plugin, claude_plugin) = Self::plugins(dir.path());
        let server = Server {
            dir,
            socket,
            tmux_plugin,
            claude_plugin,
            clients: Vec::new(),
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
        server
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

    fn list(&self) -> Vec<Value> {
        serde_json::from_str(&self.tracon("%0", &["list", "--json"])).unwrap()
    }

    fn listed(&self) -> Vec<(String, String)> {
        self.list()
            .iter()
            .map(|entry| {
                (
                    entry["pane"]["id"].as_str().unwrap().to_owned(),
                    entry["state"].as_str().unwrap_or("").to_owned(),
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
        // Only ever this test's own socket.
        assert!(self.socket.starts_with(self.dir.path()));
        let _ = self.tmux_output(&["kill-server"]);
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
fn hooks_record_state_in_pane_options() {
    let server = Server::start();
    server.hook("%0", event("SessionStart", json!({ "source": "startup" })));
    assert_eq!(server.state("%0"), "idle");
    assert_eq!(server.option("%0", "@tracon_harness"), "claude");
    assert_eq!(server.option("%0", "@tracon_session"), SESSION);

    server.hook("%0", event("PermissionRequest", bash(None)));
    assert_eq!(server.state("%0"), "waiting.permission");
    assert_eq!(server.option("%0", "@tracon_message"), "Bash: ls -la > x");
    let entry = &server.list()[0];
    assert_eq!(entry["pane"], json!({ "backend": "tmux", "id": "%0" }));
    assert_eq!(entry["location"], "work:0.0");
    assert_eq!(entry["state"], "waiting.permission");
    assert_eq!(entry["harness"], "claude");
}

#[test]
fn the_tmux_store_matches_the_reference_semantics() {
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
        let stored_before = server.option("%0", "@tracon_since");
        server.hook("%0", payload.clone());
        let text = |value: &Option<String>| value.clone().unwrap_or_default();
        let state = reference
            .state
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert_eq!(server.state("%0"), state, "step {step}: {payload}");
        assert_eq!(
            server.option("%0", "@tracon_message"),
            text(&reference.message),
            "step {step}"
        );
        assert_eq!(
            server.option("%0", "@tracon_agent"),
            text(&reference.agent),
            "step {step}"
        );
        assert_eq!(
            server.option("%0", "@tracon_session"),
            text(&reference.session),
            "step {step}"
        );
        let stored = server.option("%0", "@tracon_since");
        assert_eq!(stored.is_empty(), reference.since.is_none(), "step {step}");
        if since_before.is_some() && since_before == reference.since {
            assert_eq!(stored, stored_before, "step {step} reset since");
        }
    }
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
    let set = |pane: &str, state: &str, since: u64| {
        for (option, value) in [
            ("@tracon_state", state.to_owned()),
            ("@tracon_since", since.to_string()),
        ] {
            server.tmux(&["set-option", "-p", "-t", pane, option, &value]);
        }
    };
    set("%0", "idle", 1);
    set(&panes[0], "done", 20);
    set(&panes[1], "waiting.question", 30);
    set(&panes[2], "waiting.permission", 10);
    set(&panes[3], "working", 5);
    let pane = |id: &str, state: &str| (id.to_owned(), state.to_owned());
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
    assert_eq!(server.option(&panes[3], "@tracon_pinned"), "1");
    assert_eq!(server.listed()[2], pane(&panes[3], "working"));
    assert_eq!(server.list()[2]["pinned"], true);

    server.tmux(&["set-option", "-g", "@tracon-order", "working waiting"]);
    assert_eq!(server.listed()[0], pane(&panes[3], "working"));
    server.tracon("%0", &["unpin", &panes[3]]);
    assert_eq!(server.option(&panes[3], "@tracon_pinned"), "");

    server.tmux(&["set-option", "-g", "@tracon-order", "waiting Bad"]);
    let output = server.tracon_output("%0", &["list"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid state \"Bad\""));
}

#[test]
fn a_pinned_pane_without_an_agent_is_listed() {
    let server = Server::start();
    server.tracon("%0", &["pin", "tmux:%0"]);
    let list = server.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["state"], Value::Null);
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
    assert_eq!(server.state("%0"), "done");

    server.tracon("%0", &["focus", "%0", "--client", &client]);
    wait_until("the pane is seen", Duration::from_secs(10), || {
        (server.state("%0") == "idle").then_some(())
    });
    assert_eq!(server.option("%0", "@tracon_message"), "All done");
    // Seeing a pane doesn't answer its prompt.
    server.tracon("%0", &["focus", &other, "--client", &client]);
    server.tracon("%0", &["seen", &other]);
    assert_eq!(server.state(&other), "waiting.permission");
}

#[test]
fn a_turn_that_ends_in_view_is_seen() {
    let mut server = Server::start();
    let hidden = server.new_window("work");
    server.attach("work");
    let finished = event("Stop", json!({ "last_assistant_message": "All done" }));
    server.hook("%0", finished.clone());
    server.hook(&hidden, finished);
    assert_eq!(server.state("%0"), "idle");
    assert_eq!(server.option("%0", "@tracon_message"), "All done");
    assert_eq!(server.state(&hidden), "done");
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
    assert!(events >= 15, "{events} hooks");
    run("SessionStart", groups_command(&hooks, "SessionStart"));
    assert_eq!(server.state("%0"), "idle");
    run("SessionEnd", &session_end.expect("a SessionEnd hook"));
    assert_eq!(server.state("%0"), "");
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
        (screen.contains("waiting.permission") && screen.contains("preview-marker"))
            .then_some(screen)
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
        screen.contains("waiting.permission").then_some(())
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
