//! `attractor/agent`: ACP or native Pebble, with shared prompt assembly,
//! output validation, repair turns, and routing.
//!
//! The prompt is Fabro's: the fidelity preamble for the mode the incoming
//! edge, the node and the graph resolve to ([`crate::fidelity`]), the node's
//! prompt, and the output contract. A native node at `full` fidelity with a
//! resolved thread continues the thread's retained conversation
//! ([`crate::sessions`]); every other node starts fresh. A native session
//! reads the project documents Fabro's profile selects ([`crate::memory`])
//! and runs its tools through the hook middleware ([`crate::hooks::tools`]).
//! An ACP agent gets the same prompt; its tool hooks are best effort and it
//! never reuses a thread.
//!
//! A native node runs on a fallback plan ([`crate::fallback`]): the route
//! its model and provider resolve to, then the targets `[run.model.fallbacks]`
//! configures for that model. The plan is the one admission froze on the
//! node's config when the runtime had a catalog ([`crate::admission`]),
//! else the node builds it. Pebble runs the plan's remaining routes: a
//! provider-local model error moves the conversation to the next route, and
//! the session mirrors each move as Petri's events; a retained thread
//! carries its plan, at the route reached, to the next node.

pub(crate) mod backend;
use std::collections::BTreeMap;
use std::env;
use std::time::{Duration, Instant};

pub use backend::AgentBackend;
use backend::{AgentError, Session};
use execution::ExecutionIdentity;
use execution::controls::LiveTurns;
use frontend_attractor::Policy;
use frontend_attractor::kinds::{AGENT_KIND, MAX_OUTPUT_RETRIES, StageOutcome};
use frontend_attractor::mcps::McpServer;
use frontend_attractor::subagents::SubagentConfig;
use ir::{LogStream, Metrics, Outcome, StepKindId, Value};
use pebble_coding_agent::ShutdownReason;
use serde::Deserialize;
use serde_json::json;
use smol_str::SmolStr;
use steps::{Step, StepCtx};

use crate::acp::AgentCommand;
use crate::blobs::{self, OutputStore};
use crate::compaction;
use crate::contract::{Contract, Parsed, repair_message, validate};
use crate::fallback::{self, FrozenPlan, Plan, Route, StageRequest};
use crate::fidelity::{
    self, Fidelity, Incoming, Preamble, Resolved, Source, StageInfo, ThreadConfig,
};
use crate::outcome::{ExplicitRoutes, Stage};
use crate::pebble::{PebbleClient, Resume};
use crate::sessions::{Retained, SessionService};
use crate::stage::{self, RunInfo};

pub const KIND: StepKindId = AGENT_KIND;

/// The environment variable naming the ACP command for nodes that set none.
pub const DEFAULT_COMMAND_ENV: &str = "PETRI_ACP_COMMAND";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default)]
    pub backend:              AgentBackend,
    pub label:                String,
    pub node:                 String,
    #[serde(default)]
    pub kind:                 Option<String>,
    #[serde(default)]
    pub goal:                 String,
    #[serde(default)]
    pub prompt:               String,
    #[serde(default)]
    pub model:                Option<String>,
    #[serde(default)]
    pub provider:             Option<String>,
    #[serde(default)]
    pub reasoning_effort:     Option<String>,
    #[serde(default)]
    pub fidelity:             Option<String>,
    #[serde(default)]
    pub default_fidelity:     Option<String>,
    #[serde(default)]
    pub thread_id:            Option<String>,
    #[serde(default)]
    pub default_thread:       Option<String>,
    #[serde(default)]
    pub classes:              Vec<String>,
    /// The token that fired the node: the incoming edge's `from`,
    /// `fidelity` and `thread_id`.
    #[serde(default)]
    pub incoming:             Value,
    /// The node is a parallel branch's first node: threads are inert and
    /// `full` degrades.
    #[serde(default)]
    pub branch:               bool,
    /// Read project documents. Agent nodes always do; the flag is a prompt
    /// node's.
    #[serde(default = "default_true")]
    pub project_memory:       bool,
    #[serde(default)]
    pub speed:                Option<String>,
    #[serde(default)]
    pub max_tokens:           Option<i64>,
    /// `[run.model.fallbacks]` as lowered: the chains keyed by the requested
    /// model, references as written. Absent once admission resolved them.
    #[serde(default)]
    pub fallbacks:            BTreeMap<String, Vec<String>>,
    /// The plan admission froze for this node (`crate::admission`): the
    /// concrete routes and the notices. Absent when the graph was admitted
    /// without a catalog; the node then builds its own.
    #[serde(default)]
    pub plan:                 Option<FrozenPlan>,
    /// Context compaction, Fabro's values unless the frontend says otherwise
    /// (`crate::compaction`).
    #[serde(default)]
    pub compaction:           compaction::CompactionSettings,
    /// The workflow's own skill directories (`[run.agent] skills`), searched
    /// after Fabro's conventional ones.
    #[serde(default)]
    pub skill_dirs:           Vec<String>,
    /// The sub-agent tools a native session advertises and the bound on its
    /// agent tree; the lowering writes the reference defaults.
    #[serde(default)]
    pub subagents:            SubagentConfig,
    /// Every stage of the workflow, for the preamble.
    #[serde(default)]
    pub stages:               Value,
    #[serde(default)]
    pub output_schema:        Option<Value>,
    #[serde(default = "default_output_retries")]
    pub output_retries:       u64,
    #[serde(default)]
    pub acp:                  Option<Value>,
    /// The run's `[run.agent.mcps]` servers a native session connects to.
    #[serde(default)]
    pub mcps:                 Vec<McpServer>,
    #[serde(default)]
    pub on_failure:           Option<Policy>,
    #[serde(default)]
    pub on_retries_exhausted: Option<Policy>,
    /// The node's explicit routes, for failure promotion.
    #[serde(default, rename = "routes")]
    pub explicit_routes:      Option<ExplicitRoutes>,
    #[serde(default)]
    pub timeout_ms:           Option<u64>,
    #[serde(default)]
    pub kv:                   Value,
    /// A `for_each` branch's item, rendered as fenced untrusted data by the
    /// branch step. Appended after the prompt, as Fabro appends it.
    #[serde(default)]
    pub item_data:            Option<String>,
    #[serde(default)]
    pub nodes:                Value,
}

fn default_output_retries() -> u64 {
    2
}

fn default_true() -> bool {
    true
}

/// The `kind` of the `StepEvent::Custom` payload an agent node emits when
/// its fidelity and thread resolve: `{ kind, node, firing, attempt, fidelity,
/// fidelity_source, thread, thread_source, reused, backend }`.
pub const THREAD_EVENT: &str = "attractor.thread";

/// The `kind` of the `StepEvent::Custom` payload an agent node emits when a
/// host's interrupt stopped its current model turn, on either backend:
/// `{ kind, node, firing, attempt, backend, session }`. The session stays
/// open and the node continues with its next input. On the native backend
/// Pebble's own `RoundInterrupted` precedes it in the backend envelope.
pub const INTERRUPTED_EVENT: &str = "attractor.turn.interrupted";

pub struct AgentStep;

impl AgentConfig {
    fn command(&self) -> Result<AgentCommand, String> {
        if let Some(acp) = &self.acp {
            if let Some(line) = acp.get("command").and_then(Value::as_str) {
                return AgentCommand::from_command_line(line);
            }
            if let Some(config) = acp.get("config") {
                return AgentCommand::from_config(config);
            }
        }
        match env::var(DEFAULT_COMMAND_ENV) {
            Ok(line) if !line.trim().is_empty() => AgentCommand::from_command_line(&line),
            _ => Err(format!(
                "node `{}` names no ACP agent: set `acp.command` or `acp.config` on the node or \
                 the graph, or {DEFAULT_COMMAND_ENV} in the environment",
                self.node
            )),
        }
    }

    fn contract(&self) -> Result<Contract, String> {
        Contract::from_config(self.output_schema.as_ref())
    }

    /// Fabro's resolution of this node's fidelity and thread. `degrade` is
    /// the resume fallback: the thread's conversation is gone.
    pub fn resolve(&self, degrade: bool) -> Resolved {
        let config = ThreadConfig {
            fidelity:         self.fidelity.clone(),
            default_fidelity: self.default_fidelity.clone(),
            thread_id:        self.thread_id.clone(),
            default_thread:   self.default_thread.clone(),
            classes:          self.classes.clone(),
        };
        fidelity::resolve(
            &config,
            &Incoming::from_value(&self.incoming),
            self.branch,
            degrade,
        )
    }

    /// The prompt as the agent receives it: the preamble for `fidelity`,
    /// the node's prompt, the contract.
    pub fn assemble(&self, fidelity: Fidelity, run_id: &str, contract: &Contract) -> String {
        let stages: Vec<StageInfo> = fidelity::stages(&self.stages);
        let preamble = Preamble {
            goal: &self.goal,
            run_id,
            stages: &stages,
            nodes: &self.nodes,
            kv: &self.kv,
        };
        let mut body = self.prompt.clone();
        if let Some(item) = self.item_data.as_deref().filter(|item| !item.is_empty()) {
            // A `for_each` branch's item follows the prompt, as Fabro renders
            // the target for one item.
            body.push_str("\n\n");
            body.push_str(item);
        }
        let mut out = preamble.prompt(fidelity, &body);
        out.push_str(&contract.prompt_suffix());
        out
    }

    /// The `provider/model` selector a native session runs on.
    pub fn selector(&self) -> Option<String> {
        let model = self.model.as_deref().filter(|s| !s.trim().is_empty())?;
        Some(self.provider.as_ref().map_or_else(
            || model.to_owned(),
            |provider| {
                if model.starts_with(&format!("{provider}/")) {
                    model.to_owned()
                } else {
                    format!("{provider}/{model}")
                }
            },
        ))
    }
}

#[async_trait::async_trait]
impl Step for AgentStep {
    const NAME: &'static str = "attractor/agent";
    type Config = AgentConfig;

    async fn run(&self, mut config: AgentConfig, mut ctx: StepCtx) -> Outcome {
        if let Some(store) = ctx.capability::<OutputStore>() {
            blobs::restore_fabro_view(&mut config.kv, &mut config.nodes, store.0.as_ref()).await;
        }
        let on_failure = config.on_failure;
        let on_retries_exhausted = config.on_retries_exhausted;
        let final_attempt = ctx.is_final_attempt();
        let routes = config.explicit_routes.clone();
        let kv = config.kv.clone();
        let fail = |reason: String, class: &str| {
            Stage::failed(reason, class, on_failure)
                .with_routing(routes.clone(), kv.clone())
                .with_retries(on_retries_exhausted, final_attempt)
                .into_outcome(&config.node)
        };
        if config.output_retries > MAX_OUTPUT_RETRIES {
            return fail(
                format!(
                    "`output_retries={}` exceeds the hard maximum of {MAX_OUTPUT_RETRIES}",
                    config.output_retries
                ),
                "bad_config",
            );
        }
        let contract = match config.contract() {
            Ok(contract) => contract,
            Err(message) => return fail(message, "bad_config"),
        };
        let started = Instant::now();
        stage::record(&ctx);
        let run_id = ctx
            .capability::<RunInfo>()
            .map(|run| run.run_id.clone())
            .unwrap_or_default();
        // A retained thread is reused only by a native node at effective
        // `full` fidelity; a retained conversation the run lost (a resume, a
        // failed predecessor) degrades the node to `summary:high`, as Fabro
        // documents.
        let sessions = ctx.capability::<SessionService>();
        let mut resolved = config.resolve(false);
        let mut retained: Option<Retained> = None;
        // A native node's fallback plan: its route, then the configured
        // chain for that model. An ACP node has no plan (the command owns
        // its model).
        let mut planned: Option<FrozenPlan> = None;
        if config.backend == AgentBackend::Api {
            let Some(client) = ctx.capability::<PebbleClient>() else {
                return fail(
                    "Native Pebble requires a PebbleClient capability".into(),
                    "pebble_unconfigured",
                );
            };
            // A provider with no model (`--provider` alone, or a bare
            // `default_provider`) runs the provider's default model.
            if let Err(message) = fallback::fill_provider_default(
                &client.0,
                &mut config.model,
                config.provider.as_deref(),
            ) {
                return fail(
                    format!("agent node `{}`: {message}", config.node),
                    "bad_config",
                );
            }
            let request = StageRequest::for_agent(&config);
            planned = match fallback::plan_for_stage(&mut ctx, &client.0, &request).await {
                Ok(planned) => Some(planned),
                Err(AgentError::Cancelled) => return Outcome::cancelled(),
                Err(AgentError::Failed { class, message }) => return fail(message, &class),
                Err(AgentError::Model(failure)) => {
                    return fail(failure.to_string(), &failure.class());
                }
            };
        }
        let requested_selector = planned
            .as_ref()
            .map(|p| p.original.selector())
            .unwrap_or_default();
        if config.backend == AgentBackend::Api
            && resolved.fidelity == Fidelity::Full
            && let Some(thread) = resolved.thread.clone()
        {
            retained = sessions.as_ref().and_then(|s| s.take(&thread));
            match &retained {
                Some(kept) if requested_selector != kept.selector => {
                    ctx.log(
                        LogStream::Stderr,
                        format!(
                            "thread `{thread}` was retained on `{}`; this node runs \
                             `{requested_selector}`, so it starts a fresh conversation",
                            kept.selector,
                        ),
                    )
                    .await;
                    retained = None;
                }
                None if sessions.as_ref().is_some_and(|s| s.lost(&thread)) => {
                    resolved = config.resolve(true);
                }
                _ => {}
            }
        }
        if config.backend == AgentBackend::Acp
            && resolved.thread.is_some()
            && resolved.fidelity == Fidelity::Full
        {
            ctx.log(
                LogStream::Stderr,
                "the ACP backend does not reuse threads; this node starts a fresh agent session",
            )
            .await;
        }
        let reused = retained.is_some();
        let _ = ctx
            .logs
            .send(ir::StepEvent::Custom(json!({
                "kind": THREAD_EVENT,
                "node": config.node,
                "firing": ctx.firing,
                "attempt": ctx.attempt,
                "fidelity": resolved.fidelity.as_str(),
                "fidelity_source": resolved.fidelity_source.as_str(),
                "thread": resolved.thread,
                "thread_source": resolved.thread_source.map(Source::as_str),
                "reused": reused,
                "backend": match config.backend { AgentBackend::Api => "api", AgentBackend::Acp => "acp" },
            })))
            .await;
        // A retained thread continues on its own plan, at the route it
        // reached; a fresh session starts this node's plan at its original.
        // Pebble runs the routes after the one reached; the session's plan
        // is read back once the node's turns are done.
        let (plan, resume) = match (retained, planned) {
            (Some(kept), _) => {
                let plan = kept.plan.clone();
                (kept.plan, Resume::Export {
                    export: Box::new(kept.export),
                    plan,
                })
            }
            (None, Some(frozen)) => {
                let plan = frozen.plan();
                fallback::Stage::of(&ctx).plan(&plan, &frozen.notices).await;
                (plan.clone(), Resume::Fresh(plan))
            }
            (None, None) => {
                let plan = Plan::single(Route {
                    provider:         "acp".into(),
                    model:            config.model.clone().unwrap_or_default(),
                    reasoning_effort: None,
                    speed:            None,
                });
                (plan.clone(), Resume::Fresh(plan))
            }
        };
        let open = Box::pin(Session::open(&config, &mut ctx, resume));
        let mut session = match open.await {
            Ok(session) => session,
            Err(AgentError::Cancelled) => return Outcome::cancelled(),
            Err(AgentError::Failed { class, message }) => return fail(message, &class),
            Err(AgentError::Model(failure)) => return fail(failure.to_string(), &failure.class()),
        };
        let mut turns = 0;
        let result = Box::pin(run_session(
            &config,
            resolved.fidelity,
            &run_id,
            &contract,
            &mut ctx,
            &mut session,
            &mut turns,
        ))
        .await;
        // The route the plan reached, after any failover Pebble ran.
        let plan = session.plan().unwrap_or(plan);
        let reason = match &result {
            Ok(_) => ShutdownReason::Completed,
            Err(AgentError::Cancelled) => ShutdownReason::Cancelled,
            Err(_) => ShutdownReason::Error,
        };
        // The conversation outlives the node only through its export, taken
        // before the agent shuts down; a failed node discards it.
        if config.backend == AgentBackend::Api
            && resolved.fidelity == Fidelity::Full
            && let (Some(thread), Some(sessions)) = (&resolved.thread, &sessions)
        {
            match (&result, session.export()) {
                (Ok(_), Some(export)) => {
                    let uses = sessions.uses(thread) + 1;
                    sessions.retain(thread, Retained {
                        export,
                        node: config.node.clone(),
                        selector: plan.original().selector(),
                        uses,
                        plan: plan.clone(),
                    });
                }
                _ => sessions.mark_lost(thread),
            }
        }
        let shutdown = session.shutdown(reason, ctx.env.grace()).await;
        let result = match result {
            Ok(stage) => shutdown.map(|()| stage),
            Err(error) => Err(error),
        };
        let mut outcome = match result {
            Ok(mut stage) => {
                if let Some(store) = ctx.capability::<OutputStore>() {
                    blobs::offload_updates(&mut stage.context_updates, store.0.as_ref()).await;
                }
                stage
                    .with_routing(routes.clone(), kv.clone())
                    .with_retries(on_retries_exhausted, final_attempt)
                    .into_outcome(&config.node)
            }
            Err(AgentError::Cancelled) => Outcome::cancelled(),
            Err(AgentError::Failed { class, message }) => fail(message, &class),
            Err(AgentError::Model(failure)) => fail(failure.to_string(), &failure.class()),
        };
        let custom = session.metrics(turns);
        outcome.metrics = Metrics {
            duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            custom,
            ..Metrics::default()
        };
        outcome
    }
}

async fn run_session(
    config: &AgentConfig,
    fidelity: Fidelity,
    run_id: &str,
    contract: &Contract,
    ctx: &mut StepCtx,
    session: &mut Session,
    turn_count: &mut u64,
) -> Result<Stage, AgentError> {
    let mut prompt = config.assemble(fidelity, run_id, contract);
    let mut repairs = 0_u64;
    // Each turn is marked live for the control service while it runs, so a
    // host's interrupt finds it; a driver built outside the coordinator has
    // no identity, and a host without the capability never interrupts.
    let turns = ctx.capability::<LiveTurns>();
    let execution = ctx
        .capability::<ExecutionIdentity>()
        .map(|identity| identity.execution);
    let (outcome, text) = loop {
        // Every turn, the repair turns included, runs on the same plan: a
        // provider-local failure moves the conversation to the next route,
        // inside Pebble, and the session reports the move.
        let live = match (&turns, execution) {
            (Some(turns), Some(execution)) => Some(turns.begin(execution, ctx.firing)),
            _ => None,
        };
        let text = session
            .prompt(
                &prompt,
                &mut ctx.control,
                ctx.env.grace(),
                config.timeout_ms.map(Duration::from_millis),
            )
            .await;
        drop(live);
        let text = text?;
        ctx.log(LogStream::Stdout, text.clone()).await;
        *turn_count += 1;
        match validate(contract, &text) {
            Ok(parsed) => break (parsed, text),
            Err(problem) if repairs < config.output_retries => {
                repairs += 1;
                ctx.log(
                        LogStream::Stderr,
                        format!("the response does not meet the output contract ({problem}); repair turn {repairs}"),
                    )
                    .await;
                prompt = repair_message(&problem);
            }
            Err(problem) => {
                return Err(AgentError::failed(
                    "bad_output",
                    format!(
                        "the response did not meet the output contract after {repairs} repair turn(s): {problem}"
                    ),
                ));
            }
        }
    };

    let mut stage = Stage::new(StageOutcome::Succeeded, config.on_failure);
    stage.output.insert("text".into(), json!(text));
    stage.output.insert("turns".into(), json!(turn_count));
    stage.context_updates.insert(
        SmolStr::new(format!("response.{}", config.node)),
        json!(text),
    );
    // Fabro keeps the first 200 characters in `last_response`.
    let excerpt: String = text.chars().take(200).collect();
    stage
        .context_updates
        .insert(SmolStr::new("last_response"), json!(excerpt));
    stage
        .context_updates
        .insert(SmolStr::new("last_stage"), json!(config.node));
    match outcome {
        Parsed::Directive(directive) => directive.apply_to(&mut stage),
        // Fabro's schema contracts promise routing-kind context_updates
        // semantics: a top-level `context_updates` object inside the
        // validated response reaches the run context exactly like a
        // routing directive's does, so later stages can read the keys
        // through `stdin_source`/kv. Without this spread, a schema'd
        // stage's updates stay buried under `output.<node>` and every
        // downstream reader resolves them as missing.
        Parsed::Structured(value) => {
            stage.output.insert("structured".into(), value.clone());
            stage
                .context_updates
                .insert(SmolStr::new(format!("output.{}", config.node)), value.clone());
            if let Some(updates) = value.get("context_updates").and_then(Value::as_object) {
                for (key, item) in updates {
                    stage.context_updates.insert(SmolStr::new(key.clone()), item.clone());
                }
            }
            // The routing contract survives the schema contract: routing fields
            // the directive kind applies, a schema response carries too (Fabro's
            // schema files document "keeps the routing contract" — required routing
            // fields, label enums). Without this extraction every conditional edge
            // on preferred_label falls through to the catch-all under a schema.
            if let Some(label) = value.get("preferred_next_label").and_then(Value::as_str) {
                stage.output.insert("preferred_label".into(), json!(label));
            }
            if let Some(ids) = value.get("suggested_next_ids") {
                stage.output.insert("suggested_next_ids".into(), ids.clone());
            }
        }
        Parsed::Plain => {}
    }
    Ok(stage)
}
