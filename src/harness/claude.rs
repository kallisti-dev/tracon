//! Claude Code, through the Claude plugin's two channels: the settings
//! hooks in `hooks/hooks.json`, whose payloads carry `hook_event_name`, and
//! the hooks module `hooks/register.ts`, which forwards the mod events the
//! settings hooks can't see, under `event`.
//!
//! Subagent events carry `agent_id`. A subagent's permission prompt still
//! shows in this pane, so it raises a wait like the main agent's, and the
//! store records which agent raised it: that agent's later events end the
//! wait, and the main agent's tool and turn events leave it alone. See
//! `docs/compatibility.md` for what Claude Code reports and what it doesn't.

mod screen;

use serde_json::Value;

use super::{Harness, command_is};
use crate::screen::Sighting;
use crate::store::{Change, Cond, Field, Guarded, Update, valid_id};
use crate::{Error, Outcome, State, one_line};

pub struct Claude;

/// Tools that ask the user something rather than asking to act.
const QUESTION_TOOLS: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];

const MESSAGE_LENGTH: usize = 160;

/// The note the engine appends to the conversation when the user cuts a
/// turn short: `[Request interrupted by user]`, or `… for tool use` at a
/// refused prompt.
const INTERRUPTED_NOTE: &str = "[Request interrupted by user";

fn text<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(Value::as_str)
}

fn message(text: &str) -> Field<String> {
    let line = one_line(text, MESSAGE_LENGTH);
    if line.is_empty() {
        Field::Clear
    } else {
        Field::Set(line)
    }
}

/// What a tool call is about, such as `Bash: cargo test`.
fn tool_summary(payload: &Value) -> String {
    let tool = text(payload, "tool_name").unwrap_or("tool");
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let detail = match tool {
        "AskUserQuestion" => input
            .pointer("/questions/0/question")
            .and_then(Value::as_str),
        "ExitPlanMode" => return "Plan ready for approval".to_owned(),
        _ => [
            "command",
            "file_path",
            "notebook_path",
            "url",
            "query",
            "pattern",
            "description",
        ]
        .iter()
        .find_map(|field| input.get(field).and_then(Value::as_str)),
    };
    match detail {
        Some(detail) if tool == "AskUserQuestion" => detail.to_owned(),
        Some(detail) => format!("{tool}: {detail}"),
        None => tool.to_owned(),
    }
}

fn waiting_by(agent: Option<&str>) -> Cond {
    Cond::All(vec![
        Cond::StateUnder(State::known(State::WAITING)),
        Cond::Agent(agent.map(str::to_owned)),
    ])
}

/// Main-agent progress doesn't end a wait a subagent raised: its prompt is
/// still on screen.
fn unless_a_subagent_waits() -> Cond {
    Cond::negated(Cond::All(vec![
        Cond::StateUnder(State::known(State::WAITING)),
        Cond::negated(Cond::Agent(None)),
    ]))
}

fn unless_waiting() -> Cond {
    Cond::negated(Cond::StateUnder(State::known(State::WAITING)))
}

/// The gist of a prompt the engine delivered to the main agent rather than
/// the user typed: a background task's notification, or another agent's
/// message. `None` for a typed prompt.
fn delivered(prompt: &str) -> Option<&str> {
    let prompt = prompt.trim_start();
    if let Some(notice) = prompt.strip_prefix("<task-notification>") {
        return Some(
            notice
                .split_once("<summary>")
                .and_then(|(_, rest)| rest.split_once("</summary>"))
                .map_or(notice, |(summary, _)| summary),
        );
    }
    if prompt.starts_with("<agent-message") {
        return Some(prompt.split_once('>').map_or(prompt, |(_, body)| body));
    }
    None
}

/// Who a payload comes from, on either channel.
struct Origin<'a> {
    /// The subagent, or `None` for the main agent.
    agent: Option<&'a str>,
    session: Option<&'a str>,
}

impl<'a> Origin<'a> {
    fn of(payload: &'a Value) -> Result<Self, Error> {
        let agent = text(payload, "agent_id").filter(|id| !id.is_empty());
        if let Some(agent) = agent
            && !valid_id(agent)
        {
            return Err(Error::Payload(format!("invalid agent_id {agent:?}")));
        }
        Ok(Origin {
            agent,
            session: text(payload, "session_id").filter(|id| valid_id(id)),
        })
    }

    /// An update entering `state`, with the harness and the session.
    fn enter(&self, state: &'static str) -> Update {
        let mut update = Update::new(State::known(state));
        update.harness = Field::Set("claude".to_owned());
        if let Some(session) = self.session {
            update.session = Field::Set(session.to_owned());
        }
        update
    }
}

fn guarded(update: Update, when: Cond) -> Option<Guarded> {
    Some(Guarded::when(Change::Set(update), when))
}

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn runs_as(&self, command: &str) -> bool {
        command_is(command, "claude")
    }

    fn read_screen(&self, text: &str) -> Option<Sighting> {
        screen::read(text)
    }

    fn interpret(&self, payload: &Value) -> Result<Option<Guarded>, Error> {
        let origin = Origin::of(payload)?;
        if let Some(event) = text(payload, "event") {
            return Ok(from_mod(event, payload, &origin));
        }
        let event = text(payload, "hook_event_name")
            .ok_or_else(|| Error::Payload("no hook_event_name or event".into()))?;
        from_hook(event, payload, &origin)
    }
}

/// The change a settings hook's payload makes.
fn from_hook(event: &str, payload: &Value, origin: &Origin) -> Result<Option<Guarded>, Error> {
    let agent = origin.agent;
    Ok(match event {
        "SessionStart" => {
            // Compaction restarts the session, mid-turn or between
            // turns, and leaves the turn as it was.
            if text(payload, "source") == Some("compact") {
                return Ok(None);
            }
            let mut update = origin.enter(State::READY);
            update.outcome = Field::Clear;
            update.background = Some(0);
            update.message = Field::Clear;
            update.agent = Field::Clear;
            guarded(update, Cond::Always)
        }
        "SessionEnd" => {
            let session = origin
                .session
                .ok_or_else(|| Error::Payload("no valid session_id".into()))?;
            Some(Guarded::when(
                Change::Clear,
                Cond::Session(session.to_owned()),
            ))
        }
        // A subagent's turn has no prompt of its own.
        "UserPromptSubmit" if agent.is_none() => {
            let prompt = text(payload, "prompt").unwrap_or("");
            prompted(origin, prompt, delivered(prompt))
        }
        // PermissionRequest also fires for these tools; PreToolUse is
        // kept for modes that skip the permission check.
        "PreToolUse" | "PermissionRequest" => {
            let tool = text(payload, "tool_name").unwrap_or("");
            let question = QUESTION_TOOLS.contains(&tool);
            if event == "PreToolUse" && !question {
                return Ok(None);
            }
            let mut update = origin.enter(if question {
                State::WAITING_QUESTION
            } else {
                State::WAITING_PERMISSION
            });
            update.message = message(&tool_summary(payload));
            update.agent = agent.map_or(Field::Clear, |id| Field::Set(id.to_owned()));
            guarded(update, Cond::Always)
        }
        "Elicitation" => {
            let mut update = origin.enter(State::WAITING_QUESTION);
            let server = text(payload, "mcp_server_name").unwrap_or("MCP server");
            let request = text(payload, "message").unwrap_or("asks for input");
            update.message = message(&format!("{server}: {request}"));
            update.agent = agent.map_or(Field::Clear, |id| Field::Set(id.to_owned()));
            guarded(update, Cond::Always)
        }
        "Notification" => match text(payload, "notification_type") {
            // A late repeat of PermissionRequest, except for a sandboxed
            // command's network request, which raises only this.
            Some(
                kind @ ("permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog"),
            ) => {
                let mut update = origin.enter(if kind == "permission_prompt" {
                    State::WAITING_PERMISSION
                } else {
                    State::WAITING_QUESTION
                });
                update.message = message(text(payload, "message").unwrap_or(""));
                update.agent = Field::Clear;
                guarded(update, unless_waiting())
            }
            // The input prompt has sat untouched, so a wait or turn it
            // still shows ended some way Claude Code didn't report. A
            // pane already ready keeps its outcome.
            Some("idle_prompt") => {
                let mut update = origin.enter(State::READY);
                update.outcome = Field::Clear;
                update.message = Field::Clear;
                update.agent = Field::Clear;
                guarded(
                    update,
                    Cond::negated(Cond::StateUnder(State::known(State::READY))),
                )
            }
            _ => None,
        },
        "PostToolUse" | "PostToolUseFailure" | "PostToolBatch" | "ElicitationResult" => {
            let mut update = origin.enter(State::WORKING);
            if matches!(event, "PostToolUse" | "PostToolUseFailure") {
                update.message = message(&tool_summary(payload));
            }
            progress(update, agent)
        }
        "SubagentStop" => agent.and_then(|agent| {
            let mut update = origin.enter(State::WORKING);
            update.agent = Field::Clear;
            guarded(update, waiting_by(Some(agent)))
        }),
        "Stop" if agent.is_none() => {
            let background = payload
                .get("background_tasks")
                .and_then(Value::as_array)
                .map_or(0, |tasks| tasks.len() as u32);
            let mut update = origin.enter(State::READY);
            update.outcome = Field::Set(Outcome::Finished);
            update.background = Some(background);
            update.message = message(text(payload, "last_assistant_message").unwrap_or(""));
            guarded(update, unless_a_subagent_waits())
        }
        "StopFailure" if agent.is_none() => {
            let mut update = origin.enter(State::READY);
            update.outcome = Field::Set(Outcome::Error);
            let error = text(payload, "error").unwrap_or("unknown");
            let detail =
                text(payload, "error_details").or_else(|| text(payload, "last_assistant_message"));
            update.message = message(&match detail {
                Some(detail) => format!("{error}: {detail}"),
                None => error.to_owned(),
            });
            guarded(update, unless_a_subagent_waits())
        }
        _ => None,
    })
}

/// The change a mod event makes, as the hooks module forwards it: the
/// event's name under `event` and the fields read here.
fn from_mod(event: &str, payload: &Value, origin: &Origin) -> Option<Guarded> {
    let main = origin.agent.is_none();
    let flag = |field: &str| payload.get(field).and_then(Value::as_bool).unwrap_or(false);
    match event {
        // Typed, or delivered by the engine: a task's notification, another
        // agent's message, a scheduled prompt.
        "prompt.submit" if main => {
            let prompt = text(payload, "text").unwrap_or("");
            let typed = !flag("is_meta")
                && matches!(
                    text(payload, "origin"),
                    Some("composer" | "bridge" | "sdk" | "unclassified") | None
                );
            let gist = if typed {
                delivered(prompt)
            } else {
                Some(delivered(prompt).unwrap_or(prompt))
            };
            prompted(origin, prompt, gist)
        }
        // A turn that no prompt started: the one on a shell command's output.
        "turn.start" if main => {
            let mut update = origin.enter(State::WORKING);
            update.outcome = Field::Clear;
            guarded(update, unless_a_subagent_waits())
        }
        "turn.complete" if main => match text(payload, "reason") {
            Some("aborted") => {
                let mut update = origin.enter(State::READY);
                update.outcome = Field::Set(Outcome::Interrupted);
                update.agent = Field::Clear;
                guarded(update, unless_a_subagent_waits())
            }
            Some("error") => {
                let mut update = origin.enter(State::READY);
                update.outcome = Field::Set(Outcome::Error);
                guarded(update, unless_a_subagent_waits())
            }
            // A refused prompt ends the turn as answered, after the note
            // that says otherwise.
            _ => {
                let mut update = origin.enter(State::READY);
                update.outcome = Field::Set(Outcome::Finished);
                guarded(
                    update,
                    Cond::All(vec![
                        unless_a_subagent_waits(),
                        Cond::negated(Cond::Outcome(Some(Outcome::Interrupted))),
                    ]),
                )
            }
        },
        "session.append" => {
            let row = text(payload, "text").unwrap_or("").trim_start();
            match text(payload, "door") {
                Some("note") if row.starts_with(INTERRUPTED_NOTE) => match origin.agent {
                    None => {
                        let mut update = origin.enter(State::READY);
                        update.outcome = Field::Set(Outcome::Interrupted);
                        update.agent = Field::Clear;
                        guarded(update, unless_a_subagent_waits())
                    }
                    // The subagent's own prompt was refused; the main agent
                    // goes on.
                    Some(agent) => {
                        let mut update = origin.enter(State::WORKING);
                        update.agent = Field::Clear;
                        guarded(update, waiting_by(Some(agent)))
                    }
                },
                // The shell command's output arrived, so the command is over.
                Some("command") if main && row.starts_with("<bash-stdout>") => {
                    let mut update = origin.enter(State::READY);
                    update.outcome = Field::Set(Outcome::Finished);
                    guarded(update, Cond::StateUnder(State::known(State::WORKING)))
                }
                _ => None,
            }
        }
        // The prompt box emptied of a `!` line: a shell command started,
        // which no prompt event reports.
        "prompt.change" if main => {
            let before = text(payload, "before").unwrap_or("").trim_start();
            if text(payload, "origin") != Some("clear") || !before.starts_with('!') {
                return None;
            }
            let mut update = origin.enter(State::WORKING);
            update.message = message(before);
            update.agent = Field::Clear;
            guarded(update, Cond::Always)
        }
        // A tool's row started running: the prompt for it was approved, or
        // none was needed.
        "ui.render" if main && flag("running") => {
            guarded(origin.enter(State::WORKING), unless_a_subagent_waits())
        }
        _ => None,
    }
}

/// A prompt reached the main agent: typed, it ends any wait, since none
/// can be typed while a dialog is on screen; delivered by the engine with
/// `gist`, possibly while a subagent's dialog is open, it counts as progress.
fn prompted(origin: &Origin, prompt: &str, gist: Option<&str>) -> Option<Guarded> {
    let mut update = origin.enter(State::WORKING);
    update.outcome = Field::Clear;
    match gist {
        Some(gist) => {
            update.message = message(gist);
            guarded(update, unless_a_subagent_waits())
        }
        None => {
            update.message = message(prompt);
            update.agent = Field::Clear;
            guarded(update, Cond::Always)
        }
    }
}

/// Work went on: the subagent's own prompt was answered, or the main agent
/// moved while no subagent waits.
fn progress(mut update: Update, agent: Option<&str>) -> Option<Guarded> {
    match agent {
        Some(agent) => {
            update.agent = Field::Clear;
            guarded(update, waiting_by(Some(agent)))
        }
        None => guarded(update, unless_a_subagent_waits()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::state::Record;

    const SESSION: &str = "403d5457-3ea6-4a34-8e30-a706b39e4067";
    const SUBAGENT: &str = "ae48a9ce1c4008eda";

    /// Feed payloads in order to an empty record, one second apart.
    fn replay(events: &[Value]) -> Record {
        let mut record = Record::default();
        for (time, event) in events.iter().enumerate() {
            let mut event = event.clone();
            event["session_id"] = json!(SESSION);
            if let Some(change) = Claude.interpret(&event).unwrap() {
                change.apply_to(&mut record, time as u64);
            }
        }
        record
    }

    fn event(name: &str) -> Value {
        json!({ "hook_event_name": name })
    }

    /// A mod event as the hooks module forwards it.
    fn mod_event(name: &str, fields: Value) -> Value {
        with(json!({ "event": name }), fields)
    }

    fn with(mut value: Value, extra: Value) -> Value {
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value
    }

    fn state(record: &Record) -> &str {
        record.state.as_ref().map_or("", State::as_str)
    }

    fn bash() -> Value {
        json!({ "tool_name": "Bash", "tool_input": { "command": "ls -la > x" } })
    }

    /// The subagent's version of a hook payload.
    fn sub(name: &str) -> Value {
        with(event(name), with(bash(), json!({ "agent_id": SUBAGENT })))
    }

    #[test]
    fn a_permission_prompt_waits_until_the_tool_finishes() {
        let prompt = [
            event("SessionStart"),
            with(
                event("UserPromptSubmit"),
                json!({ "prompt": "list\nfiles" }),
            ),
            with(event("PermissionRequest"), bash()),
            with(
                event("Notification"),
                json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission" }),
            ),
        ];
        let record = replay(&prompt);
        assert_eq!(state(&record), "waiting.permission");
        assert_eq!(record.message.as_deref(), Some("Bash: ls -la > x"));
        assert_eq!(record.since, Some(2));
        assert_eq!(record.harness.as_deref(), Some("claude"));
        assert_eq!(record.session.as_deref(), Some(SESSION));

        let mut approved = prompt.to_vec();
        approved.push(with(event("PostToolUse"), bash()));
        assert_eq!(state(&replay(&approved)), "working");
        approved.push(with(
            event("Stop"),
            json!({ "background_tasks": [], "last_assistant_message": "ok" }),
        ));
        let record = replay(&approved);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Finished));
        assert_eq!(record.background, 0);
        assert_eq!(record.message.as_deref(), Some("ok"));
        assert_eq!(record.since, Some(5));

        // The next prompt starts over.
        approved.push(with(event("UserPromptSubmit"), json!({ "prompt": "more" })));
        let record = replay(&approved);
        assert_eq!(state(&record), "working");
        assert_eq!(record.outcome, None);
    }

    #[test]
    fn questions_are_not_permission_prompts() {
        let ask = json!({
            "tool_name": "AskUserQuestion",
            "tool_input": { "questions": [{ "question": "Red or blue?" }] }
        });
        let record = replay(&[
            with(event("PreToolUse"), ask.clone()),
            with(event("PermissionRequest"), ask),
            with(
                event("Notification"),
                json!({ "notification_type": "permission_prompt" }),
            ),
        ]);
        assert_eq!(state(&record), "waiting.question");
        assert_eq!(record.message.as_deref(), Some("Red or blue?"));
        assert_eq!(record.since, Some(0));

        let plan = json!({ "tool_name": "ExitPlanMode", "tool_input": { "plan": "# Plan" } });
        assert_eq!(
            state(&replay(&[with(event("PermissionRequest"), plan)])),
            "waiting.question"
        );
        let other = with(event("PreToolUse"), bash());
        assert_eq!(Claude.interpret(&other).unwrap(), None);
    }

    #[test]
    fn the_notification_alone_raises_a_wait() {
        let record = replay(&[
            event("UserPromptSubmit"),
            with(
                event("Notification"),
                json!({ "notification_type": "permission_prompt", "message": "Network request" }),
            ),
        ]);
        assert_eq!(state(&record), "waiting.permission");
        assert_eq!(record.message.as_deref(), Some("Network request"));
    }

    #[test]
    fn background_tasks_are_counted_when_the_turn_ends() {
        let ended = [
            event("UserPromptSubmit"),
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a", "type": "subagent" }, { "id": "b" }] }),
            ),
        ];
        let record = replay(&ended);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Finished));
        assert_eq!(record.background, 2);
        // A new session has none.
        let mut restarted = ended.to_vec();
        restarted.push(with(event("SessionStart"), json!({ "source": "clear" })));
        let record = replay(&restarted);
        assert_eq!(
            (state(&record), record.outcome, record.background),
            ("ready", None, 0)
        );
    }

    #[test]
    fn a_subagent_wait_ends_with_its_own_events() {
        let raised = [
            event("UserPromptSubmit"),
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a" }] }),
            ),
            sub("PermissionRequest"),
        ];
        let record = replay(&raised);
        assert_eq!(state(&record), "waiting.permission");
        assert_eq!(record.agent.as_deref(), Some(SUBAGENT));

        // The main agent's progress and other subagents leave it alone.
        let mut others = raised.to_vec();
        others.push(with(event("PostToolUse"), bash()));
        others.push(with(event("Stop"), json!({ "background_tasks": [] })));
        others.push(with(
            event("PostToolUse"),
            with(bash(), json!({ "agent_id": "other" })),
        ));
        assert_eq!(state(&replay(&others)), "waiting.permission");

        let mut answered = raised.to_vec();
        answered.push(sub("PostToolUse"));
        let record = replay(&answered);
        assert_eq!(state(&record), "working");
        assert_eq!(record.agent, None);
        answered.push(sub("SubagentStop"));
        answered.push(event("UserPromptSubmit"));
        answered.push(with(event("Stop"), json!({ "background_tasks": [] })));
        let record = replay(&answered);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Finished));
    }

    #[test]
    fn a_task_notification_leaves_a_subagent_wait() {
        // Another background task finished while this subagent's dialog was
        // on screen; its result reached the main agent as a prompt.
        let notice = "<task-notification>\n<task-id>ac071dbfef3cabe24</task-id>\n\
            <status>completed</status>\n<summary>Agent \"Research timeouts\" \
            completed</summary>\n</task-notification>";
        let record = replay(&[
            event("UserPromptSubmit"),
            sub("PermissionRequest"),
            with(
                event("SubagentStop"),
                json!({ "agent_id": "ac071dbfef3cabe24" }),
            ),
            with(event("UserPromptSubmit"), json!({ "prompt": notice })),
            with(event("PostToolUse"), bash()),
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": SUBAGENT }] }),
            ),
        ]);
        assert_eq!(state(&record), "waiting.permission");
        assert_eq!(record.agent.as_deref(), Some(SUBAGENT));

        // With no dialog on screen it starts a turn, like any prompt.
        let record = replay(&[
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a" }] }),
            ),
            with(event("UserPromptSubmit"), json!({ "prompt": notice })),
        ]);
        assert_eq!(state(&record), "working");
        assert_eq!(
            record.message.as_deref(),
            Some("Agent \"Research timeouts\" completed")
        );
    }

    #[test]
    fn delivered_prompts_are_progress_not_typing() {
        let handback = "<agent-message from=\"a51dda7901fad5ecc\">\n[Subagent hand-back] pong";
        let waiting = [event("UserPromptSubmit"), sub("PermissionRequest")];
        // On either channel, a delivered prompt leaves the dialog alone.
        let mut classic = waiting.to_vec();
        classic.push(with(
            event("UserPromptSubmit"),
            json!({ "prompt": handback }),
        ));
        assert_eq!(state(&replay(&classic)), "waiting.permission");
        let mut forwarded = waiting.to_vec();
        forwarded.push(mod_event(
            "prompt.submit",
            json!({ "text": handback, "origin": "peer", "is_meta": true }),
        ));
        forwarded.push(mod_event(
            "prompt.submit",
            json!({ "text": "<task-notification>…", "origin": "task-notification" }),
        ));
        assert_eq!(state(&replay(&forwarded)), "waiting.permission");
        // A typed one ends it, whatever the text.
        let mut typed = waiting.to_vec();
        typed.push(mod_event(
            "prompt.submit",
            json!({ "text": "never mind", "origin": "composer" }),
        ));
        let record = replay(&typed);
        assert_eq!(state(&record), "working");
        assert_eq!(record.message.as_deref(), Some("never mind"));
        assert_eq!(record.agent, None);
        // The gist of a delivered message is its body.
        let record = replay(&[with(
            event("UserPromptSubmit"),
            json!({ "prompt": handback }),
        )]);
        assert_eq!(record.message.as_deref(), Some("[Subagent hand-back] pong"));
    }

    #[test]
    fn a_new_prompt_ends_a_cancelled_wait() {
        // Esc, Ctrl-C and a bare "No" fire no settings hook, so without the
        // mod the wait lasts until the next prompt.
        let record = replay(&[
            with(event("PermissionRequest"), bash()),
            with(event("UserPromptSubmit"), json!({ "prompt": "never mind" })),
        ]);
        assert_eq!(state(&record), "working");
        assert_eq!(record.message.as_deref(), Some("never mind"));
    }

    #[test]
    fn an_interrupt_ends_the_turn_interrupted() {
        // Esc while Claude works, or Ctrl-C before it replies: the engine's
        // note, then the aborted turn.
        let interrupted = [
            with(
                event("UserPromptSubmit"),
                json!({ "prompt": "write a story" }),
            ),
            mod_event("turn.start", json!({ "text": "write a story" })),
            mod_event(
                "session.append",
                json!({ "door": "note", "text": "[Request interrupted by user]" }),
            ),
            mod_event("turn.complete", json!({ "reason": "aborted" })),
        ];
        let record = replay(&interrupted);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Interrupted));
        assert_eq!(record.message.as_deref(), Some("write a story"));
        assert_eq!(record.since, Some(2));

        let mut next = interrupted.to_vec();
        next.push(with(
            event("UserPromptSubmit"),
            json!({ "prompt": "again" }),
        ));
        let record = replay(&next);
        assert_eq!((state(&record), record.outcome), ("working", None));
    }

    #[test]
    fn a_refused_prompt_is_an_interrupt() {
        // Esc at the dialog leaves the note, then ends the turn as answered,
        // with no Stop.
        let record = replay(&[
            with(event("PermissionRequest"), bash()),
            mod_event(
                "session.append",
                json!({ "door": "note", "text": "[Request interrupted by user for tool use]" }),
            ),
            mod_event("turn.complete", json!({ "reason": "answer" })),
        ]);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Interrupted));
        assert_eq!(record.agent, None);
        assert_eq!(record.message.as_deref(), Some("Bash: ls -la > x"));
        assert_eq!(record.since, Some(1));

        // A subagent's refused prompt ends its wait, and the main agent
        // goes on.
        let record = replay(&[
            sub("PermissionRequest"),
            mod_event(
                "session.append",
                json!({ "door": "note", "text": "[Request interrupted by user for tool use]", "agent_id": SUBAGENT }),
            ),
        ]);
        assert_eq!(state(&record), "working");
        assert_eq!(record.agent, None);
    }

    #[test]
    fn an_approved_tool_runs() {
        let record = replay(&[
            with(event("PermissionRequest"), bash()),
            mod_event("ui.render", json!({ "running": true, "tool": "Bash" })),
        ]);
        assert_eq!(state(&record), "working");
        assert_eq!(record.message.as_deref(), Some("Bash: ls -la > x"));
        // The main agent's rows don't answer a subagent's dialog.
        let record = replay(&[
            sub("PermissionRequest"),
            mod_event("ui.render", json!({ "running": true, "tool": "Bash" })),
        ]);
        assert_eq!(state(&record), "waiting.permission");
        let stopped = mod_event("ui.render", json!({ "running": false }));
        assert_eq!(Claude.interpret(&stopped).unwrap(), None);
    }

    #[test]
    fn a_shell_command_is_a_turn_of_its_own() {
        let started = [
            with(event("Stop"), json!({ "last_assistant_message": "ok" })),
            mod_event(
                "prompt.change",
                json!({ "origin": "clear", "before": "! sleep 6 && echo done" }),
            ),
        ];
        let record = replay(&started);
        assert_eq!(state(&record), "working");
        assert_eq!(record.message.as_deref(), Some("! sleep 6 && echo done"));

        let mut finished = started.to_vec();
        finished.push(mod_event(
            "session.append",
            json!({ "door": "command", "text": "<bash-stdout>done</bash-stdout><bash-stderr></bash-stderr>" }),
        ));
        let record = replay(&finished);
        assert_eq!(
            (state(&record), record.outcome),
            ("ready", Some(Outcome::Finished))
        );
        assert_eq!(record.message.as_deref(), Some("! sleep 6 && echo done"));
        // Claude then runs a turn on the output.
        finished.push(mod_event(
            "turn.start",
            json!({ "text": "<bash-stdout>done</bash-stdout>" }),
        ));
        assert_eq!(state(&replay(&finished)), "working");

        // Other cleared drafts say nothing.
        for fields in [
            json!({ "origin": "clear", "before": "hello" }),
            json!({ "origin": "history", "before": "! ls" }),
        ] {
            let change = Claude
                .interpret(&mod_event("prompt.change", fields))
                .unwrap();
            assert_eq!(change, None);
        }
    }

    #[test]
    fn a_turn_ends_through_the_mod_too() {
        let turn = |reason: &str| {
            replay(&[
                mod_event("turn.start", json!({ "text": "x" })),
                mod_event("turn.complete", json!({ "reason": reason })),
            ])
        };
        let answered = turn("answer");
        assert_eq!(
            (state(&answered), answered.outcome),
            ("ready", Some(Outcome::Finished))
        );
        assert_eq!(turn("error").outcome, Some(Outcome::Error));
        assert_eq!(turn("refusal").outcome, Some(Outcome::Finished));
        // A subagent's turn is its own business.
        let theirs = mod_event(
            "turn.complete",
            json!({ "reason": "aborted", "agent_id": SUBAGENT }),
        );
        assert_eq!(Claude.interpret(&theirs).unwrap(), None);
        // Both channels report one end, and the hook's detail stays.
        let record = replay(&[
            event("UserPromptSubmit"),
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a" }], "last_assistant_message": "ok" }),
            ),
            mod_event("turn.complete", json!({ "reason": "answer" })),
        ]);
        assert_eq!(record.since, Some(1));
        assert_eq!(record.background, 1);
        assert_eq!(record.message.as_deref(), Some("ok"));
    }

    #[test]
    fn idle_prompt_ends_only_a_turn_in_progress() {
        let idle = with(
            event("Notification"),
            json!({ "notification_type": "idle_prompt" }),
        );
        let waited = replay(&[with(event("PermissionRequest"), bash()), idle.clone()]);
        assert_eq!(state(&waited), "ready");
        assert_eq!(waited.outcome, None);
        assert_eq!(waited.message, None);
        let worked = replay(&[event("UserPromptSubmit"), idle.clone()]);
        assert_eq!((state(&worked), worked.outcome), ("ready", None));
        // A finished turn keeps its outcome, background tasks and all.
        let finished = replay(&[
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a" }], "last_assistant_message": "ok" }),
            ),
            idle,
        ]);
        assert_eq!(state(&finished), "ready");
        assert_eq!(finished.outcome, Some(Outcome::Finished));
        assert_eq!(finished.background, 1);
        assert_eq!(finished.message.as_deref(), Some("ok"));
        assert_eq!(finished.since, Some(0));
    }

    #[test]
    fn errors_carry_their_detail() {
        let record = replay(&[with(
            event("StopFailure"),
            json!({ "error": "rate_limit", "error_details": "429 Too Many Requests" }),
        )]);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Error));
        assert_eq!(
            record.message.as_deref(),
            Some("rate_limit: 429 Too Many Requests")
        );
    }

    #[test]
    fn elicitations_are_questions() {
        let record = replay(&[with(
            event("Elicitation"),
            json!({ "mcp_server_name": "notion", "message": "Pick a page" }),
        )]);
        assert_eq!(state(&record), "waiting.question");
        assert_eq!(record.message.as_deref(), Some("notion: Pick a page"));
        let record = replay(&[
            with(event("Elicitation"), json!({})),
            event("ElicitationResult"),
        ]);
        assert_eq!(state(&record), "working");
    }

    #[test]
    fn session_end_clears_only_its_own_session() {
        let mut record = replay(&[event("SessionStart")]);
        record.pinned = true;
        let stale = json!({ "hook_event_name": "SessionEnd", "session_id": "old" });
        Claude
            .interpret(&stale)
            .unwrap()
            .unwrap()
            .apply_to(&mut record, 5);
        assert_eq!(state(&record), "ready");
        let current = json!({ "hook_event_name": "SessionEnd", "session_id": SESSION });
        Claude
            .interpret(&current)
            .unwrap()
            .unwrap()
            .apply_to(&mut record, 6);
        assert_eq!(
            record,
            Record {
                pinned: true,
                ..Record::default()
            }
        );
    }

    #[test]
    fn compaction_keeps_the_state() {
        // What Claude Code 2.1.300 sends for one compaction.
        let compaction = |trigger: &str| {
            [
                with(event("PreCompact"), json!({ "trigger": trigger })),
                with(event("SubagentStop"), json!({ "agent_id": "compactor" })),
                with(event("SessionStart"), json!({ "source": "compact" })),
                with(event("PostCompact"), json!({ "trigger": trigger })),
            ]
        };
        let mut mid_turn = vec![event("UserPromptSubmit")];
        mid_turn.extend(compaction("auto"));
        assert_eq!(state(&replay(&mid_turn)), "working");
        // `/compact`, and compaction while the session idles.
        let mut between_turns = vec![event("UserPromptSubmit"), event("Stop")];
        between_turns.extend(compaction("manual"));
        let record = replay(&between_turns);
        assert_eq!(state(&record), "ready");
        assert_eq!(record.outcome, Some(Outcome::Finished));
        assert_eq!(record.since, Some(1));
    }

    #[test]
    fn malformed_payloads_are_errors() {
        assert!(Claude.interpret(&json!({})).is_err());
        assert!(
            Claude
                .interpret(&json!({ "hook_event_name": "SessionEnd" }))
                .is_err()
        );
        let agent = json!({ "hook_event_name": "PostToolUse", "agent_id": "a,b" });
        assert!(Claude.interpret(&agent).is_err());
        assert_eq!(Claude.interpret(&event("FileChanged")).unwrap(), None);
        let unknown = mod_event("tool.describe", json!({}));
        assert_eq!(Claude.interpret(&unknown).unwrap(), None);
    }
}
