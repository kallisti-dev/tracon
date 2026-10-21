//! Claude Code, through the hooks in the Claude plugin's `hooks/hooks.json`.
//!
//! Subagent events carry `agent_id`. A subagent's permission prompt still
//! shows in this pane, so it raises a wait like the main agent's, and the
//! store records which agent raised it: that agent's later events end the
//! wait, and the main agent's tool and turn events leave it alone. See
//! `docs/compatibility.md` for what Claude Code reports and what it doesn't.

use serde_json::Value;

use super::{Harness, command_is};
use crate::store::{Change, Cond, Field, Guarded, Update, valid_id};
use crate::{Error, State, one_line};

pub struct Claude;

/// Tools that ask the user something rather than asking to act.
const QUESTION_TOOLS: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];

const MESSAGE_LENGTH: usize = 160;

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

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn runs_as(&self, command: &str) -> bool {
        command_is(command, "claude")
    }

    fn interpret(&self, payload: &Value) -> Result<Option<Guarded>, Error> {
        let event = text(payload, "hook_event_name")
            .ok_or_else(|| Error::Payload("no hook_event_name".into()))?;
        let agent = text(payload, "agent_id").filter(|id| !id.is_empty());
        if let Some(agent) = agent
            && !valid_id(agent)
        {
            return Err(Error::Payload(format!("invalid agent_id {agent:?}")));
        }
        let session = text(payload, "session_id").filter(|id| valid_id(id));
        let set = |state: &'static str| {
            let mut update = Update::new(State::known(state));
            update.harness = Field::Set(self.name().to_owned());
            if let Some(session) = session {
                update.session = Field::Set(session.to_owned());
            }
            update
        };
        let guarded = |update: Update, when: Cond| Some(Guarded::when(Change::Set(update), when));

        Ok(match event {
            "SessionStart" => {
                // Compaction restarts the session mid-turn.
                if text(payload, "source") == Some("compact") {
                    return Ok(None);
                }
                let mut update = set(State::IDLE);
                update.message = Field::Clear;
                update.agent = Field::Clear;
                guarded(update, Cond::Always)
            }
            "SessionEnd" => {
                let session =
                    session.ok_or_else(|| Error::Payload("no valid session_id".into()))?;
                Some(Guarded::when(
                    Change::Clear,
                    Cond::Session(session.to_owned()),
                ))
            }
            // A subagent's turn has no prompt of its own; the main agent's
            // UserPromptSubmit covers notifications from background tasks.
            "UserPromptSubmit" if agent.is_none() => {
                let mut update = set(State::WORKING);
                update.message = message(text(payload, "prompt").unwrap_or(""));
                update.agent = Field::Clear;
                guarded(update, Cond::Always)
            }
            // PermissionRequest also fires for these tools; PreToolUse is
            // kept for modes that skip the permission check.
            "PreToolUse" | "PermissionRequest" => {
                let tool = text(payload, "tool_name").unwrap_or("");
                let question = QUESTION_TOOLS.contains(&tool);
                if event == "PreToolUse" && !question {
                    return Ok(None);
                }
                let mut update = set(if question {
                    State::WAITING_QUESTION
                } else {
                    State::WAITING_PERMISSION
                });
                update.message = message(&tool_summary(payload));
                update.agent = agent.map_or(Field::Clear, |id| Field::Set(id.to_owned()));
                guarded(update, Cond::Always)
            }
            "Elicitation" => {
                let mut update = set(State::WAITING_QUESTION);
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
                    let mut update = set(if kind == "permission_prompt" {
                        State::WAITING_PERMISSION
                    } else {
                        State::WAITING_QUESTION
                    });
                    update.message = message(text(payload, "message").unwrap_or(""));
                    update.agent = Field::Clear;
                    guarded(update, unless_waiting())
                }
                // The input prompt has sat untouched, so any wait or work it
                // shows is over. A finished turn stays `done` until seen, and
                // background tasks run on while the prompt is idle.
                Some("idle_prompt") => {
                    let mut update = set(State::IDLE);
                    update.message = Field::Clear;
                    update.agent = Field::Clear;
                    guarded(
                        update,
                        Cond::All(vec![
                            Cond::negated(Cond::StateUnder(State::known(State::DONE))),
                            Cond::negated(Cond::StateUnder(State::known(
                                State::WORKING_BACKGROUND,
                            ))),
                        ]),
                    )
                }
                _ => None,
            },
            "PostToolUse" | "PostToolUseFailure" | "PostToolBatch" | "PreCompact"
            | "ElicitationResult" => {
                let mut update = set(State::WORKING);
                if matches!(event, "PostToolUse" | "PostToolUseFailure") {
                    update.message = message(&tool_summary(payload));
                }
                match agent {
                    // The subagent's own prompt was answered.
                    Some(agent) => {
                        update.agent = Field::Clear;
                        guarded(update, waiting_by(Some(agent)))
                    }
                    None => guarded(update, unless_a_subagent_waits()),
                }
            }
            "SubagentStop" => agent.and_then(|agent| {
                let mut update = set(State::WORKING);
                update.agent = Field::Clear;
                guarded(update, waiting_by(Some(agent)))
            }),
            "Stop" if agent.is_none() => {
                let background = payload
                    .get("background_tasks")
                    .and_then(Value::as_array)
                    .is_some_and(|tasks| !tasks.is_empty());
                let mut update = set(if background {
                    State::WORKING_BACKGROUND
                } else {
                    State::DONE
                });
                update.message = message(text(payload, "last_assistant_message").unwrap_or(""));
                guarded(update, unless_a_subagent_waits())
            }
            "StopFailure" if agent.is_none() => {
                let mut update = set(State::ERROR);
                let error = text(payload, "error").unwrap_or("unknown");
                let detail = text(payload, "error_details")
                    .or_else(|| text(payload, "last_assistant_message"));
                update.message = message(&match detail {
                    Some(detail) => format!("{error}: {detail}"),
                    None => error.to_owned(),
                });
                guarded(update, unless_a_subagent_waits())
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::state::Record;

    const SESSION: &str = "403d5457-3ea6-4a34-8e30-a706b39e4067";

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
        assert_eq!(state(&record), "done");
        assert_eq!(record.message.as_deref(), Some("ok"));
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
    fn background_tasks_keep_the_turn_working() {
        let record = replay(&[
            event("UserPromptSubmit"),
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a", "type": "subagent" }] }),
            ),
        ]);
        assert_eq!(state(&record), "working.background");
    }

    #[test]
    fn a_subagent_wait_ends_with_its_own_events() {
        let sub = |name: &str| {
            with(
                event(name),
                with(bash(), json!({ "agent_id": "ae48a9ce1c4008eda" })),
            )
        };
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
        assert_eq!(record.agent.as_deref(), Some("ae48a9ce1c4008eda"));

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
        assert_eq!(state(&replay(&answered)), "done");
    }

    #[test]
    fn a_new_prompt_ends_a_cancelled_wait() {
        // Esc, Ctrl-C and a bare "No" fire nothing, so the wait lasts until
        // the next prompt.
        let record = replay(&[
            with(event("PermissionRequest"), bash()),
            with(event("UserPromptSubmit"), json!({ "prompt": "never mind" })),
        ]);
        assert_eq!(state(&record), "working");
        assert_eq!(record.message.as_deref(), Some("never mind"));
    }

    #[test]
    fn idle_prompt_ends_waits_but_not_done_or_background_work() {
        let idle = with(
            event("Notification"),
            json!({ "notification_type": "idle_prompt" }),
        );
        let waited = replay(&[with(event("PermissionRequest"), bash()), idle.clone()]);
        assert_eq!(state(&waited), "idle");
        assert_eq!(waited.message, None);
        let finished = replay(&[with(event("Stop"), json!({})), idle.clone()]);
        assert_eq!(state(&finished), "done");
        let background = replay(&[
            with(
                event("Stop"),
                json!({ "background_tasks": [{ "id": "a", "type": "subagent" }] }),
            ),
            idle,
        ]);
        assert_eq!(state(&background), "working.background");
    }

    #[test]
    fn errors_carry_their_detail() {
        let record = replay(&[with(
            event("StopFailure"),
            json!({ "error": "rate_limit", "error_details": "429 Too Many Requests" }),
        )]);
        assert_eq!(state(&record), "error");
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
        assert_eq!(state(&record), "idle");
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
    fn compaction_keeps_the_turn() {
        let record = replay(&[
            event("UserPromptSubmit"),
            event("PreCompact"),
            with(event("SessionStart"), json!({ "source": "compact" })),
        ]);
        assert_eq!(state(&record), "working");
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
    }
}
