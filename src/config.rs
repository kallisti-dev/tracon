//! Every tunable, in one place: the defaults, what the configuration file
//! sets over them, settings and functions alike, and the tmux options over
//! both.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::Error;
use crate::format::{Style, Template};
use crate::label::Table;
use crate::list;
use crate::screen::Sighting;
use crate::state::Record;
use crate::status::{self, Rule, Status, parse_order};
use crate::summary;
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
pub const SUMMARY_FORMAT_OPTION: &str = "@tracon-summary-format";
/// One group of the summary, as `tracon summary` shows it.
pub const DEFAULT_SUMMARY_FORMAT: &str = "[{icon} {count}]({style})";
pub const SUMMARY_SEPARATOR_OPTION: &str = "@tracon-summary-separator";
pub const DEFAULT_SUMMARY_SEPARATOR: &str = " · ";

/// What a configuration file supplies in place of the built-in status
/// function and label tables: `f`, from a pane's record and what its screen
/// shows to its status, and `g`, from a status to its label, icon and style.
/// `None` from a `g` function leaves the lookup to the tables.
pub trait Functions: fmt::Debug + Send + Sync {
    fn status(&self, record: &Record, sighting: Option<Sighting>) -> Option<Status>;
    fn label(&self, status: &Status) -> Option<String>;
    fn icon(&self, status: &Status) -> Option<String>;
    fn style(&self, status: &Status) -> Option<String>;
}

/// How `tracon summary` shows the groups of the list.
#[derive(Clone, Debug)]
pub struct Summary {
    /// One group, over the variables in [`summary::VARIABLES`].
    pub format: Template,
    /// Between two groups.
    pub separator: String,
}

#[derive(Clone, Debug)]
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
    pub summary: Summary,
    /// The configuration file's functions, when it defines any.
    pub functions: Option<Arc<dyn Functions>>,
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
            summary: Summary {
                format: Template::parse(DEFAULT_SUMMARY_FORMAT)
                    .expect("the default summary format parses"),
                separator: DEFAULT_SUMMARY_SEPARATOR.to_owned(),
            },
            functions: None,
        }
    }
}

impl Config {
    /// The defaults, with each value replaced by its tmux option when that is
    /// set.
    pub fn from_tmux(tmux: &Tmux) -> Result<Self, Error> {
        let mut config = Config::default();
        config.apply_tmux(tmux)?;
        Ok(config)
    }

    /// Replace each value by its tmux option when that is set:
    /// `@tracon-order`, `@tracon-recent-first`, `@tracon-preview-interval`
    /// in milliseconds, `@tracon-labels`, `@tracon-icons`, `@tracon-styles`,
    /// `@tracon-format`, `@tracon-summary-format` and
    /// `@tracon-summary-separator`.
    pub fn apply_tmux(&mut self, tmux: &Tmux) -> Result<(), Error> {
        if let Some(order) = set(tmux, ORDER_OPTION)? {
            self.order = parse_order(&order).map_err(|error| invalid(ORDER_OPTION, error))?;
        }
        if let Some(recent_first) = set(tmux, RECENT_FIRST_OPTION)? {
            self.recent_first =
                parse_order(&recent_first).map_err(|error| invalid(RECENT_FIRST_OPTION, error))?;
        }
        if let Some(interval) = set(tmux, PREVIEW_INTERVAL_OPTION)? {
            self.preview_interval = parse_milliseconds(PREVIEW_INTERVAL_OPTION, &interval)?;
        }
        if let Some(labels) = set(tmux, LABELS_OPTION)? {
            self.labels = self.labels.clone().with(LABELS_OPTION, &labels)?;
        }
        if let Some(icons) = set(tmux, ICONS_OPTION)? {
            self.icons = self.icons.clone().with(ICONS_OPTION, &icons)?;
        }
        if let Some(styles) = set(tmux, STYLES_OPTION)? {
            self.styles = self.styles.clone().with(STYLES_OPTION, &styles)?;
            for style in self.styles.values() {
                Style::parse(style).map_err(|error| invalid(STYLES_OPTION, error))?;
            }
        }
        if let Some(format) = set(tmux, FORMAT_OPTION)? {
            self.format = parse_format(FORMAT_OPTION, &format, &list::VARIABLES)?;
        }
        if let Some(format) = set(tmux, SUMMARY_FORMAT_OPTION)? {
            self.summary.format =
                parse_format(SUMMARY_FORMAT_OPTION, &format, &summary::VARIABLES)?;
        }
        // Whitespace is a separator too, so only an empty option is unset.
        let separator = tmux.global_option(SUMMARY_SEPARATOR_OPTION)?;
        if !separator.is_empty() {
            self.summary.separator = separator;
        }
        Ok(())
    }

    /// The status of a pane: the configuration file's function, or the
    /// built-in [`status::status`].
    pub fn status(&self, record: &Record, sighting: Option<Sighting>) -> Option<Status> {
        match &self.functions {
            Some(functions) => functions.status(record, sighting),
            None => status::status(record, sighting),
        }
    }

    /// How a status is labelled: the configuration file's function, the
    /// labels table, or the status's own name.
    pub fn label(&self, status: &Status) -> String {
        self.functions
            .as_ref()
            .and_then(|functions| functions.label(status))
            .or_else(|| self.labels.get(status).map(str::to_owned))
            .unwrap_or_else(|| status.to_string())
    }

    /// A status's icon, or nothing.
    pub fn icon(&self, status: &Status) -> String {
        self.functions
            .as_ref()
            .and_then(|functions| functions.icon(status))
            .or_else(|| self.icons.get(status).map(str::to_owned))
            .unwrap_or_default()
    }

    /// A status's style, as [`Style::parse`] reads it, or nothing.
    pub fn style(&self, status: &Status) -> String {
        self.functions
            .as_ref()
            .and_then(|functions| functions.style(status))
            .or_else(|| self.styles.get(status).map(str::to_owned))
            .unwrap_or_default()
    }
}

/// A format, checked against the `variables` it may use.
pub fn parse_format(
    option: &'static str,
    text: &str,
    variables: &[&str],
) -> Result<Template, Error> {
    let template = Template::parse(text).map_err(|error| invalid(option, error))?;
    if let Some(unknown) = template
        .variables()
        .into_iter()
        .find(|variable| !variables.contains(variable))
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
    fn formats_are_checked_against_their_variables() {
        assert!(parse_format(FORMAT_OPTION, "{icon} {label}", &list::VARIABLES).is_ok());
        assert_eq!(
            parse_format(FORMAT_OPTION, "{icon} {colour}", &list::VARIABLES)
                .unwrap_err()
                .to_string(),
            "invalid @tracon-format: unknown variable {colour}"
        );
        assert_eq!(
            parse_format(FORMAT_OPTION, "[x", &list::VARIABLES)
                .unwrap_err()
                .to_string(),
            "invalid @tracon-format: an unclosed ["
        );
        assert_eq!(
            parse_format(SUMMARY_FORMAT_OPTION, "{count} {age}", &summary::VARIABLES)
                .unwrap_err()
                .to_string(),
            "invalid @tracon-summary-format: unknown variable {age}"
        );
    }

    /// Functions that answer for one status and leave the rest alone.
    #[derive(Debug)]
    struct Shell;

    impl Functions for Shell {
        fn status(&self, record: &Record, sighting: Option<Sighting>) -> Option<Status> {
            if record
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with('!'))
            {
                return Some("working.shell".parse().unwrap());
            }
            status::status(record, sighting)
        }

        fn label(&self, status: &Status) -> Option<String> {
            (status.as_str() == "working.shell").then(|| "in a shell".to_owned())
        }

        fn icon(&self, _: &Status) -> Option<String> {
            None
        }

        fn style(&self, status: &Status) -> Option<String> {
            (status.as_str() == "working.shell").then(|| "magenta".to_owned())
        }
    }

    #[test]
    fn functions_run_before_the_tables() {
        let config = Config {
            functions: Some(Arc::new(Shell)),
            ..Config::default()
        };
        let shell = Record {
            state: Some("working".parse().unwrap()),
            message: Some("! make".into()),
            ..Record::default()
        };
        let status = config.status(&shell, None).unwrap();
        assert_eq!(status.as_str(), "working.shell");
        assert_eq!(config.label(&status), "in a shell");
        // The icon falls through to the table, along the tree.
        assert_eq!(config.icon(&status), "◐");
        assert_eq!(config.style(&status), "magenta");
        let plain = Record {
            message: None,
            ..shell
        };
        let status = config.status(&plain, None).unwrap();
        assert_eq!(status.as_str(), "working");
        assert_eq!(config.label(&status), "working");
        assert_eq!(config.style(&status), "blue");
        // Without a table entry either, the status names itself.
        let review: Status = "review".parse().unwrap();
        assert_eq!(config.label(&review), "review");
        assert_eq!(config.icon(&review), "");
    }
}
