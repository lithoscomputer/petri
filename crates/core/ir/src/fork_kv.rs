//! Fork-owned kv tombstones (fabro-70af PART 2b): the removal half of the
//! `x.context_consume_keys` contract.
//!
//! `RunContext::merge` was insert-only — a stage's `context_updates` could
//! overwrite a key but never drop one, so a consumed input (`review_verdict`
//! after the planner folded it into the seed brief) stayed in the durable
//! context and every later cycle's preamble re-rendered the stale value.
//! A step that consumes a context key now writes [`tombstone()`] under that
//! key alongside its updates; `RunContext::merge` recognizes the marker and
//! removes the key instead of storing it. The marker is a plain [`Value`],
//! so it survives the event log and the checkpoint serialization unchanged,
//! and a merge that carries a tombstone for a key nothing wrote is a no-op.
//!
//! This file is fork-owned (new file, `fork_` prefix): an upstream merge
//! cannot silently absorb it. The upstream touch points are the tombstone
//! arms in `RunContext::merge`/`kv_value` in `flow.rs` and the module
//! declaration — pinned by the tests here.

use crate::Value;

/// The marker text of a tombstone value: NUL-wrapped so no workflow string
/// value can collide with it.
const TOMBSTONE_TEXT: &str = "\u{0}__petri_kv_tombstone__\u{0}";

/// The value a step writes under a context key it consumes. Merging it
/// removes the key from the run context.
#[must_use]
pub fn tombstone() -> Value {
    Value::String(String::from(TOMBSTONE_TEXT))
}

/// Whether `value` is the consume marker [`tombstone()`] produces.
#[must_use]
pub fn is_tombstone(value: &Value) -> bool {
    matches!(value, Value::String(text) if text == TOMBSTONE_TEXT)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use smol_str::SmolStr;

    use super::*;
    use crate::RunContext;

    /// Presence pin (fork feature, fabro-70af PART 2b): a tombstone in
    /// `context_updates` removes the key from the run context instead of
    /// storing the marker.
    #[test]
    fn merge_removes_a_tombstoned_key() {
        let mut context = RunContext::default();
        let seed = BTreeMap::from([
            (SmolStr::new("review_verdict"), json!("approved")),
            (SmolStr::new("journal"), json!({"painpoints": []})),
        ]);
        context.merge(&seed);
        let update = BTreeMap::from([(SmolStr::new("review_verdict"), tombstone())]);
        context.merge(&update);
        assert_eq!(context.get("review_verdict"), None);
        assert!(context.get("journal").is_some());
    }

    /// A tombstone for a key nothing wrote is a no-op, and a plain value
    /// still overwrites (last write wins).
    #[test]
    fn a_tombstone_for_an_absent_key_is_a_noop_and_plain_writes_still_win() {
        let mut context = RunContext::default();
        let updates = BTreeMap::from([
            (SmolStr::new("ghost"), tombstone()),
            (SmolStr::new("k"), json!(2)),
        ]);
        context.merge(&updates);
        assert_eq!(context.get("ghost"), None);
        assert_eq!(context.get("k"), Some(&json!(2)));
    }

    /// `kv_value` never shows a marker even if one slipped past a merge.
    #[test]
    fn kv_value_hides_markers() {
        let mut context = RunContext::default();
        let updates = BTreeMap::from([
            (SmolStr::new("live"), json!(1)),
            (SmolStr::new("dead"), tombstone()),
        ]);
        context.merge(&updates);
        let value = context.kv_value();
        assert!(value.get("live").is_some());
        assert!(value.get("dead").is_none());
    }
}
