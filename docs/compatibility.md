# Compatibility and limitations

## Supported environments

The flake supports Apple Silicon macOS and Linux (`aarch64-darwin`,
`aarch64-linux` and `x86_64-linux`). tmux 3.2 or newer is required for
popups; development uses tmux 3.7c. Claude Code is the only harness so far,
and tmux the only backend.

Home Manager installs the Claude plugin as a personal plugin in Claude Code's
`skills` directory, which Claude Code 2.1.157 and later load in every
session. The hooks were checked against Claude Code 2.1.300; earlier versions
may lack some of the events they use. [Managed settings](https://code.claude.com/docs/en/managed-settings)
that set `allowManagedHooksOnly` or `disableAllHooks` stop the hooks, and
tracon then lists Claude panes as `unknown`.

## What Claude Code reports

Observed with Claude Code 2.1.300 on 2026-10-19 and 2026-10-21, logging
every hook event from a real session in a tmux pane:

| Action | Events |
| --- | --- |
| A permission prompt appears | `PreToolUse`, `PermissionRequest`, then `Notification` `permission_prompt` about six seconds later |
| Approve | `PostToolUse` when the tool finishes, `PostToolBatch`, `Stop` |
| Approve within six seconds | the same, with no `permission_prompt` notification |
| Reject with "No" | nothing |
| Reject with a comment | `PostToolBatch`, then `Stop` |
| Esc at a prompt | nothing |
| Ctrl-C during a running tool | nothing |
| AskUserQuestion | `PreToolUse`, `PermissionRequest`, `Notification` `permission_prompt`; after the answer, `PostToolUse`, `PostToolBatch`, `Stop` |
| ExitPlanMode | `PreToolUse`, `PermissionRequest`, `Notification` `permission_prompt` with "needs your approval for the plan" |
| A turn ends while a background subagent runs | `Stop` with the subagent in `background_tasks` |
| That subagent asks permission | `PreToolUse` and `PermissionRequest` with its `agent_id`, then the notification without one |
| After approving it | the subagent's `PostToolUse`, `PostToolBatch` and `SubagentStop`, then a main-agent `UserPromptSubmit` and `Stop` as Claude takes its result |
| `/compact` after a turn | `PreCompact` with trigger `manual`, `SubagentStop` with the compacting subagent's `agent_id`, `SessionStart` with source `compact`, `PostCompact`; no `Stop` |
| A background agent finishes | its `SubagentStop`, then a main-agent `UserPromptSubmit` whose `prompt` is the `<task-notification>` text, and `Stop` |
| An `ask` rule prompts under `--dangerously-skip-permissions` | `PreToolUse`, `PermissionRequest` and the notification, as in default mode; also for a background agent's command |

No `idle_prompt` notification arrived within 70 seconds of a bare "No".
While an approved command ran (checked on 2026-10-21), nothing was written to
the session transcript and the pane title didn't change; the screen showed a
spinner line such as `✶ Kerfuffling… (21s · ↓ 109 tokens)`, which the
permission dialog replaces. The dialog is already drawn when the
`PermissionRequest` hook runs, so a list read straight after that hook
doesn't see the spinner. The spinner starts without its timer, as
`· Nebulizing…`. Esc or "No" at a prompt, and Esc or Ctrl-C while an
approved command ran, all left `⎿  Interrupted · What should Claude do
instead?` above the input box. With vim mode on, the first Esc while Claude
works only leaves insert mode.
tmux's `pane-focus-in` fired on attach, on `select-pane`, and when the
terminal reported focus.

When a session's prompt cache lasts an hour, Claude Code also compacts the
session while it sits idle, about 50 minutes after the turn ends, and prints
`Compacted while idle, before the prompt cache expired`. It does so through
the routine `/compact` uses, so the same events should fire; in four sessions
seen on 2026-10-21, a hook ran as the compaction began and no `Stop` followed.

With Claude Code 2.1.309 on 2026-10-23, cancelling a prompt before Claude
replied, with Esc (twice in vim mode) or Ctrl-C, fired only
`UserPromptSubmit`. The prompt disappeared from the conversation and its
text went back into the input box, with no interrupt notice, so the last
turn shown was the one before, ending in its `✻ Baked for 4s · done 5:55 PM`
line.

## What the hooks module sees

Observed with Claude Code 2.1.310 on 2026-10-28, logging every event a
hooks module receives from a real session, beside the settings hooks:

| Action | Mod events |
| --- | --- |
| A `!` shell command | `prompt.change` with origin `clear` as the box empties, then nothing until the command ends: `session.append` rows through the `command` door with the `<bash-input>` and the `<bash-stdout>`, together, then `turn.start` with the output and a model turn on it, ending in `Stop` and `turn.complete`. A `ToolProgress` render with the background hint comes three seconds in |
| A permission prompt appears | `tool.call`, `tool.check` answering `ask`, `classic.PermissionRequest` |
| Approve | the `ToolGroup` (or `ToolUse`) render's call turns `isRunning`; nothing else until `PostToolUse` |
| Esc at the dialog | `tool.call` resolves with the rejection, a `session.append` note `[Request interrupted by user for tool use]`, then `turn.complete` with reason `answer` and no `Stop` |
| Esc during a turn | the note `[Request interrupted by user]`, then `turn.complete` with reason `aborted` |
| Ctrl-C before a reply | `prompt.submit`, `turn.start`, the note, `turn.complete` `aborted`, and `prompt.change` with origin `engine` putting the text back |
| Ctrl-C during an approved command | the tool's result `Exit code 137`, the note for tool use, `turn.complete` `aborted` |
| A background agent finishes | its `SubagentHandback` delivers an `<agent-message>` as a prompt (`prompt.submit` with origin `peer` and `isMeta`, and a `UserPromptSubmit` with that text), a turn on it, then the `<task-notification>` prompt row |
| Ctrl-U on a draft | a `prompt.edit`, not a `clear` |

Whether clearing a draft with Esc raises a `clear` wasn't checked: the test
session had vim mode on, where Esc leaves insert mode. The `clear` origin
also covers a stashed draft.

## Blind spots

- **Without the hooks module.** Esc, Ctrl-C and "No" without a comment fire
  no settings hook, `Stop` doesn't run after an interrupt, a prompt cancelled
  before Claude replies fires `UserPromptSubmit` alone, and nothing fires
  when a prompt is approved. The hooks module reports each of these; on a
  Claude Code without the mods API the record stays `waiting` or `working`,
  and tracon reads the screen instead: the interrupt notice or a finished
  turn with no prompt after it lists the pane as `interrupted`, and the
  spinner line with no dialog as `working`; see
  [screen reading](design.md#screen-reading).
- **Screen text.** The corrections match text Claude Code draws, checked
  with 2.1.300 and, for cancelled prompts, 2.1.309. If a later version draws
  it differently, the pane shows its recorded state, as above, until the next
  hook; the preview shows the truth.
- **A `!` line not run.** The module reads a cleared draft that starts with
  `!` as a shell command, since a submit empties the box the same way a
  clear or a stash does. A `!` line cleared or stashed instead lists the
  pane as `working` until its next event.
- **Sessions started before the plugin loaded** have no hooks and are listed
  as `unknown` until restarted.
- **A crashed agent** that never ran SessionEnd leaves its last state on the
  pane until the pane closes or an agent starts there again.
- One agent per pane is supported. The pane is found through `$TMUX_PANE`,
  so an agent inside a nested multiplexer reports to the outer tmux pane.

## Testing limits

The tests run the binary and both plugins against real tmux servers, each
with a daemon of its own, as assembled from the source and as packaged by
Nix, with hook payloads and forwarded mod events in place of an agent.
Clients attach in control mode, which needs no terminal; popups are only
checked to open without error. One test runs the real Claude Code binary
without network access and checks that a session started in a new pane
reports through the plugin installed as Home Manager installs it, and
clears its state when it ends; the Nix checks also validate the plugin,
hooks module included, with `claude plugin validate --strict`. The tests
don't cover a real conversation, permission prompt or subagent; those were
checked by hand as described above, the hooks module's reports among them
on 2026-10-29 against a session of Claude Code 2.1.310.

See [troubleshooting](troubleshooting.md) for diagnosing a setup.
