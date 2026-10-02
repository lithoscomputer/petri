//! A tiny host for the pure core: it turns commands into scripted step results
//! and feeds them back, so tests read as "run this graph and see what
//! happened".

#![allow(
    dead_code,
    reason = "each test binary compiles the whole module, and no one test uses every helper"
)]

use std::collections::BTreeMap;
use std::mem;
use std::time::Duration;

use engine::{
    Admission, Command, EngineExit, EngineState, Event, GroupDecision, RouteDecision, WeightedDraw,
    apply,
};
use ir::{
    Attempt, FiringId, Generation, Graph, NodeId, Outcome, RunStatus, StepKind, StepKindId,
    StepKinds, Token, Value,
};

/// What the host is being asked to run.
#[derive(Clone, Debug)]
pub(crate) struct StartInfo {
    pub firing:     FiringId,
    pub node:       NodeId,
    /// Node name, including the `#index` suffix on expansion clones.
    pub name:       String,
    /// Node name with any `#index` suffix removed.
    pub base:       String,
    /// Clone index, when this node came out of an expansion.
    pub index:      Option<u32>,
    pub generation: Generation,
    /// Which try this is, 1-based.
    pub attempt:    Attempt,
    pub config:     Value,
    pub inputs:     Vec<Token>,
}

impl StartInfo {
    /// The payload of the first input token.
    pub(crate) fn input(&self) -> Value {
        self.inputs
            .first()
            .map_or(Value::Null, |t| t.payload.clone())
    }
}

/// The single step kind the tests use. Nodes differ by config, not by kind.
pub(crate) struct Noop;

impl StepKind for Noop {
    fn id(&self) -> StepKindId {
        NOOP
    }
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "noop"
    }
}

/// The load-time lookup `validate_with` takes, over the one test kind.
pub(crate) struct Kinds(Vec<Box<dyn StepKind>>);

impl StepKinds for Kinds {
    fn get(&self, id: &StepKindId) -> Option<&dyn StepKind> {
        self.0.iter().find(|k| k.id() == *id).map(AsRef::as_ref)
    }
}

pub(crate) fn registry() -> Kinds {
    Kinds(vec![Box::new(Noop)])
}

pub(crate) const NOOP: StepKindId = StepKindId::new_static("noop");

type Responder = Box<dyn FnMut(&StartInfo) -> Outcome>;
type Observer = Box<dyn FnMut(&EngineState, &[Command])>;

pub(crate) struct Harness {
    pub state:             EngineState,
    /// The graph the run started from, before any splice. Replay needs this
    /// one.
    pub original_graph:    Graph,
    responder:             Responder,
    /// Every command the core produced, in order.
    pub commands:          Vec<Command>,
    /// Names of the nodes the host was told to start, in order.
    pub started:           Vec<String>,
    /// The most steps that were running at the same time.
    pub max_concurrent:    usize,
    /// Every `ScheduleRetry` the core issued, in order.
    pub scheduled_retries: Vec<(FiringId, Attempt, Duration)>,
    pub status:            Option<RunStatus>,
    /// Called after every `apply` with the new state and the commands it
    /// produced.
    observer:              Option<Observer>,
}

impl Harness {
    pub(crate) fn new(graph: Graph) -> Self {
        Self {
            original_graph:    graph.clone(),
            state:             EngineState::new(graph),
            responder:         Box::new(|_| Outcome::success(Value::Null)),
            commands:          Vec::new(),
            started:           Vec::new(),
            max_concurrent:    0,
            scheduled_retries: Vec::new(),
            status:            None,
            observer:          None,
        }
    }

    /// Watch the run as it goes: every `apply`'s new state and the commands
    /// it produced.
    pub(crate) fn observe(mut self, f: impl FnMut(&EngineState, &[Command]) + 'static) -> Self {
        self.observer = Some(Box::new(f));
        self
    }

    /// Decide each step's result from the request.
    pub(crate) fn respond_with(mut self, f: impl FnMut(&StartInfo) -> Outcome + 'static) -> Self {
        self.responder = Box::new(f);
        self
    }

    /// Fixed results per node base name; anything unlisted succeeds with
    /// `null`.
    pub(crate) fn results(self, results: BTreeMap<&'static str, Outcome>) -> Self {
        let table: BTreeMap<String, Outcome> = results
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        self.respond_with(move |info| {
            table
                .get(&info.base)
                .cloned()
                .unwrap_or_else(|| Outcome::success(Value::Null))
        })
    }

    /// Start the run and pump until the core reports it finished.
    pub(crate) fn run(&mut self) -> RunStatus {
        self.feed(Event::ExecutionStarted {
            start: engine::EngineStart::default(),
        });
        // Steps are held until the whole batch is issued, so `max_concurrent`
        // reflects what the core allowed to run at once, not the order the host
        // happened to reply in.
        loop {
            let pending: Vec<Command> = self
                .commands
                .iter()
                .filter(|c| matches!(c, Command::StartStep(_)))
                .cloned()
                .collect();
            self.commands
                .retain(|c| !matches!(c, Command::StartStep { .. }));
            if pending.is_empty() {
                break;
            }
            self.max_concurrent = self.max_concurrent.max(pending.len());
            for command in pending {
                let Command::StartStep(resolved) = command else {
                    continue;
                };
                let name = self
                    .state
                    .graph()
                    .node(resolved.node())
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                let (base, index) = split_clone_name(&name);
                let firing = resolved.id();
                let attempt = resolved.attempt();
                let info = StartInfo {
                    firing,
                    node: resolved.node(),
                    name: name.clone(),
                    base,
                    index,
                    generation: resolved.generation(),
                    attempt,
                    config: resolved.config().clone(),
                    inputs: resolved.inputs().to_vec(),
                };
                self.started.push(name);
                let outcome = (self.responder)(&info);
                self.feed(Event::StepStarted { firing, attempt });
                self.finish_attempt(firing, attempt, outcome);
                // A retry keeps the firing live; walk the backoff without a clock.
                self.drain_retries();
                if let Some(status) = self.status {
                    return status;
                }
            }
        }
        self.status.unwrap_or_else(|| self.state.folded_status())
    }

    /// Answer every outstanding `ScheduleRetry` at once. The driver would sleep
    /// and add jitter; a test just feeds the event straight back.
    pub(crate) fn drain_retries(&mut self) {
        loop {
            let pending: Vec<(FiringId, Attempt, Duration)> = self
                .commands
                .iter()
                .filter_map(|c| match c {
                    Command::ScheduleRetry {
                        firing,
                        next_attempt,
                        base_delay,
                    } => Some((*firing, *next_attempt, *base_delay)),
                    _ => None,
                })
                .collect();
            if pending.is_empty() {
                return;
            }
            self.commands
                .retain(|c| !matches!(c, Command::ScheduleRetry { .. }));
            self.scheduled_retries.extend(pending.iter().copied());
            for (firing, next_attempt, _) in pending {
                self.feed(Event::RetryElapsed {
                    firing,
                    next_attempt,
                });
            }
        }
    }

    /// Push one event through the core, keeping the commands it produced.
    pub(crate) fn feed(&mut self, event: Event) {
        let state = mem::replace(&mut self.state, EngineState::new(Graph::new()));
        let (state, commands) = apply(state, event);
        self.state = state;
        if let Some(observer) = self.observer.as_mut() {
            observer(&self.state, &commands);
        }
        for command in &commands {
            if let Command::FinishExecution {
                exit: EngineExit::Terminal { status },
            } = command
            {
                self.status = Some(*status);
            }
        }
        self.commands.extend(commands.iter().cloned());
        for command in commands {
            match &command {
                Command::Admit { decision_id } => {
                    let event = Event::AdmissionDecided {
                        decision_id: *decision_id,
                        decision:    Admission::Admit,
                        trace:       Vec::new(),
                    };
                    self.feed(event);
                }
                Command::ResolveRouting {
                    decision_id,
                    restart_allowed,
                    groups,
                } => {
                    let decisions = groups
                        .iter()
                        .map(|proposal| {
                            let (decision, draw) = resolve_group(proposal);
                            let decision =
                                engine::enforce_restart_limit(*restart_allowed, proposal, decision);
                            GroupDecision {
                                group: proposal.group,
                                draw,
                                trace: Vec::new(),
                                decision,
                            }
                        })
                        .collect();
                    let event = Event::RoutingResolved {
                        decision_id: *decision_id,
                        groups:      decisions,
                    };
                    self.feed(event);
                }
                _ => {}
            }
        }
    }

    /// Cancel a scope mid-run, then keep pumping.
    pub(crate) fn cancel(&mut self, scope: ir::CancelScopeId) {
        self.feed(Event::cancel_scope(scope));
    }

    /// Pull the `StartStep` commands issued so far, as `(firing, node name)`.
    /// Used by tests that drive the core one event at a time.
    pub(crate) fn take_starts(&mut self) -> Vec<(FiringId, String)> {
        let starts: Vec<(FiringId, String)> = self
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::StartStep(resolved) => Some((
                    resolved.id(),
                    self.state
                        .graph()
                        .node(resolved.node())
                        .map(|n| n.name.to_string())
                        .unwrap_or_default(),
                )),
                _ => None,
            })
            .collect();
        self.commands
            .retain(|c| !matches!(c, Command::StartStep { .. }));
        for (_, name) in &starts {
            self.started.push(name.clone());
        }
        starts
    }

    /// Report a step's result to the core, for the attempt it is running.
    pub(crate) fn finish(&mut self, firing: FiringId, outcome: Outcome) {
        let attempt = self
            .state
            .firing(firing)
            .map_or(Attempt::FIRST, |f| f.attempt);
        self.feed(Event::StepStarted { firing, attempt });
        self.finish_attempt(firing, attempt, outcome);
    }

    /// Feed a finished attempt as the driver does: the node's exhaustion
    /// policy applies before the record, and the core records what it is
    /// given.
    fn finish_attempt(&mut self, firing: FiringId, attempt: Attempt, outcome: Outcome) {
        let outcome = match self
            .state
            .firing(firing)
            .and_then(|live| self.state.graph().node(live.node))
        {
            Some(node) => node.retry.finalize(attempt, outcome),
            None => outcome,
        };
        self.feed(Event::StepFinished {
            firing,
            attempt,
            outcome,
        });
    }

    /// Replay the log from a fresh state and check it comes back
    /// byte-identical.
    ///
    /// The determinism canary: if any core decision depended on a clock, on
    /// iteration order, or on anything outside the state, the logs diverge.
    pub(crate) fn verify_replay(&self) {
        if let Err(mismatch) = engine::verify_replay(self.original_graph.clone(), &self.state.log) {
            panic!("replay was not byte-identical: {mismatch}");
        }
    }

    pub(crate) fn output(&self, node: &str) -> Value {
        self.state.output(node).cloned().unwrap_or(Value::Null)
    }

    /// How many times a node base name was started.
    pub(crate) fn start_count(&self, base: &str) -> usize {
        self.started
            .iter()
            .filter(|n| split_clone_name(n).0 == base)
            .count()
    }

    pub(crate) fn statuses(&self) -> Vec<(String, String)> {
        self.state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag().to_string()))
            .collect()
    }

    pub(crate) fn status_of(&self, name: &str) -> Option<String> {
        self.state
            .history()
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.outcome.status.tag().to_string())
    }

    pub(crate) fn commands_of<T>(&self, f: impl Fn(&Command) -> Option<T>) -> Vec<T> {
        self.commands.iter().filter_map(f).collect()
    }
}

/// The engine's own deterministic pick, with a fixed roll of zero for weighted
/// tiers — the host would roll randomly; a test walks the same cursor with a
/// known draw.
fn resolve_group(proposal: &engine::RoutingProposal) -> (RouteDecision, Option<WeightedDraw>) {
    let draw = (proposal.pick == Some(ir::PickPolicy::WeightedRandom)
        && !proposal.candidates.is_empty())
    .then(|| WeightedDraw {
        tier:       proposal.tier.unwrap_or(0),
        candidates: proposal
            .candidates
            .iter()
            .map(|candidate| candidate.edge)
            .collect(),
        roll:       0,
        total:      proposal
            .candidates
            .iter()
            .map(|candidate| u64::from(candidate.weight))
            .sum(),
    });
    let picked = engine::deterministic_pick(proposal, draw.as_ref())
        .expect("the harness draw matches its proposal");
    (
        picked.map_or(RouteDecision::None, RouteDecision::Emit),
        draw,
    )
}

/// Stand-in for the process step kind's `soft_fail` handling.
///
/// Producing `PartialSuccess` is a step-kind decision, not a core one: there is
/// no node-level policy. This is the shape BuildKite's `soft_fail` and GHA's
/// `continue-on-error` both lower onto.
pub(crate) fn process_outcome(config: &Value, exit_code: i32) -> Outcome {
    if exit_code == 0 {
        return Outcome::success(serde_json::json!({ "exit_code": 0 }));
    }
    let failure = ir::FailureInfo::exit_status(exit_code);
    let soft = match config.get("soft_fail") {
        Some(Value::Bool(true)) => true,
        Some(Value::Array(codes)) => codes
            .iter()
            .any(|c| c.as_i64() == Some(i64::from(exit_code))),
        _ => false,
    };
    let output = serde_json::json!({ "exit_code": exit_code });
    if soft {
        // The real failure rides along in `underlying`, so the log never records a
        // clean success for something that failed.
        Outcome::partial(failure, output)
    } else {
        Outcome::new(ir::Status::Failure(failure), output)
    }
}

fn split_clone_name(name: &str) -> (String, Option<u32>) {
    match name.rsplit_once('#') {
        Some((base, index)) => match index.parse::<u32>() {
            Ok(index) => (base.to_string(), Some(index)),
            Err(_) => (name.to_string(), None),
        },
        None => (name.to_string(), None),
    }
}
