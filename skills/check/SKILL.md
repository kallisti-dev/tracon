---
description: Check whether tracon tracks this Claude session's state, and explain how to fix any problem found
disable-model-invocation: true
allowed-tools: Bash(tracon check *)
---

Run `tracon check --session ${CLAUDE_SESSION_ID}` with the Bash tool. It
reports whether the tmux server, this pane and Claude Code are set up so that
tracon tracks this session's state.

Summarize the result. For each line marked `problem`, explain the cause and the
fix, using `${CLAUDE_PLUGIN_ROOT}/docs/troubleshooting.md`. Change configuration
only if the user asks.
