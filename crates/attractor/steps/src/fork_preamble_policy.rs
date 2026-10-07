//! Fork-owned preamble/context policy (fabro-70af PART 2b): the node-aware
//! seam the preamble family consults at render time.
//!
//! The legacy per-node `x.*` family — `x.preamble_stages_ignore`,
//! `x.preamble_stages_latest_only`, `x.context_allow_keys`,
//! `x.preamble_allow_keys`, `x.context_consume_keys`,
//! `x.preamble_budget_kb`, `x.preamble_output_max_lines` — shaped what a
//! node's fidelity preamble showed: which completed stages it heard about,
//! which `## Context` keys reached it, how big the whole preamble and each
//! output block could grow, and which inputs it consumed. The petri rework
//! rendered preambles from the run context alone (`fidelity.rs`), with no
//! place for those decisions to act.
//!
//! This module is that place. A host installs a
//! [`PreamblePolicySource`] as the [`PreamblePolicyHandle`] capability —
//! the same host-replacement seam as `HookServiceHandle` — keyed by node
//! name; the agent and prompt steps ask it for the node's
//! [`PreamblePolicy`] and thread it into [`crate::fidelity::Preamble`].
//! A host without the capability gets the default (no-op) policy, so
//! upstream behavior is unchanged.
//!
//! Consume-keys are the one member that acts outside rendering: after a
//! successful stage, the step writes `fork_kv::tombstone()` under each
//! consumed key, and `RunContext::merge` removes the key — the durable
//! context drops consumed inputs instead of re-rendering them stale.
//!
//! This file is fork-owned (new file, `fork_` prefix): an upstream merge
//! cannot silently absorb it. The upstream touch points are the
//! `Preamble.policy` field, the filter calls in `fidelity.rs`, the
//! capability queries in `agent.rs`/`prompt.rs`, and the module
//! declaration — all pinned by the tests here.

use std::collections::BTreeMap;
use std::sync::Arc;

use ir::{Value, fork_kv};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use steps::StepCtx;

/// The per-node preamble/context policy: the `x.*` family as one value.
///
/// The default is a no-op: every stage visible, every context key allowed,
/// no budget, no cap, nothing consumed — exactly the rendering without a
/// policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreamblePolicy {
    /// The node the policy is for (informational; enforcement is keyed by
    /// the caller's own node identity).
    pub node:                String,
    /// `x.preamble_stages_ignore`: completed stages this node's preamble
    /// never shows, by base name (`planner` hides `planner` and
    /// `planner#3`).
    pub stages_ignore:       Vec<String>,
    /// `x.preamble_stages_latest_only`: repeated firings of a stage
    /// (`tester`, `tester#2`, …) collapse to the latest firing, render-only.
    pub stages_latest_only:  bool,
    /// `x.context_allow_keys`: when set, the only `## Context` keys the
    /// node's preamble may show.
    pub context_allow_keys:  Option<Vec<String>>,
    /// `x.preamble_allow_keys`: when set, an additional `## Context` key
    /// filter; with both set, a key shows only when both lists name it.
    pub preamble_allow_keys: Option<Vec<String>>,
    /// `x.context_consume_keys`: keys removed from the run context after
    /// this node records (tombstoned at merge).
    pub consume_keys:        Vec<String>,
    /// `x.preamble_budget_kb`: the rendered preamble's byte ceiling; a
    /// longer preamble is truncated with a marker.
    pub budget_kb:           Option<u64>,
    /// `x.preamble_output_max_lines`: the per-stage output block's line
    /// ceiling in the preamble, in place of the fidelity defaults.
    pub output_max_lines:    Option<usize>,
}

impl PreamblePolicy {
    /// Whether the policy changes nothing (the default).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }

    /// The base name of a stage id: `tester#3` is `tester`.
    fn base_name(id: &str) -> &str {
        id.split('#').next().unwrap_or(id)
    }

    /// The firing number of a stage id: `tester#3` is `3`, `tester` is `0`.
    fn firing(id: &str) -> u64 {
        id.rsplit_once('#')
            .and_then(|(_, n)| n.parse().ok())
            .unwrap_or(0)
    }

    /// Whether the completed stage `id` is visible: not in
    /// `stages_ignore`, by base name.
    #[must_use]
    pub fn stage_visible(&self, id: &str) -> bool {
        let base = Self::base_name(id);
        !self.stages_ignore.iter().any(|ignored| ignored == base)
    }

    /// The ids to drop under `stages_latest_only`: for every base name with
    /// more than one firing, every firing except the highest.
    #[must_use]
    pub fn latest_only_drops<'a>(&self, ids: &[&'a str]) -> BTreeMap<&'a str, u64> {
        if !self.stages_latest_only {
            return BTreeMap::new();
        }
        let mut highest: BTreeMap<&str, (&'a str, u64)> = BTreeMap::new();
        for id in ids {
            let base = Self::base_name(id);
            let firing = Self::firing(id);
            match highest.get(base) {
                Some((_, top)) if *top >= firing => {}
                _ => {
                    highest.insert(base, (*id, firing));
                }
            }
        }
        let mut drops = BTreeMap::new();
        for id in ids {
            if let Some((keep, _)) = highest.get(Self::base_name(id))
                && keep != id
            {
                drops.insert(*id, Self::firing(id));
            }
        }
        drops
    }

    /// Whether the `## Context` key `key` reaches this node: each set allow
    /// list filters on its own, and both set means a key shows only when
    /// both name it.
    #[must_use]
    pub fn context_visible(&self, key: &str) -> bool {
        let named = |list: &Option<Vec<String>>| {
            list.as_ref()
                .is_some_and(|keys| keys.iter().any(|allowed| allowed == key))
        };
        match (&self.context_allow_keys, &self.preamble_allow_keys) {
            (Some(_), Some(_)) => {
                named(&self.context_allow_keys) && named(&self.preamble_allow_keys)
            }
            (Some(_), None) => named(&self.context_allow_keys),
            (None, Some(_)) => named(&self.preamble_allow_keys),
            (None, None) => true,
        }
    }

    /// The per-stage output block's line ceiling, the fidelity default when
    /// the node sets none.
    #[must_use]
    pub fn output_line_cap(&self, default: usize) -> usize {
        self.output_max_lines.unwrap_or(default)
    }

    /// Enforce `budget_kb` on a rendered preamble: a preamble within the
    /// ceiling passes unchanged; a longer one is cut at the last char
    /// boundary that fits, with a marker naming the policy.
    #[must_use]
    pub fn enforce_budget(&self, rendered: String) -> String {
        let Some(kb) = self.budget_kb else {
            return rendered;
        };
        let ceiling = usize::try_from(kb.saturating_mul(1024)).unwrap_or(usize::MAX);
        if rendered.len() <= ceiling {
            return rendered;
        }
        let mut cut = ceiling;
        while cut > 0 && !rendered.is_char_boundary(cut) {
            cut -= 1;
        }
        let marker = format!(
            "\n\n(preamble truncated to the x.preamble_budget_kb={kb} KB ceiling; the goal, the \
             run id and the recent stages lead)"
        );
        format!("{}{}", &rendered[..cut], marker)
    }
}

/// The host's source of a node's [`PreamblePolicy`]: the fabro host reads
/// the run's stage envelopes; a host without preamble attributes installs
/// none and the steps use the default policy.
pub trait PreamblePolicySource: Send + Sync {
    /// The policy for `node`.
    fn policy(&self, node: &str) -> PreamblePolicy;
}

/// The capability type the agent and prompt steps look the source up by,
/// the same shape as the hook service handle.
pub struct PreamblePolicyHandle(pub Arc<dyn PreamblePolicySource>);

/// The policy a step applies: the installed source's answer for `node`,
/// or the no-op default when the host installed none.
#[must_use]
pub fn policy_for(ctx: &StepCtx, node: &str) -> PreamblePolicy {
    ctx.capability::<PreamblePolicyHandle>()
        .map(|handle| handle.0.policy(node))
        .unwrap_or_default()
}

/// The consume-keys half of the contract: tombstones for every consumed
/// key, inserted into the stage's `context_updates` after a successful
/// record. A key the stage itself wrote this firing is consumed too — the
/// consumer declared it consumed.
pub fn consume_tombstones(policy: &PreamblePolicy, updates: &mut BTreeMap<SmolStr, Value>) {
    for key in &policy.consume_keys {
        updates.insert(SmolStr::new(key), fork_kv::tombstone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(overrides: impl FnOnce(&mut PreamblePolicy)) -> PreamblePolicy {
        let mut value = PreamblePolicy {
            node: "planner".into(),
            ..PreamblePolicy::default()
        };
        overrides(&mut value);
        value
    }

    /// Presence pin (fork feature, fabro-70af PART 2b): the default policy
    /// changes nothing.
    #[test]
    fn the_default_policy_is_a_noop_that_shows_everything() {
        let default = PreamblePolicy::default();
        assert!(default.is_noop());
        assert!(default.stage_visible("tester#9"));
        assert!(default.context_visible("any.key"));
        assert!(default.latest_only_drops(&["a", "a#2"]).is_empty());
        let text = "x".repeat(10);
        assert_eq!(default.enforce_budget(text.clone()), text);
        assert_eq!(default.output_line_cap(25), 25);
    }

    /// stages_ignore hides a stage and every expansion clone of it, by
    /// base name; other stages stay visible.
    #[test]
    fn stages_ignore_hides_by_base_name_including_clones() {
        let p = policy(|p| p.stages_ignore = vec!["planner".into(), "evidence".into()]);
        assert!(!p.stage_visible("planner"));
        assert!(!p.stage_visible("planner#3"));
        assert!(!p.stage_visible("evidence"));
        assert!(p.stage_visible("tester#2"));
    }

    /// latest_only keeps only the highest firing per base name — numeric,
    /// not lexicographic (`a#10` beats `a#9`).
    #[test]
    fn latest_only_keeps_the_highest_firing_numerically() {
        let p = policy(|p| p.stages_latest_only = true);
        let ids = vec!["tester", "tester#9", "tester#10", "other#2", "other#3"];
        let drops = p.latest_only_drops(&ids);
        assert_eq!(drops.keys().copied().collect::<Vec<_>>(), [
            "other#2", "tester", "tester#9"
        ]);
    }

    /// Each allow list filters on its own; both set intersect.
    #[test]
    fn allow_keys_filter_and_intersect() {
        let context_only = policy(|p| p.context_allow_keys = Some(vec!["a".into(), "b".into()]));
        assert!(context_only.context_visible("a"));
        assert!(!context_only.context_visible("c"));
        let both = policy(|p| {
            p.context_allow_keys = Some(vec!["a".into(), "b".into()]);
            p.preamble_allow_keys = Some(vec!["b".into(), "c".into()]);
        });
        assert!(!both.context_visible("a"));
        assert!(both.context_visible("b"));
        assert!(!both.context_visible("c"));
    }

    /// The budget cuts at a char boundary and names the ceiling; a preamble
    /// within it passes unchanged.
    #[test]
    fn the_budget_truncates_on_a_char_boundary_with_a_marker() {
        let p = policy(|p| p.budget_kb = Some(1));
        let text = "ä".repeat(600); // 1200 bytes, no boundary at 1024
        let cut = p.enforce_budget(text);
        assert!(cut.len() < 1200);
        assert!(cut.contains("x.preamble_budget_kb=1 KB"));
        let small = "goal: tiny".to_owned();
        assert_eq!(p.enforce_budget(small.clone()), small);
    }

    /// The output cap replaces the fidelity default only when set.
    #[test]
    fn the_output_cap_overrides_the_default_when_set() {
        let p = policy(|p| p.output_max_lines = Some(3));
        assert_eq!(p.output_line_cap(25), 3);
        assert_eq!(PreamblePolicy::default().output_line_cap(25), 25);
    }

    /// Consume tombstones mark exactly the consumed keys.
    #[test]
    fn consume_tombstones_mark_the_consumed_keys() {
        let p =
            policy(|p| p.consume_keys = vec!["review_verdict".into(), "review_feedback".into()]);
        let mut updates = BTreeMap::new();
        updates.insert(SmolStr::new("journal"), serde_json::json!({}));
        consume_tombstones(&p, &mut updates);
        assert_eq!(updates.len(), 3);
        assert!(fork_kv::is_tombstone(&updates["review_verdict"]));
        assert!(fork_kv::is_tombstone(&updates["review_feedback"]));
        assert!(!fork_kv::is_tombstone(&updates["journal"]));
    }

    /// Presence pin (fork feature, fabro-70af PART 2b): `Preamble::render`
    /// consults the policy — ignored stages vanish, allow-listed context
    /// filters `## Context`, `latest_only` collapses firings, the budget
    /// truncates — and a `None` policy renders exactly as before.
    #[test]
    fn the_preamble_render_honors_the_policy() {
        use crate::fidelity::{Fidelity, Preamble, StageInfo};

        let stages = vec![
            StageInfo {
                id:     "tester".into(),
                kind:   Some("command".into()),
                script: Some("just gate".into()),
                model:  None,
            },
            StageInfo {
                id:     "planner".into(),
                kind:   Some("agent".into()),
                script: None,
                model:  None,
            },
        ];
        let nodes = serde_json::json!({
            "tester": {"status": "success", "output": {"stdout": "gate green\n", "outcome": "succeeded"}},
            "tester#2": {"status": "failure", "output": {"stdout": "gate red\n", "outcome": "failed", "failure_reason": "clippy"}},
            "planner": {"status": "success", "output": {"text": "Plan.", "outcome": "succeeded", "model": "m"}}
        });
        let kv = serde_json::json!({
            "seed_brief": "do it",
            "stray_key": "noise",
            "tests_passed": true
        });
        let p = PreamblePolicy {
            node: "implementer".into(),
            stages_ignore: vec!["planner".into()],
            stages_latest_only: true,
            context_allow_keys: Some(vec!["seed_brief".into(), "tests_passed".into()]),
            preamble_allow_keys: Some(vec!["seed_brief".into()]),
            budget_kb: None,
            ..PreamblePolicy::default()
        };
        let preamble = Preamble {
            goal:   "G",
            run_id: "r",
            stages: &stages,
            nodes:  &nodes,
            kv:     &kv,
            policy: Some(&p),
        };
        let compact = preamble.render(Fidelity::Compact);
        assert!(
            compact.contains("tester#2"),
            "the latest firing shows:\n{compact}"
        );
        assert!(
            !compact.contains("gate green"),
            "the older firing is collapsed"
        );
        assert!(!compact.contains("planner"), "the ignored stage is hidden");
        assert!(compact.contains("seed_brief: do it"));
        assert!(!compact.contains("stray_key"), "allow keys filter context");
        assert!(!compact.contains("tests_passed"), "the two lists intersect");

        let plain = Preamble {
            goal:   "G",
            run_id: "r",
            stages: &stages,
            nodes:  &nodes,
            kv:     &kv,
            policy: None,
        };
        let plain_render = plain.render(Fidelity::Compact);
        assert!(plain_render.contains("planner"));
        assert!(plain_render.contains("stray_key"));
    }

    /// Presence pin: the budget caps a rendered preamble end to end.
    #[test]
    fn the_budget_caps_a_rendered_preamble() {
        use crate::fidelity::{Fidelity, Preamble, StageInfo};
        let stages = vec![StageInfo {
            id:     "tester".into(),
            kind:   Some("command".into()),
            script: Some("s".into()),
            model:  None,
        }];
        let nodes = serde_json::json!({
            "tester": {"status": "success", "output": {"stdout": "line\n", "outcome": "succeeded"}}
        });
        let p = PreamblePolicy {
            budget_kb: Some(1),
            ..PreamblePolicy::default()
        };
        let preamble = Preamble {
            goal:   &"g".repeat(2000),
            run_id: "r",
            stages: &stages,
            nodes:  &nodes,
            kv:     &serde_json::json!({}),
            policy: Some(&p),
        };
        let rendered = preamble.render(Fidelity::SummaryHigh);
        assert!(rendered.len() < 2000);
        assert!(rendered.contains("x.preamble_budget_kb=1 KB"));
    }

    /// Presence pin: `x.preamble_output_max_lines` caps a stage's output
    /// block inside the rendered preamble.
    #[test]
    fn the_output_max_lines_caps_an_output_block() {
        use crate::fidelity::{Fidelity, Preamble, StageInfo};
        let stages = vec![StageInfo {
            id:     "tester".into(),
            kind:   Some("command".into()),
            script: Some("s".into()),
            model:  None,
        }];
        let stdout = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let nodes = serde_json::json!({
            "tester": {"status": "success", "output": {"stdout": stdout, "outcome": "succeeded"}}
        });
        let p = PreamblePolicy {
            output_max_lines: Some(3),
            ..PreamblePolicy::default()
        };
        let preamble = Preamble {
            goal:   "G",
            run_id: "r",
            stages: &stages,
            nodes:  &nodes,
            kv:     &serde_json::json!({}),
            policy: Some(&p),
        };
        let compact = preamble.render(Fidelity::Compact);
        assert!(compact.contains("37 lines omitted"));
        assert!(!compact.contains("line 5\n"));
    }
}
