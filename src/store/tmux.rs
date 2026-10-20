//! State in tmux pane options, which die with the pane and need no daemon.
//!
//! Each change is one tmux invocation: an `if-shell -F` whose condition is
//! the change's guard as a tmux format, followed by `wait-for -S tracon` so a
//! reader can block on the channel instead of polling.

use super::{Change, Cond, Field, Guarded, Store, valid_id};
use crate::Error;
use crate::backend::{Locator, TMUX};
use crate::state::{Record, State};
use crate::tmux::{self, CHANNEL, SEPARATOR, Tmux};

pub const STATE: &str = "@tracon_state";
pub const SINCE: &str = "@tracon_since";
pub const MESSAGE: &str = "@tracon_message";
pub const HARNESS: &str = "@tracon_harness";
pub const SESSION: &str = "@tracon_session";
pub const AGENT: &str = "@tracon_agent";
pub const PINNED: &str = "@tracon_pinned";

/// The options cleared with the state; the pin outlives it.
const STATE_OPTIONS: [&str; 6] = [STATE, SINCE, MESSAGE, HARNESS, SESSION, AGENT];

#[derive(Clone, Debug, Default)]
pub struct TmuxStore {
    tmux: Tmux,
}

impl TmuxStore {
    pub fn new(tmux: Tmux) -> Self {
        TmuxStore { tmux }
    }
}

fn pane_id(pane: &Locator) -> Result<&str, Error> {
    if pane.backend != TMUX {
        return Err(Error::UnsupportedBackend(pane.backend.clone()));
    }
    if !tmux::valid_pane_id(&pane.id) {
        return Err(Error::InvalidPane(pane.id.clone()));
    }
    Ok(&pane.id)
}

fn checked_id<'a>(kind: &'static str, id: &'a str) -> Result<&'a str, Error> {
    if valid_id(id) {
        Ok(id)
    } else {
        Err(Error::InvalidId {
            kind,
            value: id.to_owned(),
        })
    }
}

/// `cond` as a tmux format that expands to 1 when it holds for the target
/// pane. States and ids are validated, so they need no escaping.
fn format(cond: &Cond) -> Result<String, Error> {
    Ok(match cond {
        Cond::Always => "1".to_owned(),
        Cond::StateUnder(state) => under(state),
        Cond::Agent(agent) => {
            let agent = agent
                .as_deref()
                .map(|id| checked_id("agent id", id))
                .transpose()?;
            format!("#{{==:#{{{AGENT}}},{}}}", agent.unwrap_or(""))
        }
        Cond::Session(session) => {
            format!(
                "#{{==:#{{{SESSION}}},{}}}",
                checked_id("session id", session)?
            )
        }
        Cond::Not(cond) => format!("#{{?{},0,1}}", format(cond)?),
        Cond::All(conds) => {
            let mut formats = conds.iter().map(format);
            match formats.next().transpose()? {
                None => "1".to_owned(),
                Some(first) => formats.try_fold(first, |all, next| {
                    Ok::<_, Error>(format!("#{{&&:{all},{}}}", next?))
                })?,
            }
        }
    })
}

fn under(state: &State) -> String {
    format!("#{{||:#{{==:#{{{STATE}}},{state}}},#{{m:{state}.*,#{{{STATE}}}}}}}")
}

/// Option values never contain the field separator, so loads stay parseable.
fn clean(value: &str) -> String {
    value.replace(SEPARATOR, " ")
}

/// The tmux command list that applies `change` to `pane`.
fn commands(pane: &str, change: &Change, now: u64) -> String {
    let mut commands = Vec::new();
    match change {
        Change::Clear => {
            for option in STATE_OPTIONS {
                commands.push(format!("set-option -pu -t {pane} {option}"));
            }
        }
        Change::Set(update) => {
            let state = &update.state;
            // Set before the state changes: -F expands the format against the
            // previous state and keeps the old time if it is unchanged.
            commands.push(format!(
                "set-option -p -t {pane} -F {SINCE} {}",
                tmux::quote(&format!(
                    "#{{?#{{==:#{{{STATE}}},{state}}},#{{{SINCE}}},{now}}}"
                ))
            ));
            commands.push(format!("set-option -p -t {pane} {STATE} {state}"));
            for (option, field) in [
                (MESSAGE, &update.message),
                (HARNESS, &update.harness),
                (SESSION, &update.session),
                (AGENT, &update.agent),
            ] {
                match field {
                    Field::Keep => {}
                    Field::Clear => commands.push(format!("set-option -pu -t {pane} {option}")),
                    Field::Set(value) => commands.push(format!(
                        "set-option -p -t {pane} {option} {}",
                        tmux::quote(&clean(value))
                    )),
                }
            }
        }
    }
    commands.join(" ; ")
}

impl Store for TmuxStore {
    fn load(&self) -> Result<Vec<(Locator, Record)>, Error> {
        let fields = [
            "pane_id", STATE, SINCE, MESSAGE, HARNESS, SESSION, AGENT, PINNED,
        ];
        let format = fields
            .map(|field| format!("#{{{field}}}"))
            .join(&SEPARATOR.to_string());
        let output = self.tmux.run(["list-panes", "-a", "-F", &format])?;
        let mut records = Vec::new();
        for line in output.lines() {
            let values: Vec<&str> = line.split(SEPARATOR).collect();
            let [pane, state, since, message, harness, session, agent, pinned] = values[..] else {
                continue;
            };
            let text = |value: &str| (!value.is_empty()).then(|| value.to_owned());
            let record = Record {
                // A malformed value written by something else reads as no state.
                state: state.parse().ok(),
                since: since.parse().ok(),
                message: text(message),
                harness: text(harness),
                session: text(session),
                agent: text(agent),
                pinned: pinned == "1",
            };
            if record != Record::default() {
                records.push((Locator::tmux(pane), record));
            }
        }
        Ok(records)
    }

    fn apply(&self, pane: &Locator, change: &Guarded, now: u64) -> Result<(), Error> {
        let pane = pane_id(pane)?;
        let condition = format(&change.when)?;
        let commands = commands(pane, &change.change, now);
        self.tmux.run([
            "if-shell", "-F", "-t", pane, &condition, &commands, ";", "wait-for", "-S", CHANNEL,
        ])?;
        Ok(())
    }

    fn set_pinned(&self, pane: &Locator, pinned: bool) -> Result<(), Error> {
        let pane = pane_id(pane)?;
        let mut args = vec!["set-option", "-p", "-t", pane, PINNED, "1"];
        if !pinned {
            args = vec!["set-option", "-pu", "-t", pane, PINNED];
        }
        args.extend([";", "wait-for", "-S", CHANNEL]);
        self.tmux.run(args)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Update;

    #[test]
    fn conditions_become_formats() {
        let waiting: State = "waiting".parse().unwrap();
        assert_eq!(
            format(&Cond::StateUnder(waiting.clone())).unwrap(),
            "#{||:#{==:#{@tracon_state},waiting},#{m:waiting.*,#{@tracon_state}}}"
        );
        assert_eq!(
            format(&Cond::All(vec![
                Cond::negated(Cond::StateUnder(waiting)),
                Cond::Agent(None),
                Cond::Session("s-1".into()),
            ]))
            .unwrap(),
            "#{&&:#{&&:#{?#{||:#{==:#{@tracon_state},waiting},#{m:waiting.*,#{@tracon_state}}},0,1},\
             #{==:#{@tracon_agent},}},#{==:#{@tracon_session},s-1}}"
        );
        assert!(format(&Cond::Session("a,b".into())).is_err());
    }

    #[test]
    fn values_are_quoted_and_cleaned() {
        let mut update = Update::new("done".parse().unwrap());
        update.message = Field::Set("it's #{done};\u{1f}".into());
        update.agent = Field::Clear;
        assert_eq!(
            commands("%1", &Change::Set(update), 7),
            "set-option -p -t %1 -F @tracon_since \
             '#{?#{==:#{@tracon_state},done},#{@tracon_since},7}' ; \
             set-option -p -t %1 @tracon_state done ; \
             set-option -p -t %1 @tracon_message 'it'\\''s #{done}; ' ; \
             set-option -pu -t %1 @tracon_agent"
        );
    }
}
