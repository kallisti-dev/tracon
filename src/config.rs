//! Every tunable, in one place for the Lua loader to fill later.

use std::time::Duration;

use crate::Error;
use crate::format::{Style, Template};
use crate::label::Table;
use crate::list;
use crate::status::{Rule, parse_order};
use crate::tmux::Tmux;

pub const DEFAULT_ORDER: &str = "waiting pinned error done working idle unknown";
pub const ORDER_OPTION: &str = "@tracon-order";
pub const DEFAULT_RECENT_FIRST: &str = "working idle unknown";
pub const RECENT_FIRST_OPTION: &str = "@tracon-recent-first";
pub const DEFAULT_PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
pub const PREVIEW_INTERVAL_OPTION: &str = "@tracon-preview-interval";
pub const LABELS_OPTION: &str = "@tracon-labels";
pub const ICONS_OPTION: &str = "@tracon-icons";
pub const STYLES_OPTION: &str = "@tracon-styles";
pub const FORMAT_OPTION: &str = "@tracon-format";
/// One row of the list, as the picker and `tracon list` show it.
pub const DEFAULT_FORMAT: &str = "{pin:<1}[{icon} {label:<16}]({style}) {age:>4}  \
    [{location:<18}](bold) [{harness:<7}](dim) {message|title}( · {background} in background)";

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
    pub labels: Table,
    pub icons: Table,
    pub styles: Table,
    /// The row format, over the variables in [`list::VARIABLES`].
    pub format: Template,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            order: parse_order(DEFAULT_ORDER).expect("the default order parses"),
            recent_first: parse_order(DEFAULT_RECENT_FIRST).expect("the default parses"),
            preview_interval: DEFAULT_PREVIEW_INTERVAL,
            labels: Table::labels(),
            icons: Table::icons(),
            styles: Table::styles(),
            format: Template::parse(DEFAULT_FORMAT).expect("the default format parses"),
        }
    }
}

impl Config {
    /// The defaults, with each value replaced by its tmux option when that is
    /// set: `@tracon-order`, `@tracon-recent-first`, `@tracon-preview-interval`
    /// in milliseconds, `@tracon-labels`, `@tracon-icons`, `@tracon-styles`
    /// and `@tracon-format`.
    pub fn from_tmux(tmux: &Tmux) -> Result<Self, Error> {
        let mut config = Config::default();
        if let Some(order) = set(tmux, ORDER_OPTION)? {
            config.order = parse_order(&order).map_err(|error| invalid(ORDER_OPTION, error))?;
        }
        if let Some(recent_first) = set(tmux, RECENT_FIRST_OPTION)? {
            config.recent_first =
                parse_order(&recent_first).map_err(|error| invalid(RECENT_FIRST_OPTION, error))?;
        }
        if let Some(interval) = set(tmux, PREVIEW_INTERVAL_OPTION)? {
            config.preview_interval = parse_milliseconds(PREVIEW_INTERVAL_OPTION, &interval)?;
        }
        if let Some(labels) = set(tmux, LABELS_OPTION)? {
            config.labels = config.labels.with(LABELS_OPTION, &labels)?;
        }
        if let Some(icons) = set(tmux, ICONS_OPTION)? {
            config.icons = config.icons.with(ICONS_OPTION, &icons)?;
        }
        if let Some(styles) = set(tmux, STYLES_OPTION)? {
            config.styles = config.styles.with(STYLES_OPTION, &styles)?;
            for style in config.styles.values() {
                Style::parse(style).map_err(|error| invalid(STYLES_OPTION, error))?;
            }
        }
        if let Some(format) = set(tmux, FORMAT_OPTION)? {
            config.format = parse_format(FORMAT_OPTION, &format)?;
        }
        Ok(config)
    }
}

/// A row format, checked against the variables rows have.
pub fn parse_format(option: &'static str, text: &str) -> Result<Template, Error> {
    let template = Template::parse(text).map_err(|error| invalid(option, error))?;
    if let Some(unknown) = template
        .variables()
        .into_iter()
        .find(|variable| !list::VARIABLES.contains(variable))
    {
        return Err(Error::InvalidOption {
            option,
            reason: format!("unknown variable {{{unknown}}}"),
        });
    }
    Ok(template)
}

/// The option's value when it is set to something.
fn set(tmux: &Tmux, option: &str) -> Result<Option<String>, Error> {
    let value = tmux.global_option(option)?;
    Ok((!value.trim().is_empty()).then_some(value))
}

fn invalid(option: &'static str, error: Error) -> Error {
    Error::InvalidOption {
        option,
        reason: error.to_string(),
    }
}

/// A positive whole number of milliseconds.
fn parse_milliseconds(option: &'static str, text: &str) -> Result<Duration, Error> {
    match text.trim().parse::<u64>() {
        Ok(milliseconds) if milliseconds > 0 => Ok(Duration::from_millis(milliseconds)),
        _ => Err(Error::Invalid {
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

    #[test]
    fn formats_are_checked_against_the_row_variables() {
        assert!(parse_format(FORMAT_OPTION, "{icon} {label}").is_ok());
        assert_eq!(
            parse_format(FORMAT_OPTION, "{icon} {colour}")
                .unwrap_err()
                .to_string(),
            "invalid @tracon-format: unknown variable {colour}"
        );
        assert_eq!(
            parse_format(FORMAT_OPTION, "[x").unwrap_err().to_string(),
            "invalid @tracon-format: an unclosed ["
        );
    }
}
