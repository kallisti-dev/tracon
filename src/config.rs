//! Every tunable, in one place for the Lua loader to fill later.

use std::time::Duration;

use crate::Error;
use crate::state::{Rule, parse_order};
use crate::tmux::Tmux;

pub const DEFAULT_ORDER: &str = "waiting pinned error done working idle unknown";
pub const ORDER_OPTION: &str = "@tracon-order";
pub const DEFAULT_RECENT_FIRST: &str = "working idle unknown";
pub const RECENT_FIRST_OPTION: &str = "@tracon-recent-first";
pub const DEFAULT_PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
pub const PREVIEW_INTERVAL_OPTION: &str = "@tracon-preview-interval";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Groups panes sort into; the first matching rule wins.
    pub order: Vec<Rule>,
    /// Groups that list the pane viewed most recently first, rather than
    /// the one longest in its state.
    pub recent_first: Vec<Rule>,
    /// How often the picker recaptures the previewed pane. Each capture runs
    /// tmux once.
    pub preview_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            order: parse_order(DEFAULT_ORDER).expect("the default order parses"),
            recent_first: parse_order(DEFAULT_RECENT_FIRST).expect("the default parses"),
            preview_interval: DEFAULT_PREVIEW_INTERVAL,
        }
    }
}

impl Config {
    /// The defaults, with each value replaced by its tmux option when that is
    /// set: `@tracon-order`, `@tracon-recent-first`, and
    /// `@tracon-preview-interval` in milliseconds.
    pub fn from_tmux(tmux: &Tmux) -> Result<Self, Error> {
        let mut config = Config::default();
        let order = tmux.global_option(ORDER_OPTION)?;
        if !order.trim().is_empty() {
            config.order = parse_order(&order)?;
        }
        let recent_first = tmux.global_option(RECENT_FIRST_OPTION)?;
        if !recent_first.trim().is_empty() {
            config.recent_first = parse_order(&recent_first)?;
        }
        let interval = tmux.global_option(PREVIEW_INTERVAL_OPTION)?;
        if !interval.trim().is_empty() {
            config.preview_interval = parse_milliseconds(PREVIEW_INTERVAL_OPTION, &interval)?;
        }
        Ok(config)
    }
}

/// A positive whole number of milliseconds.
fn parse_milliseconds(option: &'static str, text: &str) -> Result<Duration, Error> {
    match text.trim().parse::<u64>() {
        Ok(milliseconds) if milliseconds > 0 => Ok(Duration::from_millis(milliseconds)),
        _ => Err(Error::InvalidId {
            kind: option,
            value: text.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_are_positive_milliseconds() {
        let parse = |text| parse_milliseconds(PREVIEW_INTERVAL_OPTION, text);
        assert_eq!(parse(" 250 ").unwrap(), Duration::from_millis(250));
        for bad in ["0", "-5", "0.5", "fast"] {
            let error = parse(bad).unwrap_err().to_string();
            assert_eq!(error, format!("invalid @tracon-preview-interval {bad:?}"));
        }
    }
}
