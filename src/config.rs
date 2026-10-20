//! Every tunable, in one place for the Lua loader to fill later.

use crate::Error;
use crate::state::{Rule, parse_order};
use crate::tmux::Tmux;

pub const DEFAULT_ORDER: &str = "waiting pinned error done working idle unknown";
pub const ORDER_OPTION: &str = "@tracon-order";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Groups panes sort into; the first matching rule wins.
    pub order: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            order: parse_order(DEFAULT_ORDER).expect("the default order parses"),
        }
    }
}

impl Config {
    /// The defaults, with the order from the tmux option `@tracon-order` when
    /// it is set.
    pub fn from_tmux(tmux: &Tmux) -> Result<Self, Error> {
        let mut config = Config::default();
        let order = tmux.global_option(ORDER_OPTION)?;
        if !order.trim().is_empty() {
            config.order = parse_order(&order)?;
        }
        Ok(config)
    }
}
