# Design

tracon keeps one state per agent pane and lists panes in priority order. A
daemon owns that state and exposes it through two sockets; the command line,
the picker and every other front end are clients of it.

| Surface | Commands | Role |
| --- | --- | --- |
| Ingest | `tracon hook <harness>` | Harness adapters turn hook payloads into state changes |
| Query | `tracon list [--json]`, the event socket | The ordered list with each pane's state, and every change as it happens |
| Command | `tracon focus`, `next`, `pin`, `unpin`, `seen` | Act on panes through a backend |

The library (`src/lib.rs`) holds the state model, the daemon with its store
and protocol, the client, harness adapters and backends. The binary adds the
command line, the picker and the check.

## Plugin loading

tracon has two halves, installed separately. The tmux plugin's entry point,
`tracon.tmux`, runs `tracon setup` from its `bin/` directory. The Claude Code
plugin is a directory with `.claude-plugin/plugin.json` and
`hooks/hooks.json`, whose hooks run `"${CLAUDE_PLUGIN_ROOT}"/bin/tracon hook
claude`. In the Nix packages, each half's `bin/tracon` links to the same
binary. Nothing is written into Claude Code's settings.

## State, status and label

What a pane shows is built in three layers, each a function of the one
before, so that what the hooks say, what the user cares about and how it is
drawn can change independently.

**State** is what the hooks record, per pane (`src/state.rs`): the agent's
activity, `working`, `waiting.permission`, `waiting.question` or `ready`;
the `outcome` of the last turn, `finished` or `error`, until the next prompt
clears it; the number of `background` tasks running when the turn ended;
`since`, when the state or outcome last changed; `viewed`, when a client
last focused the pane; and the message, harness, session, agent and pin.
Nothing derived is stored, so focusing a pane only records `viewed`.
Adapters report the most specific state their harness distinguishes.

**Status** is what the user cares about, computed when the list is built
from the state and, as a last resort, from what the screen shows
(`src/status.rs`). Statuses form a tree, and a rule naming one matches
every status beneath it:

```
waiting                needs you
  waiting.permission
  waiting.question     a question, a plan to approve, or an MCP elicitation
error                  the turn ended with an error
done                   the turn finished and the pane wasn't focused since
working
idle                   nothing running: seen, or never prompted
  idle.interrupted     the screen shows the turn was interrupted
unknown                agent process present, no hook has reported
```

The first rule that applies gives the status:

| State | Sighting | Status |
| --- | --- | --- |
| `waiting.*` | the spinner | `working` |
| `waiting.*`, `working` | the interrupt notice, or a finished turn with no prompt after it | `idle.interrupted` |
| `waiting.*` | | the same |
| `working` | | `working` |
| `ready`, outcome `error` | | `error` |
| `ready`, outcome `finished`, not seen | | `done` |
| `ready` otherwise | | `idle` |

Seen means `viewed` is at or after `since`. `error` ignores it, so a failed
turn stays visible until the next prompt. Background tasks and pins modify
a status rather than make one: a row reads "done · 2 in background", and
`@tracon-order` names `background` and `pinned` alongside statuses.
`unknown` is derived for a pane with no state whose foreground command is a
known harness, such as `claude` or Nix's `.claude-wrapped`.

**Label** is how a status is shown (`src/label.rs`, `src/format.rs`): a
label, an icon and a style per status, looked up along the tree so that an
entry for `waiting` covers both waits, and a format string that composes a
row from them and the pane's other variables. `list --json` carries all
three layers. The [README](../README.md#configuration) describes the
options.

State is keyed by pane, never by the harness's session id, because `/clear`
and `--resume` change the session id inside one pane. The session id is kept
only so that a late SessionEnd from an earlier session doesn't clear a newer
one.

## Changes and conditions

An adapter turns each payload into a change and a condition on the pane's
current record, which the daemon applies in one step under its lock. A
change enters a state; sets, keeps or clears the outcome, message, harness,
session and agent; and sets the background count when the event reports
it. `since` is kept when the state and outcome are unchanged, so repeated
events don't reset the wait. Conditions combine "the state is under S",
"the wait was raised by agent A" and "the session is S".
`Guarded::apply_to` in `src/store/mod.rs` is the semantics; the tests replay
a sequence of events through the daemon and compare its mirror in tmux with
it at each step.

## Claude Code adapter

`src/harness/claude.rs` maps events to states:

| Event | Change | Condition |
| --- | --- | --- |
| `SessionStart` (not after compaction) | `ready`, no outcome, no background tasks, record the session | always |
| `UserPromptSubmit` | `working`, no outcome, with the prompt as message | always; for a background task's notification, no subagent waits |
| `PreToolUse` for `AskUserQuestion` or `ExitPlanMode` | `waiting.question` | always |
| `PermissionRequest` | `waiting.question` for those tools, otherwise `waiting.permission` | always |
| `Elicitation` | `waiting.question` | always |
| `Notification` `permission_prompt`, `elicitation_dialog`, `elicitation_url_dialog` | `waiting.permission` or `waiting.question` | not already waiting |
| `Notification` `idle_prompt` | `ready`, no outcome | not already `ready` |
| `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `ElicitationResult` | `working` | see below |
| `Stop` | `ready`, outcome `finished`, the count of background tasks | no subagent waits |
| `StopFailure` | `ready`, outcome `error`, with the error as message | no subagent waits |
| `SubagentStop` | `working` | that subagent's wait |
| `SessionEnd` | clear the pane's state | the session matches |

Subagent events carry `agent_id`. A subagent's permission prompt still shows
in the pane, so it raises a wait like the main agent's, and the store records
the subagent as the wait's owner. The main agent's tool and turn events then
leave the wait alone, and only that subagent's own tool events or its
`SubagentStop` end it. A prompt the user typed ends any wait, because none
can be typed while a dialog is on screen. A background task's result reaches
the main agent as a prompt too, its text starting `<task-notification>`,
while another subagent's dialog may still be open; it counts as main-agent
progress.

The permission notification repeats PermissionRequest about six seconds
later, and also follows AskUserQuestion and ExitPlanMode, so it only raises a
wait when none is shown; on its own it covers sandboxed network requests,
which fire nothing else. `idle_prompt` ends a wait or turn that Claude Code
didn't report ending, with no outcome since how it ended is unknown. A pane
already `ready` keeps its outcome and background count, so a finished turn
stays `done` until seen while the prompt sits idle.

Compaction leaves the state alone. Claude Code compacts mid-turn when the
context fills, but also at `/compact` and on its own while a session idles,
after the turn's `Stop`, and no `Stop` follows. Its events, `PreCompact`, the
compacting subagent's `SubagentStop`, `SessionStart` with source `compact` and
`PostCompact`, say nothing about the turn.

## Screen reading

Some changes fire no hook: Claude Code reports neither an approved prompt,
nor an interrupt, nor a prompt cancelled before Claude replied, so the record
stays `waiting` while an approved tool runs, and `waiting` or `working` after
Esc, Ctrl-C or a bare "No". The list reads
the plain-text screen of each pane in one of those states, and its harness
says what it recognises there (`Harness::read_screen`):

| Sighting | Claude Code shows |
| --- | --- |
| `prompt` | a dialog: its footer (`Esc to cancel · Tab to amend`) or options |
| `working` | the spinner line at the start of a row, such as `✢ Blanching… (6s · ↓ 258 tokens)` |
| `interrupted` | `⎿  Interrupted · What should Claude do instead?` after the last prompt sent |
| `finished` | a finished turn's line, such as `✻ Crunched for 28s · done 9:19 PM`, after the last prompt sent |

A dialog outranks a spinner, and a spinner outranks what an earlier turn
left. A prompt cancelled before Claude replied disappears from the screen,
so the turn before it is the last one shown. The status function then takes
the sighting into account, for every harness alike: a `working` sighting
makes a waiting pane `working`, and `interrupted` or `finished` makes a
waiting or working pane `idle.interrupted`. Only panes in those states have
their screens read.

Nothing is written, so the hooks remain the only writers. Every correction
needs a sighting, so a screen the harness doesn't recognise, as after a
Claude Code update changes its text, leaves the status the state gives.
`list --json` shows each pane's `sighting`. The patterns are tested against
real screens kept in `tests/screens/`, one directory per Claude Code
version.

## Daemon

The daemon (`src/daemon.rs`) holds every record in memory, keyed by pane,
and runs the backends, on tokio. It starts on demand: a client that finds
no socket spawns `tracon daemon` in its own process group and retries, so
nothing has to be installed or supervised, and `tracon setup` greets it
when tmux loads so it knows the server before the first hook. A daemon
running from another binary than the one setting up, left by an earlier
installation, is stopped then and replaced, so an upgrade takes effect when
tmux reloads. `tracon daemon --stop` ends it; the answer comes once the
sockets and the lock are released, so the next command starts a fresh one
at once. One lock file keeps it to one daemon per runtime directory.

The runtime directory is `$TRACON_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/tracon`,
else `/tmp/tracon-<uid>`, readable by its user alone. It holds:

| File | Contents |
| --- | --- |
| `rpc.sock` | Requests: JSON-RPC 2.0, one request and one response per line |
| `events.sock` | The stream: a `hello`, a `snapshot` of every record, then every `change` and `removed` in order, each with a sequence number |
| `journal.jsonl` | One line per record written, the last line for a pane winning; replayed at start, then rewritten to one line per live pane |
| `lock` | Held by the running daemon |
| `daemon.log` | Standard error of a daemon started on demand |

The messages are in `src/protocol.rs`; both sides ignore fields they don't
know. Every request carries the client's `TMUX` and `TMUX_PANE`, from which
the tmux backend claims the pane and the server the client runs in, so a
bare `%3` in a command resolves against that server, and the daemon serves
several tmux servers at once. A locator can name the server outright, as
`tmux:%3@/path/to/socket`.

A request runs under the daemon's lock, on the blocking pool since
backends run tmux synchronously: the change is applied, journaled and
published before the next request is handled. A subscriber's snapshot is
taken under the same lock, so nothing falls between it and the stream; one
that stops reading, or falls more than a thousand changes behind, is
dropped and reconnects for a fresh snapshot. At start, and on every list,
records of panes that no longer exist are dropped.

## Mirror

tmux formats and scripts can't read the daemon, so the tmux plugin keeps a
copy of each record in the pane's options. `tracon mirror`
(`src/backend/tmux/mirror.rs`) is a subscriber: it connects to the event
socket, writes every record of its server from the snapshot and clears the
options of panes that have some but no record, then writes each change as
it arrives. Setup starts one per server as a tmux job, so it dies with the
server; a lock per server in the runtime directory sends a second one
away, and the running one's process id is in the global option
`@tracon-mirror-pid`, which setup uses to replace it when the binary
changes and to stop it on removal. When the daemon stops, the mirror waits
for the next one and resyncs from its snapshot. tracon never reads the
options back:

| Option | Meaning |
| --- | --- |
| `@tracon_state` | The state: `working`, `waiting.permission`, `waiting.question` or `ready` |
| `@tracon_outcome` | How the last turn ended, `finished` or `error`, until the next prompt |
| `@tracon_background` | The number of background tasks still running, unset when none |
| `@tracon_since` | Epoch seconds when the state or outcome last changed |
| `@tracon_message` | One line about the state, such as the command awaiting permission |
| `@tracon_harness` | The harness, such as `claude` |
| `@tracon_session` | The harness session id |
| `@tracon_agent` | The subagent whose wait is shown, if any |
| `@tracon_pinned` | `1` when pinned; outlives the state |
| `@tracon_viewed` | When a client last focused the pane, in seconds since the epoch; outlives the state |

Each mirror write ends with `wait-for -S tracon`, so a script blocked on
that channel wakes up as it did before the daemon, a moment after the
change rather than before the hook returns. The picker reads the event
socket instead, and refreshes ages on a slow tick. Every tmux client tracon
runs is started with `-u`: without it, tmux replaces control characters in
its output when the locale isn't UTF-8, and the field separator in
`list-panes -F` output is one.

## Ordering

`order` is a list of rules. The first rule a pane matches decides its group:
a status matches itself and the statuses beneath it, `pinned` matches pinned
panes and `background` panes with background tasks. Within a group, the
pane that entered its state first is listed first. Panes that match no rule
go last. The default is `waiting pinned error done working idle unknown`,
and the tmux option `@tracon-order` replaces it.

`recent_first` names the groups that sort by when a client last focused the
pane instead, most recent first, then panes never focused, newest state
first: a group sorts this way when its rule lies within one of these. The
default is `working idle unknown`, so the attention groups stay queues while
a pane you just saw stays easy to find; `@tracon-recent-first` replaces it.
The focus hook records the time in `@tracon_viewed`, since tmux keeps no
per-pane focus time. Every tunable is in `Config` (`src/config.rs`).

## Focus

`tracon focus` switches a client to the pane's session, window and pane in
one tmux call. The client is the one given with `--client` or
`$TRACON_CLIENT`, as when the picker runs in a popup, otherwise the most
recently active client. With no client attached, focus reports that rather
than spawning a terminal.

A finished pane lists as `idle` rather than `done` once a client focuses
it: setup registers a `pane-focus-in` hook that runs `tracon seen`, which
records the time in `viewed`. tmux reports focus only with `focus-events
on`. That hook doesn't fire for a pane that already has focus, so the hook
command also records a view when a turn finishes in a pane that a client
whose terminal has focus shows.

## Setup

`tracon setup` binds the picker key, registers the focus hook, starts the
mirror, and records `@tracon-bin`, `@tracon-version`, `@tracon-dir` and
`@tracon-bound-key`. It
holds `wait-for -L tracon-setup` while it runs, so concurrent reloads don't
interleave. The hook goes in slot 80 of the `pane-focus-in` array, or the
next free slot if something else uses 80, and setup first removes any tracon
handler from another path, so reloading replaces rather than duplicates.
Changing `@tracon-key` unbinds the previous key if it still opens the picker.

Every registered command first checks that the binary exists and otherwise
does nothing. tmux keeps registrations until its server exits, so an
uninstall can't leave a failing hook. `tracon setup --remove` removes the
binding, the hook and the recorded options. `tracon.tmux` shows a setup
failure in the message line a second later, since TPM discards its output
and no client is attached while the configuration first loads.

## Hooks

A hook exits successfully outside tmux and never fails the agent's tool call:
errors go to standard error and the exit status is 0. It is one process and
one request to the daemon per event, and `hooks.json` gives each hook a
five-second timeout. Outside any pane tracon knows, it returns at once
without starting a daemon.

## Components

| Component | Responsibility |
| --- | --- |
| `src/state.rs` | States, outcomes and records |
| `src/status.rs` | The status function, rules and ordering |
| `src/label.rs`, `src/format.rs` | Labels, icons and styles per status, and the format strings that render rows |
| `src/store/` | Guarded changes and the in-memory store |
| `src/daemon.rs`, `src/protocol.rs`, `src/journal.rs`, `src/runtime.rs` | The daemon, its messages, its journal and its directory |
| `src/client.rs` | The client every command uses, which starts the daemon on demand |
| `src/harness/` | The `Harness` trait and the Claude Code adapter |
| `src/backend/` | The `Backend` trait, locators and the tmux backend, with setup and the mirror |
| `src/list.rs` | The ordered list front ends show |
| `src/cli.rs`, `src/ui.rs`, `src/check.rs` | The command line, the picker and the check |
| `tracon.tmux` | The tmux plugin's entry point |
| `.claude-plugin/plugin.json`, `hooks/hooks.json` | The Claude Code plugin |
| `skills/check/SKILL.md` | `/tracon:check`, which has Claude run the check and explain it |

A pane locator is a backend id and an id only that backend interprets, such
as `tmux:%3`, so other multiplexers and terminals can add backends without
changing the core.

See the [README](../README.md) for setup and usage.
