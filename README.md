# tracon

Terminal approach control for coding agents.

tracon tells you which of your terminal coding agents need you, and takes you
there. Claude Code reports its state through hooks; a small daemon keeps
that state per tmux pane, and a popup picker lists the agent panes with the
ones waiting for you first and the longest wait at the top.

## Requirements

- macOS or Linux
- tmux 3.2 or newer, with `focus-events on`
- Claude Code 2.1.157 or newer; tested with 2.1.310. The plugin's hooks
  module, which reports interrupts, approvals and shell commands, needs the
  mods API: 2.1.310 loads it, 2.1.295 refuses it, and what a version in
  between or an older one's settings hooks do with it wasn't checked
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

Each pane is listed with a status, derived from what Claude Code's hooks
reported, and shown with a label:

| Status | Shown as | Meaning |
| --- | --- | --- |
| `waiting.permission` | needs approval | A permission prompt is waiting for you |
| `waiting.question` | has a question | Claude asked a question or presented a plan, or an MCP server asked for input |
| `error` | failed | The turn ended with an API error |
| `done` | done | The turn finished and you haven't looked at the pane since |
| `working` | working | Claude is working |
| `idle` | idle | You have seen the pane, or the session has had no prompt yet |
| `idle.interrupted` | interrupted | You interrupted Claude, refused a prompt or cancelled one |
| `unknown` | not reporting | Claude Code runs in the pane, but no hook has reported |

Background tasks that outlive a turn show on the row, as in "done · 2 in
background". A finished pane becomes `idle` when you focus it, or at once if
it finishes while you are looking at it. Claude Code's settings hooks report
neither an approved prompt, nor an interrupt, nor a prompt you cancel before
it replies, nor a `!` shell command; the plugin's hooks module reports those.
Where Claude Code has no mods API, tracon reads the screen of a waiting or
working pane instead: Claude's spinner lists it as `working`, and its
interrupt notice, after Esc, Ctrl-C or a bare "No", or a finished turn with
no prompt after it lists it as `interrupted`. See
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

Every command talks to a daemon that starts on demand and keeps running
until `tracon daemon --stop`, or until a reload of the tmux configuration
finds it running from another binary and replaces it; it lives in
`$XDG_RUNTIME_DIR/tracon`, or `/tmp/tracon-<uid>`. For tmux status lines and scripts, the tmux plugin
mirrors what the hooks recorded for each pane into its options,
`#{@tracon_state}`, `#{@tracon_outcome}` and the others listed in the
[design](docs/design.md#mirror), and `tmux wait-for tracon` returns after
every change.

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
| `@tracon-labels` | see above | Labels per status, as `status=label,…` |
| `@tracon-icons` | `waiting=?,error=!,done=✓,working=◐,idle=○,unknown=·` | Icons per status |
| `@tracon-styles` | `waiting=bold yellow,error=bold red,done=green,working=blue,idle=dim,unknown=dim` | Styles per status |
| `@tracon-format` | see below | The row format for the picker and `tracon list` |

Each preview refresh runs `tmux capture-pane` once. On a slow machine, a
longer interval such as `500` makes the picker lighter at the cost of a
choppier preview.

`@tracon-order` is a list of statuses. Each pane goes in the group of the
first status it matches, where a status also matches the statuses beneath
it (`waiting` matches `waiting.permission`), `pinned` matches pinned panes
and `background` matches panes with background tasks. Within a group, the
pane that has been in its state longest comes first, so the oldest prompt
is at the top. In the groups `@tracon-recent-first` names, the pane you
focused most recently comes first instead, so a finished pane you just
looked at stays near the top of the idle panes; panes you haven't focused
follow, newest state first. Panes that match no entry go last. For example,
to keep pinned panes at the top:

```tmux
set -g @tracon-order 'pinned waiting error done working idle unknown'
```

`@tracon-labels`, `@tracon-icons` and `@tracon-styles` each add to or
replace the defaults, entry by entry. An entry for a status also covers the
statuses beneath it unless they have entries of their own, so
`waiting=needs you` labels both waits. A style is words such as `bold`,
`dim`, `italic`, `underline` and `reversed`, a colour name, `bright-` and a
name, `#rrggbb` or a number up to 255, and `bg:` before a colour for the
background:

```tmux
set -g @tracon-labels 'waiting.permission=approve?,done=unread'
set -g @tracon-styles 'done=bold green,idle=8'
```

`@tracon-format` composes a row, in the style of starship. `{var}` inserts
a variable, `{message|title}` the first of several with a value, and
`{label:<16}`, `{age:>4}` or `{title:^20}` pads to a width; `{title:.30}`
cuts to a length. `[text](style)` styles what it encloses, and `({style})`
is the status's style. `(…)` is a group shown only when a variable inside it
has a value, and a backslash keeps the next character. The default is:

```
{pin:<1}[{icon} {label:<16}]({style}) {age:>4}  [{location:<18}](bold) [{harness:<7}](dim) {message|title}( · {background} in background)
```

The variables are `icon`, `label`, `status` and `style`; `state`, `outcome`
and `background`, as the hooks recorded them; `age`, the time in the
state; `location`, `harness`, `title` and `message`; and `pin`, `*` when
the pane is pinned.

## Development

`nix develop`, or direnv with the included `.envrc`, provides the tools. The
tests start real tmux servers on disposable sockets under `/tmp`, each with
a daemon in a directory of its own:

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
