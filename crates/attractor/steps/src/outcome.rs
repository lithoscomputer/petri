//! How a Fabro stage's result becomes an engine outcome: the closed outcome
//! set, the failure class, failure promotion under `on_failure` and
//! `on_retries_exhausted`, and the bookkeeping context keys.
//!
//! Fabro's executor promotes a failed stage under `on_failure="succeed"` only
//! when no explicit route matches the failed outcome, and it checks the
//! routes against the prospective context: the run context with the stage's
//! own updates applied. Petri classifies once, at the step boundary, so the
//! step performs that check here with the node's explicit routes from its
//! config ([`Stage::routes`]). A failure an explicit route matches stays a
//! failure and the routing tiers take that route; any other failure becomes
//! a `PartialSuccess` that keeps the failure in `underlying`, reports
//! `succeeded` (or `partially_succeeded` under Petri's `partially_succeed`
//! extension), and routes as a success.
//!
//! A retryable failure (class `retry_requested`) with an attempt left is
//! returned as a failure for the engine to retry. On the final attempt it is
//! the stage's outcome, and Fabro's order applies to it as it does to an
//! ordinary failure: `allow_partial` (`on_retries_exhausted=
//! "partially_succeed"`) makes it a partial success with no route check, as
//! Fabro finalizes an exhausted retry; under `succeed` the explicit routes
//! are checked first and an unmatched failure is promoted; `route` and
//! `exit` leave it failed. The step knows the attempt is final from
//! [`StepCtx::is_final_attempt`](steps::StepCtx::is_final_attempt), so the
//! engine's own exhaustion policy stays `Fail` for every Fabro node.

use std::collections::BTreeMap;

use frontend_attractor::kinds::{RETRY_REQUESTED_CLASS, StageOutcome};
use frontend_attractor::labels::routing_key;
use frontend_attractor::{Policy, condition};
use ir::{FailureClass, FailureInfo, Outcome, Status, Value};
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;

/// The outcome a stage reported, in Fabro's vocabulary: what Fabro's own
/// events show for the stage. A failure `on_failure="succeed"` promoted is a
/// `PartialSuccess` on the record whose reported `outcome` is `succeeded`;
/// every other status maps by [`fabro_outcome`].
pub fn reported_outcome(outcome: &Outcome) -> StageOutcome {
    if matches!(outcome.status, Status::PartialSuccess { .. })
        && outcome.output.get("outcome").and_then(Value::as_str) == Some("succeeded")
    {
        return StageOutcome::Succeeded;
    }
    fabro_outcome(&outcome.status)
}

/// The Fabro spelling of an engine status.
pub fn fabro_outcome(status: &Status) -> StageOutcome {
    match status {
        Status::Success => StageOutcome::Succeeded,
        Status::PartialSuccess { .. } => StageOutcome::PartiallySucceeded,
        Status::Skipped => StageOutcome::Skipped,
        Status::Failure(_) | Status::Cancelled | Status::TimedOut => StageOutcome::Failed,
    }
}

/// The node's explicit routes, as the frontend lowered them
/// (`frontend_attractor::ROUTES_KEY`): what Fabro's edge selection would try
/// before falling through to the failure policy.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ExplicitRoutes {
    /// Every conditional edge's condition text.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Every unconditional labelled edge's label, in routing-key form.
    #[serde(default)]
    pub labels:     Vec<String>,
    /// Every unconditional edge's target id.
    #[serde(default)]
    pub targets:    Vec<String>,
}

impl ExplicitRoutes {
    /// Whether an explicit route matches a failed outcome: a condition that
    /// holds over the failed status and the prospective context, a preferred
    /// label naming a labelled edge, or a suggested target naming an edge.
    pub fn matches_failure(
        &self,
        output: &serde_json::Map<String, Value>,
        kv: &Value,
        context_updates: &BTreeMap<SmolStr, Value>,
    ) -> bool {
        let label = output
            .get("preferred_label")
            .and_then(Value::as_str)
            .map(routing_key);
        if let Some(label) = &label
            && self.labels.iter().any(|candidate| candidate == label)
        {
            return true;
        }
        let suggested = output
            .get("suggested_next_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .any(|id| self.targets.iter().any(|target| target == id))
            });
        if suggested {
            return true;
        }
        if self.conditions.is_empty() {
            return false;
        }
        let mut run = ir::RunContext::new();
        if let Value::Object(map) = kv {
            run.merge(
                &map.iter()
                    .map(|(k, v)| (SmolStr::new(k), v.clone()))
                    .collect(),
            );
        }
        run.merge(context_updates);
        let statics = ir::StaticCtx::new()
            .bind("status", json!("failure"))
            .bind("output", Value::Object(output.clone()));
        let env = ir::EvalEnv::new(&Value::Null, &run, &statics);
        self.conditions.iter().any(|text| {
            let mut table = ir::ExprTable::new();
            let mut diags = frontend::Diagnostics::new();
            let span = frontend::Span::file("condition");
            condition::lower(text, &mut table, &span, &mut diags, false)
                .is_some_and(|expr| ir::eval_bool(&table, expr, &env).unwrap_or(false))
        })
    }
}

/// What a stage reports, before it becomes an outcome.
pub struct Stage {
    pub outcome:              StageOutcome,
    pub failure_reason:       Option<String>,
    pub failure_class:        String,
    pub output:               serde_json::Map<String, Value>,
    pub context_updates:      BTreeMap<SmolStr, Value>,
    /// The node's `on_failure` policy, from its config.
    pub on_failure:           Option<Policy>,
    /// The node's `on_retries_exhausted` policy, from its config. Absent, an
    /// exhausted retry falls under `on_failure`, as Fabro applies its one
    /// policy to the exhausted retry.
    pub on_retries_exhausted: Option<Policy>,
    /// Whether no attempt follows this one, so a `retry_requested` failure
    /// is exhausted rather than retried. `false` until the step says
    /// otherwise ([`Stage::with_retries`]): a retryable failure is then left
    /// to the engine, which retries it or records it failed.
    pub final_attempt:        bool,
    /// The node's explicit routes, from its config, and the run context the
    /// step started with: what promotion checks a failure against.
    pub routes:               Option<ExplicitRoutes>,
    pub kv:                   Value,
}

/// What becomes of a failed stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    /// It stays a failure: an explicit route matches it, its policy routes
    /// or exits, or the engine retries it.
    Failed,
    /// The exhausted retry is accepted as a partial success, with no route
    /// check.
    Accepted,
    /// No explicit route matches, and the policy promotes it.
    Promoted(Policy),
}

impl Stage {
    pub fn new(outcome: StageOutcome, on_failure: Option<Policy>) -> Self {
        Self {
            outcome,
            failure_reason: None,
            failure_class: String::new(),
            output: serde_json::Map::new(),
            context_updates: BTreeMap::new(),
            on_failure,
            on_retries_exhausted: None,
            final_attempt: false,
            routes: None,
            kv: Value::Null,
        }
    }

    pub fn failed(reason: impl Into<String>, class: &str, on_failure: Option<Policy>) -> Self {
        let mut stage = Self::new(StageOutcome::Failed, on_failure);
        stage.failure_reason = Some(reason.into());
        stage.failure_class = class.to_string();
        stage
    }

    /// Give the stage what promotion needs: the node's explicit routes and
    /// the run context at spawn. A step whose config carries neither leaves
    /// this alone, and a failure under `succeed` is promoted unconditionally.
    #[must_use]
    pub fn with_routing(mut self, routes: Option<ExplicitRoutes>, kv: Value) -> Self {
        self.routes = routes;
        self.kv = kv;
        self
    }

    /// Give the stage what an exhausted retry needs: the node's
    /// `on_retries_exhausted` policy and whether this attempt is the last one
    /// the retry policy allows. A step that never calls this leaves every
    /// retryable failure to the engine.
    #[must_use]
    pub fn with_retries(
        mut self,
        on_retries_exhausted: Option<Policy>,
        final_attempt: bool,
    ) -> Self {
        self.on_retries_exhausted = on_retries_exhausted;
        self.final_attempt = final_attempt;
        self
    }

    /// Whether this failure asks for another attempt.
    pub fn retry_requested(&self) -> bool {
        self.failure_class == RETRY_REQUESTED_CLASS
    }

    /// Whether this failure is a retry request with no attempt left.
    fn exhausted(&self) -> bool {
        self.retry_requested() && self.final_attempt
    }

    /// Whether an explicit route matches this failure, so the failure policy
    /// must leave it failed. A stage whose config carries no routes is
    /// promoted unconditionally.
    fn explicit_route_matches(&self) -> bool {
        self.routes.as_ref().is_some_and(|routes| {
            routes.matches_failure(&self.output, &self.kv, &self.context_updates)
        })
    }

    /// What the failure policy makes of this failure, in Fabro's order. A
    /// retry request with an attempt left is the engine's to retry. An
    /// exhausted one falls under `on_retries_exhausted` (else `on_failure`):
    /// `partially_succeed` accepts it with no route check, as Fabro's
    /// `allow_partial` finalizes an exhausted retry. Every other failure
    /// falls under its policy's promotion: `succeed` (and the
    /// `partially_succeed` extension as `on_failure`) promotes it unless an
    /// explicit route matches; `route` and `exit` leave it failed.
    fn disposition(&self) -> Disposition {
        if self.retry_requested() && !self.final_attempt {
            return Disposition::Failed;
        }
        let policy = if self.exhausted() {
            self.on_retries_exhausted.or(self.on_failure)
        } else {
            self.on_failure
        };
        match policy {
            Some(Policy::PartiallySucceed) if self.exhausted() => Disposition::Accepted,
            Some(policy @ (Policy::Succeed | Policy::PartiallySucceed)) => {
                if self.explicit_route_matches() {
                    Disposition::Failed
                } else {
                    Disposition::Promoted(policy)
                }
            }
            Some(Policy::Route | Policy::Exit) | None => Disposition::Failed,
        }
    }

    /// The engine outcome. A failure that its policy accepts or promotes
    /// becomes a `PartialSuccess` here, the one classification point, with
    /// the failure kept in `underlying`. Under `succeed` the reported
    /// `outcome` is `succeeded`, as Fabro reports a promoted stage; under
    /// `allow_partial` exhaustion and Petri's `partially_succeed` extension
    /// it is `partially_succeeded`. The event log never records a clean
    /// success for a failed step.
    pub fn into_outcome(mut self, node: &str) -> Outcome {
        let mut reported = None;
        let status = match self.outcome {
            StageOutcome::Succeeded => Status::Success,
            StageOutcome::PartiallySucceeded => Status::partial_clean(),
            StageOutcome::Skipped => Status::Skipped,
            StageOutcome::Failed => {
                let reason = self
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| format!("stage `{node}` failed"));
                let info = FailureInfo::new(reason)
                    .with_class(FailureClass::new(self.failure_class.as_str()));
                match self.disposition() {
                    Disposition::Failed => Status::Failure(info),
                    Disposition::Accepted | Disposition::Promoted(Policy::PartiallySucceed) => {
                        Status::partial(info)
                    }
                    Disposition::Promoted(_) => {
                        reported = Some(StageOutcome::Succeeded);
                        let scope = if self.exhausted() {
                            "on_retries_exhausted"
                        } else {
                            "on_failure"
                        };
                        self.output.insert(
                            "promoted".into(),
                            json!(format!(
                                "{scope}=succeed promoted a failed outcome to succeeded"
                            )),
                        );
                        Status::partial(info)
                    }
                }
            }
        };
        let reported = reported.unwrap_or_else(|| fabro_outcome(&status));
        self.output
            .insert("outcome".into(), json!(reported.as_str()));
        self.output
            .insert("failure_class".into(), json!(self.failure_class));
        if let Some(reason) = &self.failure_reason {
            self.output.insert("failure_reason".into(), json!(reason));
        }
        let mut outcome = Outcome::new(status, Value::Object(self.output));
        outcome.context_updates = self.context_updates;
        outcome
            .context_updates
            .insert(SmolStr::new("failure_class"), json!(self.failure_class));
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> ExplicitRoutes {
        ExplicitRoutes {
            conditions: vec!["outcome=failed".into(), "context.mode=fast".into()],
            labels:     vec!["retry".into()],
            targets:    vec!["b".into(), "c".into()],
        }
    }

    #[test]
    fn an_explicit_failure_edge_keeps_the_failure() {
        let stage = Stage::failed("boom", "", Some(Policy::Succeed))
            .with_routing(Some(routes()), Value::Null);
        let outcome = stage.into_outcome("a");
        assert!(matches!(outcome.status, Status::Failure(_)));
        assert_eq!(outcome.output["outcome"], json!("failed"));
    }

    #[test]
    fn an_unmatched_failure_is_promoted_and_keeps_its_evidence() {
        let routes = ExplicitRoutes {
            conditions: vec!["context.mode=fast".into()],
            ..ExplicitRoutes::default()
        };
        let stage = Stage::failed("boom", "exit_status:3", Some(Policy::Succeed))
            .with_routing(Some(routes), json!({ "mode": "slow" }));
        let outcome = stage.into_outcome("a");
        let Status::PartialSuccess { underlying } = &outcome.status else {
            panic!("promoted: {:?}", outcome.status);
        };
        assert_eq!(
            underlying
                .as_ref()
                .and_then(|failure| failure.failure_info())
                .map(|f| f.message.as_str()),
            Some("boom")
        );
        assert_eq!(outcome.output["outcome"], json!("succeeded"));
        assert_eq!(outcome.output["failure_class"], json!("exit_status:3"));
    }

    #[test]
    fn prospective_context_and_labels_count_as_explicit_routes() {
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed))
            .with_routing(Some(routes()), json!({ "mode": "slow" }));
        stage
            .context_updates
            .insert(SmolStr::new("mode"), json!("fast"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed)).with_routing(
            Some(ExplicitRoutes {
                labels: vec!["retry".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("preferred_label".into(), json!("[R] Retry"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        let mut stage = Stage::failed("boom", "", Some(Policy::Succeed)).with_routing(
            Some(ExplicitRoutes {
                targets: vec!["c".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("suggested_next_ids".into(), json!(["c"]));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
    }

    #[test]
    fn a_retryable_failure_with_attempts_left_is_left_to_the_engine() {
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Succeed));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Route))
            .with_retries(Some(Policy::Succeed), false);
        let outcome = stage.into_outcome("a");
        assert!(matches!(outcome.status, Status::Failure(_)));
        assert_eq!(outcome.output["outcome"], json!("failed"));
        assert_eq!(outcome.output["failure_class"], json!("retry_requested"));
    }

    /// The last retryable failure: `succeed` checks the explicit routes first,
    /// on every tier, and only an unmatched failure is promoted.
    #[test]
    fn an_exhausted_retry_under_succeed_keeps_a_failure_an_explicit_route_matches() {
        let exhausted = |on_failure: Policy| {
            Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(on_failure))
                .with_retries(Some(Policy::Succeed), true)
        };
        // A condition over the failed status.
        let outcome = exhausted(Policy::Route)
            .with_routing(Some(routes()), Value::Null)
            .into_outcome("a");
        let Status::Failure(info) = &outcome.status else {
            panic!("kept failed: {:?}", outcome.status);
        };
        assert_eq!(info.class, RETRY_REQUESTED_CLASS);
        assert_eq!(info.message, "flaky");
        assert_eq!(outcome.output["outcome"], json!("failed"));
        assert!(outcome.output.get("promoted").is_none());
        // A condition over the prospective context.
        let mut stage = exhausted(Policy::Route).with_routing(
            Some(ExplicitRoutes {
                conditions: vec!["context.mode=fast".into()],
                ..ExplicitRoutes::default()
            }),
            json!({ "mode": "slow" }),
        );
        stage
            .context_updates
            .insert(SmolStr::new("mode"), json!("fast"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        // A preferred label.
        let mut stage = exhausted(Policy::Route).with_routing(
            Some(ExplicitRoutes {
                labels: vec!["retry".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("preferred_label".into(), json!("[R] Retry"));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
        // A suggested target.
        let mut stage = exhausted(Policy::Route).with_routing(
            Some(ExplicitRoutes {
                targets: vec!["c".into()],
                ..ExplicitRoutes::default()
            }),
            Value::Null,
        );
        stage
            .output
            .insert("suggested_next_ids".into(), json!(["c"]));
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
    }

    #[test]
    fn an_exhausted_retry_under_succeed_is_promoted_when_no_explicit_route_matches() {
        let routes = ExplicitRoutes {
            conditions: vec!["outcome=succeeded".into(), "context.mode=fast".into()],
            labels:     vec!["retry".into()],
            targets:    vec!["b".into()],
        };
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Route))
            .with_retries(Some(Policy::Succeed), true)
            .with_routing(Some(routes), json!({ "mode": "slow" }));
        let outcome = stage.into_outcome("a");
        let Status::PartialSuccess { underlying } = &outcome.status else {
            panic!("promoted: {:?}", outcome.status);
        };
        let underlying = underlying
            .as_ref()
            .and_then(|failure| failure.failure_info())
            .expect("the failure is kept");
        assert_eq!(underlying.message, "flaky");
        assert_eq!(underlying.class, RETRY_REQUESTED_CLASS);
        assert_eq!(outcome.output["outcome"], json!("succeeded"));
        assert_eq!(outcome.output["failure_class"], json!("retry_requested"));
        assert_eq!(
            outcome.output["promoted"],
            json!("on_retries_exhausted=succeed promoted a failed outcome to succeeded")
        );
        assert_eq!(reported_outcome(&outcome), StageOutcome::Succeeded);
    }

    /// Without its own `on_retries_exhausted`, an exhausted retry falls under
    /// `on_failure`, as Fabro applies its one policy to it.
    #[test]
    fn an_exhausted_retry_falls_under_on_failure_when_no_exhaustion_policy_is_set() {
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Succeed))
            .with_retries(None, true);
        let outcome = stage.into_outcome("a");
        assert!(matches!(outcome.status, Status::PartialSuccess { .. }));
        assert_eq!(outcome.output["outcome"], json!("succeeded"));
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Route))
            .with_retries(None, true);
        assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
    }

    /// `allow_partial`: the exhausted retry is a partial success even when
    /// an explicit failure route would match, as Fabro finalizes it before
    /// any route is considered.
    #[test]
    fn an_exhausted_retry_under_allow_partial_is_accepted_without_a_route_check() {
        let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Route))
            .with_retries(Some(Policy::PartiallySucceed), true)
            .with_routing(Some(routes()), Value::Null);
        let outcome = stage.into_outcome("a");
        let Status::PartialSuccess { underlying } = &outcome.status else {
            panic!("accepted: {:?}", outcome.status);
        };
        assert_eq!(
            underlying
                .as_ref()
                .and_then(|failure| failure.failure_info())
                .map(|f| f.class.as_str()),
            Some(RETRY_REQUESTED_CLASS)
        );
        assert_eq!(outcome.output["outcome"], json!("partially_succeeded"));
        assert!(outcome.output.get("promoted").is_none());
        assert_eq!(reported_outcome(&outcome), StageOutcome::PartiallySucceeded);
    }

    #[test]
    fn an_exhausted_retry_under_route_or_exit_stays_failed() {
        for policy in [Policy::Route, Policy::Exit] {
            let stage = Stage::failed("flaky", RETRY_REQUESTED_CLASS, Some(Policy::Succeed))
                .with_retries(Some(policy), true);
            let outcome = stage.into_outcome("a");
            assert!(matches!(outcome.status, Status::Failure(_)), "{policy:?}");
            assert_eq!(outcome.output["failure_class"], json!("retry_requested"));
        }
    }

    /// An ordinary failure is untouched by the exhaustion policy: it falls
    /// under `on_failure` whether or not attempts remain.
    #[test]
    fn a_non_retryable_failure_ignores_the_exhaustion_policy() {
        for final_attempt in [false, true] {
            let stage = Stage::failed("boom", "exit_status:3", Some(Policy::Route))
                .with_retries(Some(Policy::Succeed), final_attempt);
            assert!(matches!(stage.into_outcome("a").status, Status::Failure(_)));
            let stage = Stage::failed("boom", "exit_status:3", Some(Policy::Succeed))
                .with_retries(Some(Policy::Route), final_attempt);
            let outcome = stage.into_outcome("a");
            assert!(matches!(outcome.status, Status::PartialSuccess { .. }));
            assert_eq!(
                outcome.output["promoted"],
                json!("on_failure=succeed promoted a failed outcome to succeeded")
            );
        }
    }
}
