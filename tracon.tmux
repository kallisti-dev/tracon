#!/usr/bin/env bash
# Plugin entry point, run by Home Manager, TPM or run-shell in a tmux
# configuration.
# SPDX-License-Identifier: BSD-3-Clause
dir=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd)
tracon=$dir/bin/tracon

fail() {
  # display-message reads the message through a format, so it needs no
  # quoting; run-shell expands formats too, hence ##. Clients attach after
  # the configuration loads when the server starts, so it is shown a second
  # later. TPM discards this script's output.
  tmux set-option -g @tracon-error "$1"
  tmux run-shell -b -d 1 "tmux display-message -d 5000 'tracon: ##{@tracon-error}'"
  exit 1
}

[[ -x $tracon ]] || fail "$tracon is missing; reinstall the plugin"
if ! output=$("$tracon" setup --dir "$dir" 2>&1); then
  message=${output##*$'\n'}
  fail "${message#tracon: }"
fi
tmux set-option -gu @tracon-error
