# Design

tracon keeps one state per agent pane and lists panes in priority order. The
core owns that state and exposes it through surfaces; the picker and every
other front end only read the list and call commands.

| Surface | Commands | Role |
| --- | --- | --- |
| Ingest | `tracon hook <harness>` | Harness adapters turn hook payloads into state changes |
| Query | `tracon list [--json]` | The ordered list with each pane's state |
| Command | `tracon focus`, `next`, `pin`, `unpin`, `seen` | Act on panes through a backend |

The library (`src/lib.rs`) holds the state model, stores, harness adapters
and backends. The binary adds the command line, the picker and the check.

## Plugin loading

tracon has two halves, installed separately. The tmux plugin's entry point,
`tracon.tmux`, runs `tracon setup` from its `bin/` directory. The Claude Code
plugin is a directory with `.claude-plugin/plugin.json` and
`hooks/hooks.json`, whose hooks run `"${CLAUDE_PLUGIN_ROOT}"/bin/tracon hook
claude`. In the Nix packages, each half's `bin/tracon` links to the same
binary. Nothing is written into Claude Code's settings.

## States

States form a tree, and a rule naming a state matches every state beneath
it. Adapters report the most specific state their harness distinguishes:

```
waiting                needs you, reason unknown
  waiting.permission
  waiting.question     a question, a plan to approve, or an MCP elicitation
error
done                   turn finished, not yet looked at
idle                   seen, or never prompted
working
  working.background   turn ended but background tasks still run
unknown                agent process present, no hook has reported
```

`unknown` is never stored. The list derives it for a pane with no state whose
foreground command is a known harness, such as `claude` or Nix's
`.claude-wrapped`.

State is keyed by pane, never by the harness's session id, because `/clear`
and `--resume` change the session id inside one pane. The session id is kept
only so that a late SessionEnd from an earlier session doesn't clear a newer
one.

## Changes and conditions

An adapter turns each payload into a change and a condition on the pane's
current record, which the store applies atomically. A change enters a state,
keeping `since` when the pane is already in that exact state so repeated
events don't reset the wait, and sets, keeps or clears the message, harness,
session and agent. Conditions combine "the state is under S", "the wait was
raised by agent A" and "the session is S". `Guarded::apply_to` in
`src/store/mod.rs` is the reference semantics; the tests replay a sequence of
events through the tmux store and compare each step with it.

## Claude Code adapter

`src/harness/claude.rs` maps events to states:

| Event | Change | Condition |
| --- | --- | --- |
| `SessionStart` (not after compaction) | `idle`, record the session | always |
| `UserPromptSubmit` | `working`, with the prompt as message | always |
| `PreToolUse` for `AskUserQuestion` or `ExitPlanMode` | `waiting.question` | always |
| `PermissionRequest` | `waiting.question` for those tools, otherwise `waiting.permission` | always |
| `Elicitation` | `waiting.question` | always |
| `Notification` `permission_prompt`, `elicitation_dialog`, `elicitation_url_dialog` | `waiting.permission` or `waiting.question` | not already waiting |
| `Notification` `idle_prompt` | `idle` | not `done` or `working.background` |
| `PostToolUse`, `PostToolUseFailure`, `PostToolBatch`, `PreCompact`, `ElicitationResult` | `working` | see below |
| `Stop` | `done`, or `working.background` with background tasks | no subagent waits |
| `StopFailure` | `error`, with the error as message | no subagent waits |
| `SubagentStop` | `working` | that subagent's wait |
| `SessionEnd` | clear the pane's state | the session matches |

Subagent events carry `agent_id`. A subagent's permission prompt still shows
in the pane, so it raises a wait like the main agent's, and the store records
the subagent as the wait's owner. The main agent's tool and turn events then
leave the wait alone, and only that subagent's own tool events or its
`SubagentStop` end it. A new prompt ends any wait, because a prompt can't be
typed while one is on screen.

The permission notification repeats PermissionRequest about six seconds
later, and also follows AskUserQuestion and ExitPlanMode, so it only raises a
wait when none is shown; on its own it covers sandboxed network requests,
which fire nothing else. `idle_prompt` ends a wait or turn that Claude Code
didn't report ending, but leaves `done` until the pane is seen, and
`working.background` until Claude takes the tasks' results, since the prompt
sits idle while they run.

Nothing fires when a prompt is approved either, so the record stays waiting
while the approved tool runs. The list reads the screen of each waiting pane
(`Harness::shows_work`): one showing Claude Code's spinner line, and no
dialog footer, is listed as `working`. Nothing is written, so the hooks
remain the only writers, and a screen that doesn't match leaves the pane
waiting.

## Store

The tmux store keeps the record in pane options, which die with the pane,
are readable by other tools, and need no daemon:

| Option | Meaning |
| --- | --- |
| `@tracon_state` | The state |
| `@tracon_since` | Epoch seconds when the pane entered the state |
| `@tracon_message` | One line about the state, such as the command awaiting permission |
| `@tracon_harness` | The harness, such as `claude` |
| `@tracon_session` | The harness session id |
| `@tracon_agent` | The subagent whose wait is shown, if any |
| `@tracon_pinned` | `1` when pinned; outlives the state |

Each change is one tmux invocation: `if-shell -F` with the condition as a
tmux format, running the option changes, followed by `wait-for -S tracon`.
The picker blocks in `wait-for tracon` and refreshes when it returns, with a
slow refresh for ages. Loading reads every pane's options with one
`list-panes -a`, using a control character as the field separator; stores
replace it in values they write.

## Ordering

`order` is a list of rules. The first rule a pane matches decides its group:
a state matches itself and the states beneath it, and `pinned` matches pinned
panes. Within a group, the pane that entered its state first is listed
first. Panes that match no rule go last. The default is `waiting pinned error
done working idle unknown`, and the tmux option `@tracon-order` replaces it.
Every tunable is in `Config` (`src/config.rs`).

## Focus

`tracon focus` switches a client to the pane's session, window and pane in
one tmux call. The client is the one given with `--client` or
`$TRACON_CLIENT`, as when the picker runs in a popup, otherwise the most
recently active client. With no client attached, focus reports that rather
than spawning a terminal.

`done` becomes `idle` when the pane gains focus: setup registers a
`pane-focus-in` hook that runs `tracon seen`. tmux reports focus only with
`focus-events on`. That hook doesn't fire for a pane that already has focus,
so the hook command also marks a finished turn seen when a client whose
terminal has focus shows the pane.

## Setup

`tracon setup` binds the picker key, registers the focus hook, and records
`@tracon-bin`, `@tracon-version`, `@tracon-dir` and `@tracon-bound-key`. It
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
errors go to standard error and the exit status is 0. It runs one tmux
process per event, and `hooks.json` gives each hook a five-second timeout.

## Components

| Component | Responsibility |
| --- | --- |
| `src/state.rs` | States, records, rules and ordering |
| `src/store/` | The `Store` trait, guarded changes and the tmux pane-option store |
| `src/harness/` | The `Harness` trait and the Claude Code adapter |
| `src/backend/` | The `Backend` trait, locators and the tmux backend, with setup |
| `src/list.rs` | The ordered list front ends show |
| `src/cli.rs`, `src/ui.rs`, `src/check.rs` | The command line, the picker and the check |
| `tracon.tmux` | The tmux plugin's entry point |
| `.claude-plugin/plugin.json`, `hooks/hooks.json` | The Claude Code plugin |
| `skills/check/SKILL.md` | `/tracon:check`, which has Claude run the check and explain it |

A pane locator is a backend id and an id only that backend interprets, such
as `tmux:%3`, so other multiplexers and terminals can add backends without
changing the core.

See the [README](../README.md) for setup and usage.
