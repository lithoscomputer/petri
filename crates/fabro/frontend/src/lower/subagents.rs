//! Sub-agent settings for native agent nodes.
//!
//! The pinned Fabro gives every API-backend agent the sub-agent tools
//! (`spawn_agent`, `send_input`, `wait`, `close_agent`) and has no setting
//! that turns them on or off: `[run.agent]` has no such key and its parser
//! refuses one, and no node attribute exists either. It hands Pebble
//! `SubagentOptions::enabled()` with Pebble's defaults (no depth limit, four
//! open sessions per tree). Petri keeps the same shape: every `fabro/agent`
//! node carries this configuration with those defaults, the `subagents` key
//! stays refused, and what Petri decides is the bound Pebble's agent tree
//! runs under.
//!
//! An ACP agent owns its own tools, so the configuration reaches native
//! (`backend="api"`) sessions only, as in Fabro.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Pebble's default bound on the sessions one agent tree holds open at once,
/// the root session included: three children anywhere in the tree.
pub const DEFAULT_MAX_OPEN_SESSIONS: usize = 4;

/// The key the configuration is stored under in an agent node's step config.
pub const CONFIG_KEY: &str = "subagents";

/// What a native agent node tells Pebble about sub-agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentConfig {
    /// Advertise the sub-agent tools. The reference always does.
    pub enabled:           bool,
    /// The most sessions the node's agent tree may hold open at once,
    /// counting the node's own session. A value below two keeps the tools
    /// advertised and refuses every spawn.
    pub max_open_sessions: usize,
}

impl SubagentConfig {
    /// The pinned Fabro's behavior: sub-agents on, bounded by Pebble's
    /// default open-session limit.
    #[must_use]
    pub const fn reference() -> Self {
        Self {
            enabled:           true,
            max_open_sessions: DEFAULT_MAX_OPEN_SESSIONS,
        }
    }
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self::reference()
    }
}

/// Put the reference configuration on an agent node's step config.
pub(super) fn write(config: &mut Map<String, Value>) {
    let reference = SubagentConfig::reference();
    config.insert(
        CONFIG_KEY.into(),
        json!({
            "enabled": reference.enabled,
            "max_open_sessions": reference.max_open_sessions,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_configuration_round_trips() {
        let mut config = Map::new();
        write(&mut config);
        let read: SubagentConfig =
            serde_json::from_value(config[CONFIG_KEY].clone()).expect("deserializes");
        assert_eq!(read, SubagentConfig::reference());
        assert!(read.enabled);
        assert_eq!(read.max_open_sessions, DEFAULT_MAX_OPEN_SESSIONS);
    }

    #[test]
    fn an_absent_configuration_reads_as_the_reference() {
        let read: SubagentConfig = serde_json::from_value(json!({})).expect("defaults");
        assert_eq!(read, SubagentConfig::default());
    }
}
