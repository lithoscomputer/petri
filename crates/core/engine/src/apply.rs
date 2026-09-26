//! The state machine itself.
//!
//! `apply` is deterministic: no IO, no clocks, no randomness. Timeouts and step
//! results arrive as events. Routing emits tokens by feeding events back to
//! itself through an internal queue, and every one of those is logged before it
//! is applied, so the log is a complete, replayable account of the run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem;

use ir::{
    Attempt, CancelScopeId, Control, EdgeTransition, EvalEnv, ExpandTarget, Expansion,
    FailureClass, FailureInfo, FiringId, Generation, Guard, JoinPolicy, Node, NodeId, Outcome,
    PickPolicy, SelectionPolicy, Status, Token, Value, eval, eval_bool,
};

/// The failure class of a record whose firing environment could not be built —
/// a scope-env or binding expression errored before the step could exist. The
/// record's message carries the cause; the engine's error list carries it too.
pub const FIRING_ENV_CLASS: FailureClass = FailureClass::new_static("firing_env");
use smol_str::SmolStr;

use crate::context::{clone_bindings, firing_statics, primary_token, resolve_config, with_outcome};
use crate::event::{
    Admission, CancelTarget, Command, DecisionId, EngineExit, EngineStart, EntryPoint, Event,
    GroupDecision, Intervention, ResolvedFiring, RouteApplied, RouteDecision, RoutingCandidate,
    RoutingProposal, SpliceClone, SubgraphSplice, WeightedDraw,
};
use crate::log::EventOrigin;
use crate::splice::{
    PreparedSeed, PreparedSplice, apply_prepared_splice, commit_splice_plan,
    prepare_outcome_splices, reject_splices, remap_selection_policy,
};
use crate::state::{
    BatchPolicy, EngineState, Firing, FiringRecord, PendingAdmission, PendingRouting,
    PreparedRoute, RestartIntent, RunError, SpliceEffect, SpliceOrigin, synthetic,
};

const ADMISSION_BLOCKED_CLASS: FailureClass = FailureClass::new_static("admission_blocked");

/// Apply one event and return the commands it produced.
///
/// The event may cause the core to emit further events of its own (tokens,
/// splices, cascading cancellations). Those are drained inside this call, so
/// the caller only ever feeds in events that came from outside.
pub fn apply(mut state: EngineState, ev: Event) -> (EngineState, Vec<Command>) {
    let mut commands = Vec::new();
    let mut queue: VecDeque<Event> = VecDeque::from([ev]);
    // The first event came from outside; everything the drain adds is the core's
    // own.
    let mut origin = EventOrigin::External;

    loop {
        while let Some(event) = queue.pop_front() {
            state.log.append(origin, event.clone());
            origin = EventOrigin::Core;
            step(&mut state, event, &mut commands, &mut queue);
        }
        // Deferred joins are re-checked only once the queue is fully drained, so an
        // admission decision never runs ahead of an event already in flight — a
        // fail_fast cancel queued by a clone's failure must land before that
        // clone's deferred siblings are considered.
        admit_deferred(&mut state, &mut commands, &mut queue);
        if queue.is_empty() {
            break;
        }
    }

    for scope in state.release_unneeded_scopes() {
        commands.push(Command::ReleaseScope { scope });
    }
    finish_if_quiescent(&mut state, &mut commands);
    (state, commands)
}

fn step(
    state: &mut EngineState,
    event: Event,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    if state.is_finished() {
        state.push_error(RunError::AlreadyFinished);
        return;
    }
    if !state.is_started() && !matches!(event, Event::ExecutionStarted { .. }) {
        state.push_error(RunError::NotStarted);
        return;
    }

    match event {
        Event::ExecutionStarted { start } => on_execution_started(state, start, cmds),
        Event::TokenEmitted { token } => on_token(state, token, cmds, queue),
        Event::StepStarted { firing, attempt } => match state.firing_mut(firing) {
            Some(f) if f.attempt == attempt => f.started = true,
            Some(f) => {
                let running = f.attempt;
                state.push_error(RunError::AttemptMismatch {
                    firing,
                    reported: attempt,
                    running,
                });
            }
            None => state.push_error(RunError::UnknownFiring(firing)),
        },
        // Progress is observation only: logs and artifacts carry no coordination
        // meaning, so the core records them and changes nothing. A scope's
        // acquisition is the same: the driver records where the scope runs,
        // and the core routes on what its firings report.
        Event::StepProgressRecorded { .. }
        | Event::ScopeAcquired { .. }
        | Event::ScopeFailed { .. } => {}
        Event::StepFinished {
            firing,
            attempt,
            outcome,
        } => on_step_finished(state, firing, attempt, outcome, cmds, queue),
        Event::AdmissionDecided {
            decision_id,
            decision,
            trace: _,
        } => on_admitted(state, decision_id, decision, cmds, queue),
        Event::RoutingResolved {
            decision_id,
            groups,
        } => on_routing_resolved(state, decision_id, &groups, queue),
        Event::RouteApplied { applied } => on_route_applied(state, applied, cmds, queue),
        Event::RetryElapsed {
            firing,
            next_attempt,
        } => on_retry_elapsed(state, firing, next_attempt, cmds),
        Event::NodeExpanded { node, splice } => on_node_expanded(state, node, splice, queue),
        Event::CancelRequested {
            target: CancelTarget::Scope(scope),
        } => on_cancel(state, scope, cmds),
        Event::CancelRequested {
            target: CancelTarget::Group(node),
        } => {
            if state
                .graph
                .node(node)
                .is_some_and(|node| node.cancel_group.is_some())
            {
                on_cancel(state, state.cancel_scope_of(node), cmds);
            }
        }
        Event::KillRequested { scope } => on_kill(state, scope, cmds),
        Event::ControlRequested { firing, ctl } => on_control_requested(state, firing, ctl, cmds),
    }
}

// ── Run start ─────────────────────────────────────────────────────────────

fn on_execution_started(state: &mut EngineState, start: EngineStart, cmds: &mut Vec<Command>) {
    if start.max_executions == 0 {
        state.push_error(RunError::ZeroExecutionLimit);
    }
    if let EntryPoint::Node(node) = start.entry
        && state.graph.node(node).is_none()
    {
        state.push_error(RunError::UnknownEntryNode(node));
    }
    state.mark_started(start);
    let decision_id = DecisionId::ExecutionStart;
    state.insert_pending_admission(decision_id, PendingAdmission { resolved: None });
    cmds.push(Command::Admit { decision_id });
}

fn seed_execution(state: &mut EngineState, queue: &mut VecDeque<Event>) {
    let Some(start) = state.start().cloned() else {
        return;
    };
    // Entry nodes have no incoming edges, so each gets a synthetic one. Their join
    // then works like any other node's, with no special case in the firing rule.
    let entries = match start.entry {
        EntryPoint::GraphEntries => state.graph.entry.clone(),
        EntryPoint::Node(node) => {
            state.force_entry(node, Generation::ZERO);
            vec![node]
        }
    };
    for entry in entries {
        if state.graph.node(entry).is_none() {
            state.push_error(RunError::UnknownNode(entry));
            continue;
        }
        let edge = state.next_edge_id();
        state.register_seed_edge(edge, entry);
        queue.push_back(Event::TokenEmitted {
            token: Token::seeded(edge, Generation::ZERO, Value::Null),
        });
    }
}

fn on_admitted(
    state: &mut EngineState,
    decision_id: DecisionId,
    decision: Admission,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let Some(pending) = state.take_pending_admission(decision_id) else {
        state.push_error(RunError::UnknownDecision(decision_id));
        return;
    };
    match (decision_id, decision, pending.resolved) {
        (DecisionId::ExecutionStart, Admission::Admit, None) => seed_execution(state, queue),
        (DecisionId::ExecutionStart, Admission::Block { reason }, None) => {
            state.push_error(RunError::AdmissionBlocked {
                decision: decision_id,
                reason,
            });
        }
        (DecisionId::AttemptStart { firing, attempt }, decision, Some(resolved))
            if resolved.id() == firing && resolved.attempt() == attempt =>
        {
            apply_attempt_admission(state, resolved, decision, cmds);
        }
        // `Skip` at execution start, a missing or mismatched payload, a `Route`
        // id: none of these has a coherent admission to apply.
        _ => state.push_error(RunError::DecisionMismatch {
            decision: decision_id,
        }),
    }
}

fn apply_attempt_admission(
    state: &mut EngineState,
    resolved: ResolvedFiring,
    decision: Admission,
    cmds: &mut Vec<Command>,
) {
    let firing_id = resolved.id();
    let Some(firing) = state.firing(firing_id).cloned() else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };
    let outcome = match decision {
        Admission::Admit => {
            if state.acquire_scope(node.scope) {
                cmds.push(Command::AcquireScope { scope: node.scope });
            }
            cmds.push(Command::StartStep(resolved));
            return;
        }
        Admission::Skip { outcome } => outcome,
        Admission::Block { reason } => {
            state.push_error(RunError::AdmissionBlocked {
                decision: DecisionId::AttemptStart {
                    firing:  firing_id,
                    attempt: firing.attempt,
                },
                reason:   reason.clone(),
            });
            Outcome::new(
                Status::Failure(FailureInfo::new(reason).with_class(ADMISSION_BLOCKED_CLASS)),
                Value::Null,
            )
        }
    };
    state.remove_firing(firing_id);
    state.record_outcome(FiringRecord {
        firing:     firing_id,
        node:       firing.node,
        name:       node.name.clone(),
        generation: firing.generation,
        attempt:    firing.attempt,
        outcome:    outcome.clone(),
    });
    route(
        state,
        &node,
        firing_id,
        firing.generation,
        firing.attempt,
        &firing.inputs,
        &outcome,
        cmds,
    );
}

// ── Tokens and the firing rule ────────────────────────────────────────────

fn on_token(
    state: &mut EngineState,
    token: Token,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let Some(target) = state.edge_target(token.edge) else {
        state.push_error(RunError::UnknownEdge(token.edge));
        return;
    };
    if state.graph.node(target).is_none() {
        state.push_error(RunError::UnknownNode(target));
        return;
    }
    // A killed scope swallows tokens aimed inside it. A merely cancelled scope does
    // not: its nodes receive their tokens and complete without executing — or fire,
    // when marked `run_on_cancel` — so `always()` and `cancelled()` cleanup can
    // run.
    if state.is_node_killed(target) {
        return;
    }
    // A retracted admission swallows its tokens by exact identity: the key never
    // fires, and the node's other generations are untouched.
    if state.is_admission_retracted(target, token.generation) {
        return;
    }
    let key = (target, token.generation);
    // `Any` fires on the first token; later same-generation tokens are dropped.
    // The same rule stops a satisfied `All` from firing twice.
    if state.has_fired(key) {
        return;
    }
    state.store_token(target, token);
    try_fire(state, key, cmds, queue);
}

/// Check one `(node, generation)` and fire it if its join is satisfied.
fn try_fire(
    state: &mut EngineState,
    key: (NodeId, Generation),
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let (node_id, generation) = key;
    if state.has_fired(key) || state.is_superseded(node_id) {
        return;
    }
    let Some(node) = state.graph.node(node_id).cloned() else {
        state.push_error(RunError::UnknownNode(node_id));
        return;
    };
    if !is_join_satisfied(state, &node, key) {
        return;
    }

    // Nothing in a killed scope fires, ever. Its tokens were dropped and late ones
    // are swallowed before they get here; this covers whatever slipped in between.
    if state.is_node_killed(node_id) {
        return;
    }

    // §5: in a cancelled scope — or fed by cancelled work — only a node marked
    // `run_on_cancel` may actually run; everything else, expansions included,
    // completes `Cancelled` without evaluating anything, and routing still carries
    // the outcome onward. The input check is what keeps a `fail_fast` splice's
    // un-marked collector, which sits outside the cancelled scope, from starting.
    let cancelled = state.is_node_cancelled(node_id) || has_cancelled_input(state, key);
    let admitted = !cancelled || (node.run_on_cancel && node.expand.is_none());

    // Admission control: a splice with `max_parallel` holds surplus clones back
    // until a sibling finishes. The tokens stay pending, so nothing is lost. A
    // node completing without running takes no slot, so it is not held back.
    if admitted
        && let Some(splice) = state.splice_for_node(node_id)
        && let Some(max) = splice.policy.max_parallel
        && splice.live_count() >= max.max(1)
    {
        state.defer(key);
        return;
    }

    // The budget bounds synthesized outcomes too: an `Always`-guarded back edge
    // cycling through a cancelled region terminates here, exactly as a `Skipped`
    // cascade does.
    if state.firing_count(node_id) >= node.budget.max_firings {
        state.push_error(RunError::BudgetExceeded {
            node:        node_id,
            max_firings: node.budget.max_firings,
        });
        state.take_tokens(key);
        state.mark_fired(key);
        return;
    }

    let inputs = state.take_tokens(key);
    state.mark_fired(key);
    state.take_forced_entry(key);
    state.bump_firing_count(node_id);

    if !admitted {
        complete_without_running(
            state,
            &node,
            generation,
            &inputs,
            &synthetic(Status::Cancelled),
            cmds,
        );
        return;
    }

    if node.expand.is_some() {
        expand(state, &node, generation, &inputs, queue);
        return;
    }

    let statics = match firing_statics(state, node_id, &inputs, generation, Attempt::FIRST) {
        Ok(statics) => statics,
        Err(error) => {
            // The record carries the cause and a class: without them, every
            // report over the log reads "could not build the firing
            // environment" and cannot say which expression broke.
            let outcome = Outcome::new(
                Status::Failure(
                    FailureInfo::new(format!("could not build the firing environment: {error}"))
                        .with_class(FIRING_ENV_CLASS),
                ),
                Value::Null,
            );
            state.push_error(error);
            complete_without_running(state, &node, generation, &inputs, &outcome, cmds);
            return;
        }
    };
    let token = primary_token(&inputs);

    // A precondition that is false skips the node, but routing still runs, so
    // `always()` and `failure()` guards downstream still see it. In a cancelled
    // scope the record says `Cancelled`, not `Skipped`: why the node did not run.
    // An evaluation error keeps its ordinary behavior either way — cancellation
    // must not convert a broken expression into a clean cancellation.
    if let Some(precondition) = node.precondition {
        let env = EvalEnv::new(&token, state.run_context(), &statics);
        match eval_bool(&state.graph.exprs, precondition, &env) {
            Ok(true) => {}
            Ok(false) => {
                let status = if cancelled {
                    Status::Cancelled
                } else {
                    Status::Skipped
                };
                complete_without_running(
                    state,
                    &node,
                    generation,
                    &inputs,
                    &synthetic(status),
                    cmds,
                );
                return;
            }
            Err(error) => {
                state.push_error(RunError::Eval {
                    node: node_id,
                    site: SmolStr::new("precondition"),
                    error,
                });
                complete_without_running(
                    state,
                    &node,
                    generation,
                    &inputs,
                    &Outcome::failure("precondition failed to evaluate"),
                    cmds,
                );
                return;
            }
        }
    }

    let config = match resolve_config(
        &node.step.config,
        &state.graph.exprs,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
        Ok(config) => config,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: node_id,
                site: SmolStr::new("step config"),
                error,
            });
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                &Outcome::failure("step config failed to resolve"),
                cmds,
            );
            return;
        }
    };

    let firing_id = state.next_firing_id();
    // The constructor is where "no unresolved ExprId crosses the executor boundary"
    // is enforced. A malformed placeholder that `resolve_config` could not read is
    // caught here rather than reaching a step.
    let resolved = match ResolvedFiring::new(
        firing_id,
        node_id,
        generation,
        Attempt::FIRST,
        node.scope,
        inputs.clone(),
        config,
    ) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            state.push_error(RunError::UnresolvedConfig {
                node: node_id,
                path: unresolved.path,
            });
            complete_without_running(
                state,
                &node,
                generation,
                &inputs,
                &Outcome::failure("step config still holds an unresolved expression"),
                cmds,
            );
            return;
        }
    };

    let firing = Firing {
        id: firing_id,
        node: node_id,
        generation,
        attempt: Attempt::FIRST,
        scope: node.scope,
        cancel_scope: state.cancel_scope_of(node_id),
        inputs,
        started: false,
        cancelling: false,
        awaiting_retry: false,
    };
    state.insert_firing(firing);
    let decision_id = DecisionId::attempt_start(firing_id, Attempt::FIRST);
    state.insert_pending_admission(decision_id, PendingAdmission {
        resolved: Some(resolved),
    });
    cmds.push(Command::Admit { decision_id });
}

/// Whether any token waiting at this join was emitted by a firing whose
/// recorded outcome is `Cancelled`. Work fed by cancelled work is admitted the
/// same way as work inside a cancelled scope: only via `run_on_cancel`.
fn has_cancelled_input(state: &EngineState, key: (NodeId, Generation)) -> bool {
    let Some(tokens) = state.tokens_for(key) else {
        return false;
    };
    tokens.values().any(|t| state.outcome_was_cancelled(t.from))
}

fn is_join_satisfied(state: &EngineState, node: &Node, key: (NodeId, Generation)) -> bool {
    if state.is_forced_entry(key) {
        return true;
    }
    let Some(tokens) = state.tokens_for(key) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    // Incoming edges are counted as of now, so edges a splice added are included.
    let incoming = state.incoming_edges(node.id);
    match node.join {
        JoinPolicy::All => incoming.iter().all(|edge| tokens.contains_key(edge)),
        JoinPolicy::Any => true,
        JoinPolicy::Quorum { n } => tokens.len() >= n.max(1) as usize,
    }
}

/// Record an outcome for a node that never executed, then route it.
fn complete_without_running(
    state: &mut EngineState,
    node: &Node,
    generation: Generation,
    inputs: &[Token],
    outcome: &Outcome,
    cmds: &mut Vec<Command>,
) {
    let firing = state.next_firing_id();
    state.record_outcome(FiringRecord {
        firing,
        node: node.id,
        name: node.name.clone(),
        generation,
        attempt: Attempt::FIRST,
        outcome: outcome.clone(),
    });
    route(
        state,
        node,
        firing,
        generation,
        Attempt::FIRST,
        inputs,
        outcome,
        cmds,
    );
}

// ── Step completion ───────────────────────────────────────────────────────

fn on_step_finished(
    state: &mut EngineState,
    firing_id: FiringId,
    attempt: Attempt,
    outcome: Outcome,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let Some(firing) = state.firing(firing_id).cloned() else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    if firing.attempt != attempt {
        state.push_error(RunError::AttemptMismatch {
            firing:   firing_id,
            reported: attempt,
            running:  firing.attempt,
        });
        return;
    }
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };

    let cancelled = firing.cancelling || state.is_node_cancelled(firing.node);

    // Retry decision. A cancelled firing is never retried: the point of cancelling
    // is to stop the work, not to start it again.
    if !cancelled
        && node.retry.should_retry(&outcome.status)
        && node.retry.has_attempt_after(attempt)
    {
        // The firing stays live, so its scope stays held and the run stays
        // non-quiescent while the driver waits out the backoff. Nothing is recorded
        // and nothing is routed: only the final attempt is visible downstream —
        // this attempt's splice requests included, which stay in the log and
        // change nothing.
        schedule_retry(state, cmds, firing_id, &node, attempt);
        return;
    }

    // This attempt is candidate-final. Finalization is prepare-then-commit for
    // its splice requests, in this order:
    //
    // 1. Cancelled-scope check first. A normal firing's splice list is dropped
    //    wholesale — no policy check, no validation, no `invalid_splice`. A
    //    `run_on_cancel` uploader may append only fragments whose every node is
    //    also `run_on_cancel`; this is how a lazy planner admits explicit cleanup
    //    without letting cancellation create ordinary work. Mixed or destructive
    //    request lists are dropped atomically.
    // 2. Prepare every request in order against a scratch view. Preparation never
    //    touches canonical state, so a rejection leaks nothing.
    // 3. On rejection, convert to the canonical `Failure{class: invalid_splice}` —
    //    output and metrics kept, `context_updates` dropped, the message naming the
    //    request index and location, raw requests remaining only in the External
    //    event — and only then run `retry_on` and exhaustion: `invalid_splice` is
    //    an ordinary retry class, and a later attempt can succeed.
    // 4. If all requests prepare, commit consumes the plan below: record the
    //    outcome, apply in order, route the uploader, and quiescence runs at the
    //    end of `apply` as always. Nothing partial ever commits.
    let mut outcome = outcome;
    let mut plan = None;
    if !outcome.splices.is_empty() {
        let requests = mem::take(&mut outcome.splices);
        let cleanup_append = node.run_on_cancel
            && requests.iter().all(|request| {
                matches!(request.mode, ir::SpliceMode::Append)
                    && request.fragment.nodes.iter().all(|node| node.run_on_cancel)
            });
        if !cancelled || cleanup_append {
            match prepare_outcome_splices(
                state,
                &node,
                firing.generation,
                firing.cancel_scope,
                &requests,
            ) {
                Ok(prepared) => plan = Some(prepared),
                Err(error) => {
                    outcome = reject_splices(outcome, error.to_string());
                    // The converted failure gets the ordinary retry decision,
                    // and the node's exhaustion policy when none is left: the
                    // engine made this failure after the driver's hand-off,
                    // so the engine finalizes it.
                    if node.retry.should_retry(&outcome.status)
                        && node.retry.has_attempt_after(attempt)
                    {
                        schedule_retry(state, cmds, firing_id, &node, attempt);
                        return;
                    }
                    outcome = node.retry.finalize(attempt, outcome);
                }
            }
        }
    }

    // This attempt is final, and its outcome is recorded as it arrived: the
    // driver applied the node's exhaustion policy (`RetryPolicy::finalize`)
    // before any host prepared the result, so the record a host prepared is
    // the record kept.
    state.remove_firing(firing_id);

    state.record_outcome(FiringRecord {
        firing: firing_id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        attempt,
        outcome: outcome.clone(),
    });

    // fail_fast: the first clone failure cancels its siblings through the splice's
    // own cancel scope. Keyed off the final attempt, like everything else. A
    // tolerated failure never pulls its siblings down.
    if outcome.status.is_failure()
        && !node.tolerates_failure
        && let Some(splice) = state.splice_for_node(firing.node)
        && splice.policy.fail_fast
        && !state.is_scope_cancelled(splice.cancel_scope)
    {
        let scope = splice.cancel_scope;
        queue.push_back(Event::cancel_scope(scope));
    }

    // A killed firing's outcome is recorded but never routed. A merely cancelled
    // one routes like any other outcome (§5): what stops work from restarting is
    // the structural `run_on_cancel` admission in `try_fire`, not a routing hole.
    // A killed firing prepared no plan: killed implies cancelled, and the
    // cancelled check above dropped its requests.
    if state.is_node_killed(firing.node) {
        return;
    }

    // Commit before routing: splices apply before the uploader routes or the
    // engine tests quiescence, so there is no lost-upload race. The uploader's
    // routing may have gained entry groups, so route with the refreshed node.
    let node = if let Some(plan) = plan {
        commit_splice_plan(state, plan, queue);
        state.graph.node(firing.node).cloned().unwrap_or(node)
    } else {
        node
    };
    route(
        state,
        &node,
        firing_id,
        firing.generation,
        attempt,
        &firing.inputs,
        &outcome,
        cmds,
    );
}

/// Park the firing for a retry: keep it live through the backoff — which holds
/// its scope and keeps the run non-quiescent — and schedule the next attempt.
/// The caller has already decided the retry is admissible.
fn schedule_retry(
    state: &mut EngineState,
    cmds: &mut Vec<Command>,
    firing_id: FiringId,
    node: &Node,
    attempt: Attempt,
) {
    if let Some(f) = state.firing_mut(firing_id) {
        f.park_for_retry();
    }
    cmds.push(Command::ScheduleRetry {
        firing:       firing_id,
        next_attempt: attempt.next(),
        base_delay:   node.retry.base_delay(attempt),
    });
}

/// The backoff elapsed: start the next attempt.
///
/// The config is resolved again, so a retry sees the run context as it stands
/// now rather than as it stood before the first attempt.
fn on_retry_elapsed(
    state: &mut EngineState,
    firing_id: FiringId,
    next_attempt: Attempt,
    cmds: &mut Vec<Command>,
) {
    // A cancel or kill settled this firing while it was waiting out its backoff:
    // the driver's sleeper could not be recalled, so this arrival was expected.
    // The no-op is tombstone-precise; any other invalid `RetryElapsed` — unknown
    // firing, not awaiting, duplicate after the tombstone is consumed — still
    // errors below, so malformed external input stays loud.
    if state.take_retry_tombstone(firing_id) {
        return;
    }
    let Some(firing) = state.firing(firing_id).cloned() else {
        state.push_error(RunError::UnknownFiring(firing_id));
        return;
    };
    if !firing.awaiting_retry {
        state.push_error(RunError::UnexpectedRetry { firing: firing_id });
        return;
    }
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };

    let statics = match firing_statics(
        state,
        firing.node,
        &firing.inputs,
        firing.generation,
        next_attempt,
    ) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, cmds);
            return;
        }
    };
    let token = primary_token(&firing.inputs);
    let config = match resolve_config(
        &node.step.config,
        &state.graph.exprs,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
        Ok(config) => config,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: firing.node,
                site: SmolStr::new("step config"),
                error,
            });
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, cmds);
            return;
        }
    };
    let resolved = match ResolvedFiring::new(
        firing_id,
        firing.node,
        firing.generation,
        next_attempt,
        firing.scope,
        firing.inputs.clone(),
        config,
    ) {
        Ok(resolved) => resolved,
        Err(unresolved) => {
            state.push_error(RunError::UnresolvedConfig {
                node: firing.node,
                path: unresolved.path,
            });
            fail_live_firing(state, firing_id, &node, &firing, next_attempt, cmds);
            return;
        }
    };

    if let Some(f) = state.firing_mut(firing_id) {
        f.attempt = next_attempt;
        f.awaiting_retry = false;
    }
    let decision_id = DecisionId::attempt_start(firing_id, next_attempt);
    state.insert_pending_admission(decision_id, PendingAdmission {
        resolved: Some(resolved),
    });
    cmds.push(Command::Admit { decision_id });
}

/// End a live firing that could not be restarted, recording the failure and
/// routing it like any other final outcome.
fn fail_live_firing(
    state: &mut EngineState,
    firing_id: FiringId,
    node: &Node,
    firing: &Firing,
    attempt: Attempt,
    cmds: &mut Vec<Command>,
) {
    state.remove_firing(firing_id);
    let outcome = Outcome::failure("the retry could not be prepared");
    state.record_outcome(FiringRecord {
        firing: firing_id,
        node: firing.node,
        name: node.name.clone(),
        generation: firing.generation,
        attempt,
        outcome: outcome.clone(),
    });
    route(
        state,
        node,
        firing_id,
        firing.generation,
        attempt,
        &firing.inputs,
        &outcome,
        cmds,
    );
}

// ── Routing ───────────────────────────────────────────────────────────────

/// Evaluate every group once and ask the host to resolve the complete routing
/// decision. Even terminal nodes take this round trip.
fn route(
    state: &mut EngineState,
    node: &Node,
    firing: FiringId,
    generation: Generation,
    attempt: Attempt,
    inputs: &[Token],
    outcome: &Outcome,
    cmds: &mut Vec<Command>,
) {
    let base = match firing_statics(state, node.id, inputs, generation, attempt) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let statics = with_outcome(&base, outcome);
    let token = primary_token(inputs);
    let mut proposals = Vec::with_capacity(node.routing.groups.len());
    for (group_index, group) in node.routing.groups.iter().enumerate() {
        let (tier, pick, candidates) = match &group.policy {
            SelectionPolicy::FirstMatch => {
                let candidate = group.arms.iter().find(|arm| {
                    guard_passes(
                        state,
                        node.id,
                        arm.guard,
                        &format!("guard on edge {}", arm.id),
                        &token,
                        &statics,
                    )
                });
                (
                    None,
                    Some(PickPolicy::First),
                    candidate
                        .into_iter()
                        .map(|arm| routing_candidate(state, arm, None))
                        .collect(),
                )
            }
            SelectionPolicy::Tiered(tiers) => {
                let arms_by_id: BTreeMap<_, _> =
                    group.arms.iter().map(|arm| (arm.id, arm)).collect();
                let mut active = (None, None, Vec::new());
                for (tier_index, tier) in tiers.iter().enumerate() {
                    let mut eligible = Vec::new();
                    for candidate in &tier.candidates {
                        let Some(arm) = arms_by_id.get(&candidate.edge).copied() else {
                            continue;
                        };
                        if !guard_passes(
                            state,
                            node.id,
                            candidate.when,
                            &format!(
                                "guard in routing tier {tier_index} for edge {}",
                                candidate.edge
                            ),
                            &token,
                            &statics,
                        ) {
                            continue;
                        }
                        let rank = if tier.pick == PickPolicy::LowestRankThenArmOrder {
                            let Some(rank) = candidate.rank else {
                                continue;
                            };
                            match eval(
                                &state.graph.exprs,
                                rank,
                                &EvalEnv::new(&token, state.run_context(), &statics),
                            ) {
                                Ok(Value::Null) => continue,
                                Ok(Value::Number(number)) => number.as_f64(),
                                Ok(_) => {
                                    state.push_error(RunError::InvalidRouting {
                                        firing,
                                        reason: SmolStr::new(format!(
                                            "rank for edge {} is not a number or null",
                                            candidate.edge
                                        )),
                                    });
                                    continue;
                                }
                                Err(error) => {
                                    state.push_error(RunError::Eval {
                                        node: node.id,
                                        site: SmolStr::new(format!(
                                            "rank in routing tier {tier_index} for edge {}",
                                            candidate.edge
                                        )),
                                        error,
                                    });
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        eligible.push(routing_candidate(state, arm, rank));
                    }
                    if !eligible.is_empty() {
                        active = (
                            Some(u32::try_from(tier_index).expect("a graph has at most u32 tiers")),
                            Some(tier.pick),
                            eligible,
                        );
                        break;
                    }
                }
                active
            }
        };
        proposals.push(RoutingProposal {
            group: u32::try_from(group_index).expect("a graph has at most u32 routing groups"),
            tier,
            pick,
            candidates,
        });
    }

    let decision_id = DecisionId::route(firing, attempt);
    let restart_allowed = state
        .start()
        .is_some_and(|start| start.execution_index.saturating_add(1) < start.max_executions);
    state.insert_pending_routing(PendingRouting {
        firing,
        node: node.clone(),
        generation,
        attempt,
        inputs: inputs.to_vec(),
        outcome: outcome.clone(),
        run: state.run_context().clone(),
        restart_allowed,
        groups: proposals.clone(),
    });
    cmds.push(Command::ResolveRouting {
        decision_id,
        restart_allowed,
        groups: proposals,
    });
}

fn guard_passes(
    state: &mut EngineState,
    node: NodeId,
    guard: Guard,
    site: &str,
    token: &Value,
    statics: &ir::StaticCtx,
) -> bool {
    match guard {
        Guard::Always => true,
        Guard::Expr(id) => match eval_bool(
            &state.graph.exprs,
            id,
            &EvalEnv::new(token, state.run_context(), statics),
        ) {
            Ok(value) => value,
            Err(error) => {
                state.push_error(RunError::Eval {
                    node,
                    site: SmolStr::new(site),
                    error,
                });
                false
            }
        },
    }
}

fn routing_candidate(state: &EngineState, arm: &ir::Edge, rank: Option<f64>) -> RoutingCandidate {
    RoutingCandidate {
        edge: arm.id,
        weight: arm.weight,
        target: state
            .graph
            .node(arm.to)
            .map_or_else(|| SmolStr::new(""), |node| node.name.clone()),
        rank,
        transition: arm.transition,
        back: arm.back,
    }
}

fn on_routing_resolved(
    state: &mut EngineState,
    decision_id: DecisionId,
    groups: &[GroupDecision],
    queue: &mut VecDeque<Event>,
) {
    let DecisionId::Route { firing, .. } = decision_id else {
        state.push_error(RunError::UnknownDecision(decision_id));
        return;
    };
    let Some(pending) = state.take_pending_routing(firing) else {
        state.push_error(RunError::UnknownDecision(decision_id));
        return;
    };
    if DecisionId::route(pending.firing, pending.attempt) != decision_id {
        state.push_error(RunError::DecisionMismatch {
            decision: decision_id,
        });
        return;
    }
    let prepared = match validate_and_prepare_routes(state, &pending, groups) {
        Ok(prepared) => prepared,
        Err(reason) => {
            state.push_error(RunError::InvalidRouting { firing, reason });
            return;
        }
    };
    // A jump replaces the whole prepared list with itself, so it is only ever
    // the sole element. The applied records carry ids alone; the payloads stay
    // in the stored routes, uncloned.
    if let Some(PreparedRoute::Jump { target, .. }) = prepared.front() {
        queue.push_back(Event::RouteApplied {
            applied: RouteApplied::Jump {
                firing,
                target: *target,
            },
        });
    } else {
        for route in &prepared {
            let applied = match route {
                PreparedRoute::Edge { group, edge, .. } => RouteApplied::Edge {
                    firing,
                    group: *group,
                    edge: *edge,
                },
                PreparedRoute::None { group } => RouteApplied::None {
                    firing,
                    group: *group,
                },
                PreparedRoute::Jump { .. } => continue,
            };
            queue.push_back(Event::RouteApplied { applied });
        }
    }
    state.set_prepared_routes(firing, prepared);
}

fn validate_and_prepare_routes(
    state: &mut EngineState,
    pending: &PendingRouting,
    decisions: &[GroupDecision],
) -> Result<VecDeque<PreparedRoute>, SmolStr> {
    if decisions.len() != pending.groups.len() {
        return Err(SmolStr::new(
            "the group decision count does not match the proposal",
        ));
    }
    let mut prepared = VecDeque::new();
    let mut eval_context = None;
    for (index, (proposal, resolved)) in pending.groups.iter().zip(decisions).enumerate() {
        let group_index = u32::try_from(index).expect("a graph has at most u32 routing groups");
        if proposal.group != group_index || resolved.group != group_index {
            return Err(SmolStr::new("group decisions are not in declared order"));
        }
        let group = pending
            .node
            .routing
            .groups
            .get(index)
            .ok_or_else(|| SmolStr::new("the proposal names an unknown routing group"))?;
        validate_trace(state, pending, group, resolved)?;
        let expected = deterministic_pick(proposal, resolved.draw.as_ref())?;
        match &resolved.decision {
            RouteDecision::Emit(edge) => {
                let Some(arm) = group.arms.iter().find(|arm| arm.id == *edge) else {
                    return Err(SmolStr::new("the selected edge is not an arm of its group"));
                };
                let overridden = resolved
                    .trace
                    .iter()
                    .any(|entry| matches!(entry, Intervention::Override { .. }));
                if !overridden && expected != Some(*edge) {
                    return Err(SmolStr::new(
                        "the selected edge does not match the core proposal",
                    ));
                }
                if arm.transition == EdgeTransition::Restart && !pending.restart_allowed {
                    return Err(SmolStr::new(
                        "a restart edge was emitted after the execution limit",
                    ));
                }
                if eval_context.is_none() {
                    let base = firing_statics(
                        state,
                        pending.node.id,
                        &pending.inputs,
                        pending.generation,
                        pending.attempt,
                    )
                    .map_err(|error| SmolStr::new(error.to_string()))?;
                    eval_context = Some((
                        with_outcome(&base, &pending.outcome),
                        primary_token(&pending.inputs),
                    ));
                }
                let (statics, token) = eval_context
                    .as_ref()
                    .expect("an emitted route has an evaluation context");
                let payload = match arm.map {
                    None => pending.outcome.output.clone(),
                    Some(map) => match eval(
                        &state.graph.exprs,
                        map,
                        &EvalEnv::new(token, &pending.run, statics),
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            state.push_error(RunError::Eval {
                                node: pending.node.id,
                                site: SmolStr::new(format!("map on edge {}", arm.id)),
                                error,
                            });
                            Value::Null
                        }
                    },
                };
                prepared.push_back(PreparedRoute::Edge {
                    group: group_index,
                    edge: arm.id,
                    target: arm.to,
                    generation: if arm.back {
                        pending.generation.next()
                    } else {
                        pending.generation
                    },
                    payload,
                    transition: arm.transition,
                });
            }
            RouteDecision::Jump(target) => {
                if pending.node.routing.groups.len() != 1 || state.graph.node(*target).is_none() {
                    return Err(SmolStr::new(
                        "the jump target or source routing shape is invalid",
                    ));
                }
                prepared.clear();
                prepared.push_back(PreparedRoute::Jump {
                    target:     *target,
                    generation: pending.generation,
                });
                break;
            }
            RouteDecision::None => {
                if expected.is_some() {
                    return Err(SmolStr::new("the host returned None for an eligible route"));
                }
                if matches!(group.fallthrough, ir::Fallthrough::Error) {
                    state.push_error(RunError::NoArmMatched {
                        node:  pending.node.id,
                        group: index,
                    });
                }
                prepared.push_back(PreparedRoute::None { group: group_index });
            }
            RouteDecision::Block { reason } => {
                state.push_error(RunError::RoutingBlocked {
                    firing: pending.firing,
                    reason: reason.clone(),
                });
                prepared.push_back(PreparedRoute::None { group: group_index });
            }
        }
    }
    Ok(prepared)
}

fn validate_trace(
    state: &EngineState,
    pending: &PendingRouting,
    group: &ir::RoutingGroup,
    resolved: &GroupDecision,
) -> Result<(), SmolStr> {
    for intervention in &resolved.trace {
        match intervention {
            Intervention::Override { edge, .. } => {
                if !group.arms.iter().any(|arm| arm.id == *edge) {
                    return Err(SmolStr::new("an override names an undeclared group edge"));
                }
            }
            Intervention::Jump { target, .. } => {
                if pending.node.routing.groups.len() != 1 || state.graph.node(*target).is_none() {
                    return Err(SmolStr::new(
                        "a jump names an undeclared node or ambiguous source",
                    ));
                }
            }
            Intervention::Block { .. } => {}
        }
    }
    Ok(())
}

/// The deterministic pick for one routing proposal, given the recorded draw.
///
/// This is the single definition of the selection tie-breaks. The core's
/// validator recomputes the expected pick with it, and the default host
/// resolver and the engine test harness call the same function — any drift
/// between producer and validator would reject the resolver's own decision as
/// `InvalidRouting` mid-run.
pub fn deterministic_pick(
    proposal: &RoutingProposal,
    draw: Option<&WeightedDraw>,
) -> Result<Option<ir::EdgeId>, SmolStr> {
    if proposal.candidates.is_empty() {
        if draw.is_some() {
            return Err(SmolStr::new("an empty proposal cannot carry a draw"));
        }
        return Ok(None);
    }
    let pick = proposal.pick.unwrap_or(PickPolicy::First);
    if pick != PickPolicy::WeightedRandom && draw.is_some() {
        return Err(SmolStr::new(
            "only weighted random routing may carry a draw",
        ));
    }
    match pick {
        PickPolicy::First => Ok(proposal.candidates.first().map(|candidate| candidate.edge)),
        PickPolicy::HighestWeightThenLexical => {
            let mut winner = &proposal.candidates[0];
            for candidate in &proposal.candidates[1..] {
                if candidate.weight > winner.weight
                    || (candidate.weight == winner.weight && candidate.target < winner.target)
                {
                    winner = candidate;
                }
            }
            Ok(Some(winner.edge))
        }
        PickPolicy::LowestRankThenArmOrder => {
            let mut winner: Option<&RoutingCandidate> = None;
            for candidate in &proposal.candidates {
                let Some(rank) = candidate.rank else {
                    continue;
                };
                if winner.is_none_or(|current| {
                    rank.total_cmp(&current.rank.unwrap_or(f64::INFINITY))
                        .is_lt()
                }) {
                    winner = Some(candidate);
                }
            }
            Ok(winner.map(|candidate| candidate.edge))
        }
        PickPolicy::WeightedRandom => {
            let draw = draw.ok_or_else(|| SmolStr::new("weighted routing requires a draw"))?;
            if Some(draw.tier) != proposal.tier {
                return Err(SmolStr::new("the draw names the wrong tier"));
            }
            let candidates: Vec<_> = proposal.candidates.iter().map(|item| item.edge).collect();
            if draw.candidates != candidates {
                return Err(SmolStr::new(
                    "the draw candidate list differs from the proposal",
                ));
            }
            let total: u64 = proposal
                .candidates
                .iter()
                .map(|item| u64::from(item.weight))
                .sum();
            if total == 0 || draw.total != total || draw.roll >= total {
                return Err(SmolStr::new(
                    "the weighted draw has an invalid total or roll",
                ));
            }
            let mut roll = draw.roll;
            for candidate in &proposal.candidates {
                let weight = u64::from(candidate.weight);
                if roll < weight {
                    return Ok(Some(candidate.edge));
                }
                roll -= weight;
            }
            Err(SmolStr::new("the weighted draw did not select a candidate"))
        }
    }
}

/// The canonical reason a restart route is refused past the execution limit.
pub const RESTART_LIMIT_REASON: &str = "maximum executions per invocation reached";

/// Downgrade an over-budget restart `Emit` into a `Block`.
///
/// The single definition of the restart-limit courtesy every resolver owes: the
/// core rejects an over-budget restart emit as `InvalidRouting`, so a resolver
/// that skipped this downgrade would fail the run instead of blocking the
/// route.
pub fn enforce_restart_limit(
    restart_allowed: bool,
    proposal: &RoutingProposal,
    decision: RouteDecision,
) -> RouteDecision {
    if restart_allowed {
        return decision;
    }
    match decision {
        RouteDecision::Emit(edge)
            if proposal.candidates.iter().any(|candidate| {
                candidate.edge == edge && candidate.transition == EdgeTransition::Restart
            }) =>
        {
            RouteDecision::Block {
                reason: SmolStr::new(RESTART_LIMIT_REASON),
            }
        }
        decision => decision,
    }
}

fn on_route_applied(
    state: &mut EngineState,
    applied: RouteApplied,
    cmds: &mut Vec<Command>,
    queue: &mut VecDeque<Event>,
) {
    let firing = applied.firing();
    let Some(prepared) = state.prepared_route(firing) else {
        state.push_error(RunError::InvalidRouting {
            firing,
            reason: SmolStr::new("RouteApplied has no prepared decision"),
        });
        return;
    };
    let matches = match (&applied, prepared) {
        (
            RouteApplied::Edge { group, edge, .. },
            PreparedRoute::Edge {
                group: expected_group,
                edge: expected_edge,
                ..
            },
        ) => group == expected_group && edge == expected_edge,
        (
            RouteApplied::Jump { target, .. },
            PreparedRoute::Jump {
                target: expected, ..
            },
        ) => target == expected,
        (RouteApplied::None { group, .. }, PreparedRoute::None { group: expected }) => {
            group == expected
        }
        _ => false,
    };
    if !matches {
        state.push_error(RunError::InvalidRouting {
            firing,
            reason: SmolStr::new("RouteApplied differs from the resolved decision"),
        });
        return;
    }
    let Some(prepared) = state.take_prepared_route(firing) else {
        return;
    };
    match prepared {
        PreparedRoute::Edge {
            edge,
            target: _,
            generation,
            payload,
            transition: EdgeTransition::Continue,
            ..
        } => queue.push_back(Event::TokenEmitted {
            token: Token::new(edge, generation, payload, firing),
        }),
        PreparedRoute::Edge {
            edge,
            target,
            transition: EdgeTransition::Restart,
            ..
        } => {
            state.begin_restart(RestartIntent {
                edge,
                target,
                source: firing,
            });
            begin_restart_shutdown(state, cmds);
        }
        PreparedRoute::Jump { target, generation } => {
            let edge = state.next_edge_id();
            state.register_seed_edge(edge, target);
            state.force_entry(target, generation);
            queue.push_back(Event::TokenEmitted {
                token: Token::new(edge, generation, Value::Null, firing),
            });
        }
        PreparedRoute::None { .. } => {}
    }
}

// ── Expansion ─────────────────────────────────────────────────────────────

/// Build the splice for a `for_each` node and hand it back as an event, so the
/// clones appear in the log exactly as they appear in the graph.
fn expand(
    state: &mut EngineState,
    node: &Node,
    generation: Generation,
    inputs: &[Token],
    queue: &mut VecDeque<Event>,
) {
    let Some(Expansion::ForEach {
        items,
        target,
        max_parallel,
        fail_fast,
    }) = node.expand.clone()
    else {
        return;
    };

    let statics = match firing_statics(state, node.id, inputs, generation, Attempt::FIRST) {
        Ok(statics) => statics,
        Err(error) => {
            state.push_error(error);
            return;
        }
    };
    let token = primary_token(inputs);
    let items = match eval(
        &state.graph.exprs,
        items,
        &EvalEnv::new(&token, state.run_context(), &statics),
    ) {
        Ok(value) => value,
        Err(error) => {
            state.push_error(RunError::Eval {
                node: node.id,
                site: SmolStr::new("for_each items"),
                error,
            });
            return;
        }
    };
    let Value::Array(items) = items else {
        state.push_error(RunError::ItemsNotArray {
            node: node.id,
            got:  SmolStr::new(type_name(&items)),
        });
        return;
    };

    // The region to clone. `ExpandTarget::Node` is the one-node case of the same
    // rule, so both go through one code path.
    let region: Vec<NodeId> = match target {
        ExpandTarget::Node => vec![node.id],
        ExpandTarget::Subgraph { entry, exit } => {
            if entry != node.id {
                state.push_error(RunError::ExpansionEntryMismatch { node: node.id });
                return;
            }
            region_nodes(state, entry, exit)
        }
    };
    let region_entry = node.id;

    let payload = inputs.first().map_or(Value::Null, |t| t.payload.clone());

    let mut clones = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "every clone reserves node ids from the graph's `u32` id space, so a run \
                      can never hold more clones than a `u32` counts"
        )]
        let index = index as u32;
        // Old id to new id, decided before any edge is rewritten so edges inside
        // the region point at clones and edges leaving it still point outward.
        // Ids come from the state's allocator, not `nodes.len()`: another
        // expansion may already sit in the queue with ids past the live length.
        let mut remap: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        for old in &region {
            remap.insert(*old, state.next_node_id());
        }

        let mut nodes = Vec::with_capacity(region.len());
        for old in &region {
            let Some(source) = state.graph.node(*old) else {
                continue;
            };
            let mut clone = source.clone();
            clone.id = remap[old];
            clone.name = SmolStr::new(format!("{}#{index}", source.name));
            clone.cancel_group = source
                .cancel_group
                .map(|anchor| remap.get(&anchor).copied().unwrap_or(anchor));
            // Clones never expand again; that is what makes expansion terminate.
            clone.expand = None;
            for group in &mut clone.routing.groups {
                // Fresh edge ids, and the tiers that name edges follow them: a
                // tiered policy's candidates refer to arms by id, so a clone
                // whose tiers still named the template's arms would find no
                // eligible edge and emit nothing.
                let mut edge_map: BTreeMap<ir::EdgeId, ir::EdgeId> = BTreeMap::new();
                for arm in &mut group.arms {
                    let fresh = state.next_edge_id();
                    edge_map.insert(arm.id, fresh);
                    arm.id = fresh;
                    if let Some(new_target) = remap.get(&arm.to) {
                        arm.to = *new_target;
                    }
                }
                group.policy = remap_selection_policy(
                    &group.policy,
                    |edge| edge_map.get(&edge).copied().unwrap_or(edge),
                    |expr| expr,
                );
            }
            nodes.push(clone);
        }

        let entry = remap[&region_entry];
        let seed_edge = state.next_edge_id();
        clones.push(SpliceClone {
            index,
            item: item.clone(),
            nodes,
            entry,
            seed_edge,
        });
    }

    let splice = SubgraphSplice {
        cancel_scope: state.next_cancel_scope_id(),
        source: node.id,
        region: region.clone(),
        generation,
        payload,
        clones,
        max_parallel,
        fail_fast,
    };
    queue.push_back(Event::NodeExpanded {
        node: node.id,
        splice,
    });
}

/// Nodes reachable from `entry` without passing through `exit`, plus `exit`.
/// Validation has already checked that the exit postdominates the entry.
fn region_nodes(state: &EngineState, entry: NodeId, exit: NodeId) -> Vec<NodeId> {
    let mut seen = BTreeSet::from([entry]);
    let mut order = vec![entry];
    let mut queue = VecDeque::from([entry]);
    while let Some(node) = queue.pop_front() {
        if node == exit {
            continue;
        }
        let Some(nd) = state.graph.node(node) else {
            continue;
        };
        for edge in nd.routing.edges() {
            if state.graph.node(edge.to).is_some() && seen.insert(edge.to) {
                order.push(edge.to);
                queue.push_back(edge.to);
            }
        }
    }
    order
}

/// The `ForEach` producer's second half: turn the logged [`SubgraphSplice`]
/// into a [`PreparedSplice`] and hand it to the one applicator. The event stays
/// the wire shape; the applicator is the only code that mutates the live graph.
fn on_node_expanded(
    state: &mut EngineState,
    source: NodeId,
    splice: SubgraphSplice,
    queue: &mut VecDeque<Event>,
) {
    let parent = state.cancel_scope_of(source);

    let mut nodes = Vec::new();
    let mut bindings = BTreeMap::new();
    let mut seeds = Vec::new();
    for clone in splice.clones {
        let clone_binding = clone_bindings(clone.index, &clone.item);
        for node in clone.nodes {
            bindings.insert(node.id, clone_binding.clone());
            nodes.push(node);
        }
        seeds.push(PreparedSeed {
            edge:       clone.seed_edge,
            entry:      clone.entry,
            generation: splice.generation,
            payload:    splice.payload.clone(),
        });
    }

    let prepared = PreparedSplice {
        owner: source,
        cancel_scope: splice.cancel_scope,
        parent_scope: parent,
        nodes,
        exprs: Vec::new(),
        scopes: Vec::new(),
        bindings,
        seeds,
        routing_extensions: Vec::new(),
        origin: SpliceOrigin::Expansion,
        policy: BatchPolicy {
            max_parallel: splice.max_parallel,
            fail_fast:    splice.fail_fast,
        },
        effects: splice
            .region
            .into_iter()
            .map(SpliceEffect::Supersede)
            .collect(),
    };
    apply_prepared_splice(state, prepared, queue);
}

// ── Host-delivered controls ───────────────────────────────────────────────

/// A host asks for a control to reach one live firing (§6).
///
/// Only `Control::Deliver` to a live, not-cancelling, not-awaiting-retry firing
/// produces a `DeliverControl` command; no state changes and nothing routes.
/// Everything else is a logged no-op — the event is the audit trail either way:
///
/// - `Cancel` and `Kill` have their own scope-routed events whose closure
///   bookkeeping (`cancelling`, kill tiers, `run_on_cancel` admission) a raw
///   per-firing path would bypass.
/// - A dead, unknown or awaiting-retry firing is never a `RunError`: a late
///   answer must not fail the run.
///
/// The command-or-no-command result is the disposition the driver reports.
fn on_control_requested(
    state: &mut EngineState,
    firing: FiringId,
    ctl: Control,
    cmds: &mut Vec<Command>,
) {
    if !matches!(ctl, Control::Deliver(_)) {
        return;
    }
    let deliverable = state
        .firing(firing)
        .is_some_and(|f| !f.cancelling && !f.awaiting_retry)
        && !state.is_awaiting_admission(firing);
    if deliverable {
        cmds.push(Command::DeliverControl { firing, ctl });
    }
}

// ── Cancellation ──────────────────────────────────────────────────────────

/// The polite tier. Live firings get `Control::Cancel`; pending tokens survive,
/// so nodes in the scope complete `Cancelled` — or fire, when marked
/// `run_on_cancel` — as their joins satisfy (§5).
fn on_cancel(state: &mut EngineState, scope: CancelScopeId, cmds: &mut Vec<Command>) {
    if scope == CancelScopeId::ROOT {
        state.clear_restart();
    }
    stop_scope(state, scope, false, true, cmds);
}

/// The forced tier: the pre-v3 cancel behavior, kept under its own event.
/// Tokens drop, nothing routes, nothing is admitted — `run_on_cancel` included
/// — and `Control::Kill` reaches every live firing, already-cancelling ones
/// too.
fn on_kill(state: &mut EngineState, scope: CancelScopeId, cmds: &mut Vec<Command>) {
    if scope == CancelScopeId::ROOT {
        state.clear_restart();
        state.clear_pending_decisions();
    }
    stop_scope(state, scope, true, true, cmds);
}

fn begin_restart_shutdown(state: &mut EngineState, cmds: &mut Vec<Command>) {
    stop_scope(state, CancelScopeId::ROOT, false, false, cmds);
}

/// Both tiers share one shape — mark the closure, doom its live firings — and
/// differ only where the spec says they do: a kill drops tokens, routes
/// nothing, and reaches firings the polite tier already signalled.
fn stop_scope(
    state: &mut EngineState,
    scope: CancelScopeId,
    kill: bool,
    records_root_cancel: bool,
    cmds: &mut Vec<Command>,
) {
    let closure = state.cancel_scope_closure(scope);
    for id in &closure {
        if kill {
            state.mark_scope_killed(*id);
        } else {
            state.mark_scope_cancelled(*id);
        }
    }
    let root = scope == CancelScopeId::ROOT;
    if root && records_root_cancel {
        state.mark_cancelled();
    }
    if kill {
        if root {
            state.drop_all_tokens();
        } else {
            let nodes = state.nodes_in_scopes(&closure);
            state.drop_tokens_for_nodes(&nodes);
        }
    }

    let doomed: Vec<Firing> = state
        .live_firings()
        .filter(|f| root || closure.contains(&f.cancel_scope))
        .cloned()
        .collect();
    for firing in doomed {
        if state.is_awaiting_admission(firing.id) {
            settle_awaiting_admission(state, &firing, !kill, cmds);
            continue;
        }
        // A firing waiting out a retry backoff has no work in flight and no driver
        // task to deliver to, so the core settles it at once: recorded — and, under
        // a cancel only, routed.
        if firing.awaiting_retry {
            settle_awaiting_retry(state, &firing, !kill, cmds);
            continue;
        }
        if let Some(f) = state.firing_mut(firing.id) {
            // Only the polite tier skips already-cancelling firings: a plain Cancel
            // cannot say "skip the ladder", so a kill reaches them too.
            if !kill && f.cancelling {
                continue;
            }
            f.cancelling = true;
        }
        cmds.push(Command::DeliverControl {
            firing: firing.id,
            ctl:    if kill { Control::Kill } else { Control::Cancel },
        });
    }
}

/// Settle a firing that a cancel or kill caught mid-backoff: record `Cancelled`
/// now — there is no task to deliver a control to — and leave a tombstone for
/// the driver's unrecallable `RetryElapsed`. Under a cancel the outcome routes;
/// under a kill it does not.
fn settle_awaiting_retry(
    state: &mut EngineState,
    firing: &Firing,
    routes: bool,
    cmds: &mut Vec<Command>,
) {
    state.add_retry_tombstone(firing.id);
    settle_parked_firing(state, firing, routes, cmds);
}

/// The same settle for a firing caught awaiting admission: its pending decision
/// is withdrawn instead of a tombstone being left.
fn settle_awaiting_admission(
    state: &mut EngineState,
    firing: &Firing,
    routes: bool,
    cmds: &mut Vec<Command>,
) {
    state.remove_admission_for_firing(firing.id);
    settle_parked_firing(state, firing, routes, cmds);
}

/// The shared settle: retire the firing, record `Cancelled`, and — under a
/// cancel only — route the outcome.
fn settle_parked_firing(
    state: &mut EngineState,
    firing: &Firing,
    routes: bool,
    cmds: &mut Vec<Command>,
) {
    state.remove_firing(firing.id);
    let Some(node) = state.graph.node(firing.node).cloned() else {
        state.push_error(RunError::UnknownNode(firing.node));
        return;
    };
    let outcome = synthetic(Status::Cancelled);
    state.record_outcome(FiringRecord {
        firing:     firing.id,
        node:       firing.node,
        name:       node.name.clone(),
        generation: firing.generation,
        attempt:    firing.attempt,
        outcome:    outcome.clone(),
    });
    if routes {
        route(
            state,
            &node,
            firing.id,
            firing.generation,
            firing.attempt,
            &firing.inputs,
            &outcome,
            cmds,
        );
    }
}

// ── Admission control and completion ──────────────────────────────────────

fn admit_deferred(state: &mut EngineState, cmds: &mut Vec<Command>, queue: &mut VecDeque<Event>) {
    let held = state.take_deferred();
    for key in held {
        try_fire(state, key, cmds, queue);
    }
}

fn finish_if_quiescent(state: &mut EngineState, cmds: &mut Vec<Command>) {
    if !state.is_started() || state.is_finished() || !state.is_quiescent() {
        return;
    }
    let exit = match state.restart_intent() {
        Some(intent) if !state.is_cancelled() => EngineExit::Restart {
            edge:   intent.edge,
            target: intent.target,
            source: intent.source,
        },
        _ => EngineExit::Terminal {
            status: state.folded_status(),
        },
    };
    state.mark_finished(exit.clone());
    // Terminal release, in the same transition as the finish: a token parked at an
    // unsatisfiable join would otherwise hold its environment forever, and a
    // finished serialized state must claim no resources.
    for scope in state.release_all_scopes() {
        cmds.push(Command::ReleaseScope { scope });
    }
    cmds.push(Command::FinishExecution { exit });
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
