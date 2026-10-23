//! What the native format may refer to.

/// The engine bindings an expression may name. Anything else is a diagnostic,
/// with this list in the hint, because a typo that evaluates to `null` is
/// exactly the silent failure the format exists to prevent.
pub(crate) const KNOWN_BINDINGS: &[&str] = &[
    "token",
    "input",
    "inputs",
    "nodes",
    "kv",
    "env",
    "status",
    "output",
    "outcome",
    "item",
    "index",
    "generation",
    "attempt",
    "node",
    "run",
    "params",
];

/// Keys the format understands at each level, for unknown-key rejection.
pub(crate) const TOP_KEYS: &[&str] = &["name", "params", "scopes", "entry", "nodes"];
pub(crate) const SCOPE_KEYS: &[&str] = &[
    "runtime",
    "source",
    "services",
    "requirements",
    "env",
    "workspace",
    "grace",
];
pub(crate) const NODE_KEYS: &[&str] = &[
    "scope", "step", "run", "shell", "config", "join", "if", "budget", "retry", "next", "select",
    "parallel", "for_each",
];
pub(crate) const ARM_KEYS: &[&str] = &["to", "when", "map", "back"];
pub(crate) const FOR_EACH_KEYS: &[&str] = &[
    "items",
    "parallel",
    "until",
    "max_parallel",
    "fail_fast",
    "max_iterations",
];
pub(crate) const BUDGET_KEYS: &[&str] = &["max_firings", "timeout"];
pub(crate) const RETRY_KEYS: &[&str] = &["max_attempts", "backoff", "retry_on", "on_exhaustion"];
pub(crate) const BACKOFF_KEYS: &[&str] = &["initial", "factor", "max", "jitter"];
pub(crate) const RETRY_ON_KEYS: &[&str] = &["statuses", "classes"];
