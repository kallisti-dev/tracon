import type { Register } from 'claude-code'

// The mod half of the Claude Code plugin: forwards what the settings hooks
// in hooks.json can't see to `tracon hook claude`, as JSON with an `event`
// field naming the mod event. The adapter in src/harness/claude.rs says
// what each one changes; this file only picks the events and the fields
// it reads, and never fails the session.

type Payload = Record<string, unknown>

type Engine = {
  plugin: { root: string }
  process: {
    run: (
      argv: readonly string[],
      init?: { stdin?: string; timeoutMs?: number },
    ) => Promise<unknown>
  }
}

/** The session the settings hooks named last, for the record's session. */
let session: string | undefined
/** The prompt box as the person left it, so a cleared `!` line is known. */
let draft = ''
/** Tool calls already reported as running, so a redraw reports nothing. */
const running = new Set<string>()

async function report($: Engine, payload: Payload) {
  try {
    await $.process.run([`${$.plugin.root}/bin/tracon`, 'hook', 'claude'], {
      stdin: JSON.stringify({ session_id: session, ...payload }),
      timeoutMs: 5000,
    })
  } catch {
    // A missing binary or daemon is tracon's problem, never Claude's.
  }
}

function firstText(content: readonly unknown[]): string | undefined {
  for (const block of content) {
    if (block && typeof block === 'object' && (block as { type?: string }).type === 'text') {
      return (block as { text?: string }).text
    }
  }
  return undefined
}

async function reportRunning(
  $: Engine,
  calls: ReadonlyArray<{ tool_use_id: string; tool: string; isRunning: boolean }>,
) {
  for (const call of calls) {
    if (call.isRunning && !running.has(call.tool_use_id)) {
      running.add(call.tool_use_id)
      await report($, { event: 'ui.render', running: true, tool: call.tool })
    }
  }
}

export const register: Register = on => {
  on('classic.SessionStart', ($, e, next) => {
    session = e.session_id
    return next(e)
  })
  on('classic.UserPromptSubmit', ($, e, next) => {
    session = e.session_id
    return next(e)
  })
  on('prompt.edit', ($, e, next) => {
    draft = e.text.slice(0, e.start) + e.inputText + e.text.slice(e.end)
    return next(e)
  })
  // A `!` line runs in the shell without a prompt event: the box is
  // cleared and nothing else says so until the command ends.
  on('prompt.change', async ($, e, next) => {
    const before = draft
    draft = e.text
    if (e.origin.kind === 'clear' && before.trimStart().startsWith('!')) {
      await report($, { event: 'prompt.change', origin: 'clear', before })
    }
    return next(e)
  })
  on('prompt.submit', async ($, e, next) => {
    await report($, {
      event: 'prompt.submit',
      text: e.text.slice(0, 2000),
      origin: e.origin.kind,
      is_meta: e.isMeta === true,
    })
    return next(e)
  })
  on('turn.start', async ($, e, next) => {
    if (!e.agentId) {
      running.clear()
      await report($, { event: 'turn.start', text: e.text.slice(0, 200) })
    }
    return next(e)
  })
  on('turn.complete', async ($, e, next) => {
    if (!e.agentId) {
      await report($, { event: 'turn.complete', reason: e.reason })
    }
    return next(e)
  })
  // The engine's own rows: the note an interrupt or a refusal leaves, and
  // a shell command's output.
  on('session.append', { door: 'note' }, async ($, e, next) => {
    const text = firstText(e.message.content)
    if (text?.startsWith('[Request interrupted by user')) {
      await report($, { event: 'session.append', door: 'note', text: text.slice(0, 200), agent_id: e.agentId })
    }
    return next(e)
  })
  on('session.append', { door: 'command' }, async ($, e, next) => {
    const text = firstText(e.message.content)
    if (text?.startsWith('<bash-stdout>')) {
      await report($, { event: 'session.append', door: 'command', text: text.slice(0, 200), agent_id: e.agentId })
    }
    return next(e)
  })
  // Nothing else says a prompt was approved: the tool's row starts running.
  on('ui.render', { component: 'ToolGroup' }, async ($, e, next) => {
    await reportRunning($, e.props.calls)
    return next(e)
  })
  on('ui.render', { component: 'ToolUse' }, async ($, e, next) => {
    await reportRunning($, [e.props])
    return next(e)
  })
}
