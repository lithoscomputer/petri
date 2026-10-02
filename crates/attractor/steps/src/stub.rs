//! Stub steps: one simulated step per Fabro kind, the way Fabro's `--dry-run`
//! handlers simulate a stage.
//!
//! A stub returns `Success` with `output.outcome = "succeeded"` and
//! `output.simulated = true`; a human gate picks its first choice as the
//! preferred label. A node's config may carry a `simulate` object that scripts
//! the outcome instead — one of the four Fabro outcomes, a failure class, a
//! preferred label, suggested targets, context updates — which is how a test
//! drives every routing tier and failure policy through a real run with no
//! model, shell or person.
//!
//! A stub runs no process and reads no file, so the scope it fires in needs
//! nothing behind it. What stands behind it is the host's choice:
//! `petri run --dry-run` puts the runtime's sandboxes on the simulated
//! provider ([`Runtime::simulated_sandboxes`]), so no plugin is launched for
//! the backend the workflow's environment selects and its scope records name
//! the provider `simulated`; a host whose own hooks work a real workspace
//! during a dry run (Fabro checkpoints its host workspace) keeps the
//! sandboxes it configured.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use frontend_attractor::Policy;
use frontend_attractor::kinds::{ALL, HUMAN_KIND, StageOutcome};
use frontend_attractor::labels::strip_accelerator;
use ir::placeholder::contains_placeholder;
use ir::{FailureClass, Outcome, StepKindId, Value};
use runtime::Runtime;
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx, StepRunner};

use crate::outcome::{ExplicitRoutes, Stage};
use crate::parallel::{BranchStep, FanInStep, ForkStep};

/// What a stub is told to return.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Simulate {
    /// `succeeded` (default), `partially_succeeded`, `failed` or `skipped`.
    #[serde(default)]
    pub outcome:            Option<StageOutcome>,
    /// The failure class when `outcome` is `failed`: `retry_requested` asks
    /// for another attempt.
    #[serde(default)]
    pub failure_class:      Option<String>,
    #[serde(default)]
    pub failure_reason:     Option<String>,
    #[serde(default)]
    pub preferred_label:    Option<String>,
    #[serde(default)]
    pub suggested_next_ids: Vec<String>,
    #[serde(default)]
    pub context_updates:    BTreeMap<SmolStr, Value>,
    /// Scripted per call: the n-th time this node's stub runs in a run — an
    /// attempt after a retry, a second visit after a goal gate sent the run
    /// back, or a `loop_restart` — takes the n-th entry, and the last entry
    /// repeats. Fabro's engine calls a handler once per attempt in the same
    /// order, so the oracle generator scripts its handlers the same way.
    #[serde(default)]
    pub calls:              Vec<Self>,
}

#[derive(Deserialize)]
struct StubConfig {
    #[serde(default)]
    node:                 Option<String>,
    #[serde(default)]
    on_failure:           Option<Policy>,
    #[serde(default)]
    on_retries_exhausted: Option<Policy>,
    #[serde(default)]
    choices:              Vec<Choice>,
    #[serde(default)]
    freeform_target:      Option<String>,
    #[serde(default)]
    simulate:             Option<Simulate>,
    /// The node's explicit routes, so a simulated failure is promoted the
    /// way a real one is.
    #[serde(default)]
    routes:               Option<ExplicitRoutes>,
    #[serde(default)]
    kv:                   Value,
    #[serde(flatten)]
    _rest:                BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Choice {
    key:   String,
    label: String,
    to:    String,
}

/// Scripts for stubs, by node name, registered as a run capability. A
/// registered graph must not change after lowering (a parallel branch's
/// child graph is named by its digest), so a test that scripts a stage's
/// stub hands the scripts to the runtime instead of editing the config. A
/// node's own `simulate` config wins when it has one.
#[derive(Clone, Debug, Default)]
pub struct StubScripts(pub BTreeMap<String, Simulate>);

/// The simulated step for one kind.
pub struct StubStep {
    kind:  StepKindId,
    calls: Arc<Mutex<HashMap<String, usize>>>,
}

impl StubStep {
    pub fn new(kind: StepKindId) -> Self {
        Self {
            kind,
            calls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn with_calls(kind: StepKindId, calls: Arc<Mutex<HashMap<String, usize>>>) -> Self {
        Self { kind, calls }
    }
}

impl ir::StepKind for StubStep {
    fn id(&self) -> StepKindId {
        self.kind.clone()
    }

    fn name(&self) -> &str {
        self.kind.as_str()
    }

    fn validate_config(&self, config: &Value) -> Result<(), steps::StepFailure> {
        if contains_placeholder(config) {
            return Ok(());
        }
        serde_json::from_value::<StubConfig>(config.clone())
            .map(drop)
            .map_err(|e| steps::StepFailure {
                class:   FailureClass::new_static("bad_config"),
                message: e.to_string(),
            })
    }
}

#[async_trait::async_trait]
impl StepRunner for StubStep {
    async fn run(&self, ctx: StepCtx) -> Outcome {
        let config: StubConfig = match serde_json::from_value(ctx.config.clone()) {
            Ok(config) => config,
            Err(e) => {
                return steps::StepFailure {
                    class:   FailureClass::new_static("bad_config"),
                    message: format!("step config is invalid: {e}"),
                }
                .into();
            }
        };
        let node = config.node.clone().unwrap_or_else(|| ctx.node.to_string());
        let mut script = config
            .simulate
            .clone()
            .or_else(|| {
                ctx.capability::<StubScripts>()
                    .and_then(|scripts| scripts.0.get(&node).cloned())
            })
            .unwrap_or_default();
        if !script.calls.is_empty() {
            let call = self.next_call(ctx.env.workspace_path(), &ctx.node);
            let chosen = script.calls[call.min(script.calls.len() - 1)].clone();
            script = chosen;
        }
        let mut output = json!({
            "simulated": true,
            "node": node,
            "text": format!("[Simulated] {}", ctx.node),
        });
        let answers = script
            .outcome
            .is_none_or(|outcome| outcome == StageOutcome::Succeeded);
        if self.kind == HUMAN_KIND
            && answers
            && script.preferred_label.is_none()
            && script.suggested_next_ids.is_empty()
        {
            // Fabro's dry run answers a human gate with its first choice.
            if let Some(first) = config.choices.first() {
                script.preferred_label = Some(first.label.clone());
                script.suggested_next_ids = vec![first.to.clone()];
                script
                    .context_updates
                    .entry(SmolStr::new("human.gate.selected"))
                    .or_insert_with(|| json!(first.key));
                script
                    .context_updates
                    .entry(SmolStr::new("human.gate.label"))
                    .or_insert_with(|| json!(first.label));
            } else if let Some(target) = &config.freeform_target {
                script.suggested_next_ids = vec![target.clone()];
            }
        }
        if let Some(label) = &script.preferred_label {
            output["preferred_label"] = json!(strip_accelerator(label));
        }
        if !script.suggested_next_ids.is_empty() {
            output["suggested_next_ids"] = json!(script.suggested_next_ids);
        }
        let stage_outcome = script.outcome.unwrap_or_default();
        let mut stage = match stage_outcome {
            StageOutcome::Failed => {
                let class = script.failure_class.clone().unwrap_or_default();
                let reason = script
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| format!("[Simulated] {} failed", ctx.node));
                Stage::failed(reason, &class, config.on_failure)
            }
            other => Stage::new(other, config.on_failure),
        };
        stage.output.clone_from(
            output
                .as_object()
                .expect("the simulated output is constructed as an object"),
        );
        stage.context_updates = script.context_updates;
        stage
            .with_routing(config.routes.clone(), config.kv.clone())
            .with_retries(config.on_retries_exhausted, ctx.is_final_attempt())
            .into_outcome(&node)
    }
}

/// How many times a scripted stub has run for `node` in the run whose
/// workspace is `workspace`, counted here because a step has no memory of
/// its own across attempts, visits and restarted executions. The workspace
/// path is one per invocation, so a run's successor executions share the
/// count while different runs (different run dirs) do not. Test-only state:
/// replay never runs a step, so the counter cannot touch determinism.
impl StubStep {
    fn next_call(&self, workspace: &str, node: &str) -> usize {
        let mut calls = self
            .calls
            .lock()
            .expect("the stub call table is not poisoned");
        let count = calls.entry(format!("{workspace}/{node}")).or_insert(0);
        let current = *count;
        *count += 1;
        current
    }
}

/// A `Step`-shaped wrapper so a caller can register a stub under a typed
/// config path too.
#[async_trait::async_trait]
impl Step for Simulate {
    const NAME: &'static str = "attractor/simulate";
    type Config = Value;

    async fn run(&self, _config: Value, _ctx: StepCtx) -> Outcome {
        Outcome::success(Value::Null)
    }
}

/// Register a stub for every Fabro step kind. The sandboxes are left as the
/// runtime has them: a caller that wants a dry run to touch no provider adds
/// [`Runtime::simulated_sandboxes`], as `petri run --dry-run` does.
pub fn register_stubs(runtime: Runtime) -> Runtime {
    let mut registry = runtime.registry().clone();
    let calls = Arc::new(Mutex::new(HashMap::new()));
    for kind in ALL {
        registry.register_runner(Arc::new(StubStep::with_calls(
            (*kind).clone(),
            calls.clone(),
        )));
    }
    // The structural steps run for real: a dry run still takes each fork's
    // snapshot, still starts each branch child (whose stages are simulated)
    // and still joins the results.
    registry.register(ForkStep);
    registry.register(BranchStep);
    registry.register(FanInStep);
    runtime.steps(registry)
}
