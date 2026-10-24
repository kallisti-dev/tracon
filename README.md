# tracon

Terminal approach control for coding agents.

tracon tells you which of your terminal coding agents need you, and takes you
there. Claude Code reports its state through hooks; tracon keeps that state
on each tmux pane, and a popup picker lists the agent panes with the ones
waiting for you first and the longest wait at the top.

## Requirements

- macOS or Linux
- tmux 3.2 or newer, with `focus-events on`
- Claude Code 2.1.157 or newer; tested with 2.1.300
- Nix with flakes, to build and install it. Other installation methods are
  planned.

## Installation

The flake provides the two halves as separate packages:

- `tmux-plugin`, built with nixpkgs' `tmuxPlugins.mkTmuxPlugin`, which binds
  the picker key and tracks focus
- `claude-plugin`, the Claude Code plugin whose hooks report each session's
  state, with `/tracon:check`

Install each where you configure that program. With Home Manager:

```nix
{ inputs, pkgs, ... }:
let
  tracon = inputs.tracon.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  programs.tmux.plugins = [ tracon.tmux-plugin ];
  programs.tmux.extraConfig = "set -g focus-events on";
  programs.claude-code.plugins.tracon = tracon.claude-plugin;
}
```

Home Manager links the Claude plugin into Claude Code's `skills` directory,
where Claude Code loads it in every session; nothing is written into Claude's
settings. Without Home Manager, link the `claude-plugin` package to
`~/.claude/skills/tracon` and run the tmux plugin's entry point from your tmux
configuration:

```tmux
run-shell /path/to/tmux-plugin/share/tmux-plugins/tracon/tracon.tmux
```

Claude Code sessions that were already running when the plugin was installed
don't have its hooks. Restart them, for example with `claude --continue`.

Without flakes, `default.nix` builds the same packages with the nixpkgs
revision pinned in `flake.lock`, or with a `pkgs` argument you pass.

### Checking the setup

In a Claude session, run `/tracon:check`. Claude runs the check and explains
any problem it reports. From a shell in a tmux pane, run it directly:

```sh
"$(tmux show-options -gqv @tracon-bin)" check
```

### Uninstalling

Remove both packages from your configuration. tmux keeps the key binding and
focus hook until the server exits, but they do nothing once the binary is
gone. To remove them from a running server first:

```sh
"$(tmux show-options -gqv @tracon-bin)" setup --remove
```

## Usage

Press prefix + a to open the picker. It lists agent panes in priority order,
with a live preview of the selected pane below the list.

| Key | Action |
| --- | --- |
| Up, Down, j, k, Ctrl-p, Ctrl-n | Move the selection |
| Enter | Focus the pane, in whatever session it is |
| p | Pin or unpin the pane |
| / | Filter the list (fuzzy) as you type |
| q, Esc | Close |

While filtering, the arrow keys, Ctrl-p, Ctrl-n and Enter work as above,
Ctrl-u clears the filter, and Esc clears it and returns to the list.

When no pane runs an agent, the key shows a message instead.

Each pane is in one of these states. Hooks report the most specific state
Claude Code distinguishes:

| State | Meaning |
| --- | --- |
| `waiting.permission` | A permission prompt is waiting for you |
| `waiting.question` | Claude asked a question or presented a plan, or an MCP server asked for input |
| `error` | The turn ended with an API error |
| `done` | The turn finished and you haven't looked at the pane since |
| `working` | Claude is working |
| `working.background` | The turn ended, but background tasks still run |
| `idle` | You have seen the pane, or the session has had no prompt yet |
| `unknown` | Claude Code runs in the pane, but no hook has reported |

A finished pane becomes `idle` when you focus it, or at once if it finishes
while you are looking at it. Claude Code reports neither an approved prompt,
nor an interrupt, nor a prompt you cancel before it replies, so tracon also
reads the screen of a waiting or working pane: Claude's spinner lists it as
`working`, and its interrupt notice, after Esc, Ctrl-C or a bare "No", or a
finished turn with no prompt after it lists it as `idle`. See
[compatibility](docs/compatibility.md) for details.

The same list and actions are available as commands, for scripts and other
front ends:

```sh
tracon list [--json]        # the ordered list
tracon next                 # focus the top of the list, skipping the current pane
tracon focus %3             # focus a pane on the most recently active client
tracon pin [%3]             # pin a pane, the current one by default
tracon unpin [%3]
```

`tracon` here is the binary recorded in the tmux option `@tracon-bin`.

## Configuration

Set these tmux options. Set `@tracon-key` before the plugin loads; with Home
Manager, put it in the plugin entry's `extraConfig`, since plugins load
before `programs.tmux.extraConfig`.

| Option | Default | Meaning |
| --- | --- | --- |
| `@tracon-key` | `a` | Key in the prefix table that opens the picker |
| `@tracon-order` | `waiting pinned error done working idle unknown` | Groups the list is sorted into |
| `@tracon-recent-first` | `working idle unknown` | Groups that list the pane you viewed last first |
| `@tracon-preview-interval` | `100` | Milliseconds between refreshes of the picker's preview |

Each preview refresh runs `tmux capture-pane` once. On a slow machine, a
longer interval such as `500` makes the picker lighter at the cost of a
choppier preview.

`@tracon-order` is a list of states. Each pane goes in the group of the first
state it matches, where a state also matches the states beneath it
(`waiting` matches `waiting.permission`) and `pinned` matches pinned panes.
Within a group, the pane that has been in its state longest comes first, so
the oldest prompt is at the top. In the groups `@tracon-recent-first` names,
the pane you focused most recently comes first instead, so a finished pane
you just looked at stays near the top of the idle panes; panes you haven't
focused follow, newest state first. Panes that match no entry go last. For
example, to keep pinned panes at the top:

```tmux
set -g @tracon-order 'pinned waiting error done working idle unknown'
```

## Development

`nix develop`, or direnv with the included `.envrc`, provides the tools. The
tests start real tmux servers on disposable sockets under `/tmp`:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

To also run the test that starts Claude Code, without network access:

```sh
CLAUDE_BINARY=$(command -v claude) cargo test real_
```

`nix flake check` runs the tests against the source and against the Nix
packages, with formatting and lint checks and the Claude Code test. On macOS,
that last check needs the Nix build sandbox off or relaxed, as building
claude-code there does.

Releases are tags on `main`. To release, set the same version in
`.claude-plugin/plugin.json` and `Cargo.toml`, commit it to `main`, and push a
tag named `v` followed by that version, such as `v0.2.0`. The release workflow
checks that the tag matches the manifest and is on `main`, runs the checks on
each platform, and creates the GitHub release.

## Documentation

- [Design](docs/design.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Compatibility and limitations](docs/compatibility.md)
