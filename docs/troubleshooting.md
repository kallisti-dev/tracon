# Troubleshooting

Start with the check. In a Claude session, run `/tracon:check`; from a shell
in a tmux pane, run:

```sh
"$(tmux show-options -gqv @tracon-bin)" check
```

Each line is `ok`, `info` or `problem`. The problems it reports:

## tracon is not set up in this tmux server

The tmux plugin hasn't run. Check that it is in your tmux configuration and
reload it, for example with `tmux source-file ~/.config/tmux/tmux.conf`. If
setup failed, its message is in the global option `@tracon-error`:

```sh
tmux show-options -gqv @tracon-error
```

## tmux runs a different version, or the binary no longer exists

The server still runs the binary from an earlier installation, such as a
replaced Nix store path. Reload the tmux configuration.

## The key no longer opens the picker

Something bound the key after tracon loaded. Bind your other command to a
different key, or set `@tracon-key` to another key before the plugin loads,
then reload.

## The pane-focus-in hook is missing, or focus-events is off

Finished panes then stay `done` after you look at them. Reload the
configuration to register the hook, and set `focus-events on`. tmux only
reports focus changes from terminals that send focus events; WezTerm and kitty
do.

If the hook goes missing again after a reload, something in your
configuration sets `pane-focus-in` without an index, which replaces every
`pane-focus-in` hook, tracon's included. Give that hook an index, such as
`set-hook -g 'pane-focus-in[0]' …`, or append it with `set-hook -ga`.

## Claude Code doesn't load the tracon plugin

Install the `claude-plugin` package where Claude Code loads plugins: with Home
Manager, `programs.claude-code.plugins.tracon`; otherwise a link at
`~/.claude/skills/tracon`. `/plugin` in a Claude session lists what loaded and
any errors. A version difference between the two halves means one was updated
without the other; install both from the same revision.

## Claude Code in a pane has reported no state

That session started before the plugin was installed, or hooks are disabled
by managed settings (`/status` shows which apply). Restart it in the pane,
for example with `claude --continue`.

## This pane records no session, or another one

The check ran from a Claude session whose hooks didn't reach this pane: the
session started before the plugin loaded, or `$TMUX_PANE` doesn't name the
pane, as inside a nested multiplexer. Restart the session in this pane.

## A pane shows the wrong state

Claude Code reports nothing when you approve a prompt, interrupt it, press
Esc at a prompt, answer "No" without a comment, or cancel a prompt before
Claude replies. tracon covers those by
reading the screen, and if a Claude Code update changed the text it reads,
the pane keeps its last state until your next prompt. See
[compatibility](compatibility.md#blind-spots). To inspect what a pane
records, and what tracon read on its screen (`sighting`):

```sh
tmux show-options -p -t %3 | grep @tracon_
tracon list --json
```
