//! Agent tool permissions, using Pebble's three permission levels.

use serde::{Deserialize, Serialize};

/// Agent tool access. Omission preserves Petri's historical full access.
/// These levels govern agents, not workflow scripts or runtime persistence.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPermissions {
    /// Native reads and subagents; no arbitrary shell commands.
    ReadOnly,
    /// Native reads, file edits, and subagents; no arbitrary shell commands.
    ReadWrite,
    /// All agent tools, including shell, MCP, and host tools.
    #[default]
    Full,
}
