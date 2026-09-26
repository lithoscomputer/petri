//! The splice boundary: preparation and the one applicator.
//!
//! Three representations, one direction of travel. A `SpliceRequest` is
//! fragment-local, serialized in `StepFinished`, and untrusted. Preparation
//! ([`prepare_outcome_splices`]) turns an ordered list of them into a
//! [`SplicePlan`] against a scratch view of `EngineState` — remapping every
//! identifier from live-graph high-water marks, resolving attachments, and
//! computing retraction — without touching canonical state, so a rejected
//! transaction leaks nothing, not even allocator movement. [`PreparedSplice`]
//! is engine-private, non-serializable, and constructible only here (or by the
//! `ForEach` producer); `apply_prepared_splice` is infallible for domain
//! errors, because validation is a type boundary, not a convention.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ir::{
    BinOp, CancelScopeId, Edge, EdgeId, Expr, ExprId, ExprOrValue, FailureClass, FailureInfo,
    Generation, Guard, JoinPolicy, Local, Node, NodeId, Outcome, Scope, ScopeId, SelectGroup,
    SpliceContext, SpliceMode, SplicePolicy, SpliceRequest, Status, StepRef, Token, Value,
    placeholder, validate, validate_request,
};
use smol_str::SmolStr;

use crate::event::Event;
use crate::state::{
    AdmissionKey, Allocators, AppliedSplice, BatchPolicy, EngineState, SpliceEffect, SpliceOrigin,
};

/// The failure class every rejected splice transaction converts to. Registered
/// in the §13 table; an ordinary retry class — a matching `retry_on` re-runs
/// the step, and a later attempt can succeed.
pub const INVALID_SPLICE_CLASS: FailureClass = FailureClass::new_static("invalid_splice");

/// Copy a selection policy while moving its edge and expression ids into a
/// new id space.
pub(crate) fn remap_selection_policy<S: Copy, T>(
    policy: &ir::SelectionPolicy<S>,
    mut edge: impl FnMut(EdgeId<S>) -> EdgeId<T>,
    mut expr: impl FnMut(ExprId<S>) -> ExprId<T>,
) -> ir::SelectionPolicy<T> {
    match policy {
        ir::SelectionPolicy::FirstMatch => ir::SelectionPolicy::FirstMatch,
        ir::SelectionPolicy::Tiered(tiers) => ir::SelectionPolicy::Tiered(
            tiers
                .iter()
                .map(|tier| ir::Tier {
                    candidates: tier
                        .candidates
                        .iter()
                        .map(|candidate| ir::Candidate {
                            edge: edge(candidate.edge),
                            when: match candidate.when {
                                Guard::Always => Guard::Always,
                                Guard::Expr(id) => Guard::Expr(expr(id)),
                            },
                            rank: candidate.rank.map(&mut expr),
                        })
                        .collect(),
                    pick:       tier.pick,
                })
                .collect(),
        ),
    }
}

/// The canonical invalid-splice conversion: the outcome becomes a
/// `Failure{class: invalid_splice}` with `message`, output and metrics are
/// kept, and `context_updates` drop with the requests. Both converters — the
/// engine's preparation rejection and the driver's secret backstop — go through
/// here, so the rule cannot drift.
pub fn reject_splices(outcome: Outcome, message: impl Into<String>) -> Outcome {
    Outcome {
        status: Status::Failure(FailureInfo::new(message).with_class(INVALID_SPLICE_CLASS)),
        context_updates: BTreeMap::new(),
        splices: Vec::new(),
        ..outcome
    }
}

// ── Preparation errors ────────────────────────────────────────────────────

/// Why a splice transaction was rejected. Preparation wraps the fragment-local
/// [`ir::FragmentValidationError`] — which cannot know a request index — with
/// the index and phase; only the engine boundary (`on_step_finished`) maps this
/// to the canonical `Failure{class: invalid_splice}`.
#[derive(Clone, Debug, thiserror::Error)]
#[error("splice request {request_index} rejected during {}: {source}", .source.phase())]
pub(crate) struct SplicePreparationError {
    pub request_index: usize,
    pub source:        PreparationRejection,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum PreparationRejection {
    #[error("{}", format_fragment_errors(.0))]
    Fragment(Vec<ir::FragmentValidationError>),
    #[error("the uploader's policy {policy:?} does not authorize {mode:?}")]
    Policy {
        policy: SplicePolicy,
        mode:   SpliceMode,
    },
    #[error(
        "fragment node `{node}` declares policy {declared:?}, above its uploader's {cap:?}; \
         a fragment can never mint more authority than its uploader"
    )]
    Delegation {
        node:     SmolStr,
        declared: SplicePolicy,
        cap:      SplicePolicy,
    },
    #[error("instance name `{name}` collides with a node already in the graph")]
    NameCollision { name: SmolStr },
    #[error("`depends_on` names `{name}`, which no live or planned node carries")]
    UnknownReference { name: SmolStr },
    #[error(
        "`depends_on` target `{name}` has admissions in more than one generation; \
         a reference must resolve to exactly one admission, so loop nodes reject in v1"
    )]
    AmbiguousReference { name: SmolStr },
    #[error("`depends_on` target `{name}` was retracted by an earlier request in this outcome")]
    RetractedReference { name: SmolStr },
    #[error(
        "dependent `{name}` joins with {join:?}; only `All` joins can be extended to \
         wait for the batch in v1"
    )]
    DependentJoinNotAll { name: SmolStr, join: JoinPolicy },
}

impl PreparationRejection {
    /// Which preparation phase this rejection comes from — policy
    /// authorization, fragment-local validation, or composition with the
    /// live graph.
    fn phase(&self) -> &'static str {
        match self {
            Self::Policy { .. } | Self::Delegation { .. } => "policy",
            Self::Fragment(_) => "validation",
            Self::NameCollision { .. }
            | Self::UnknownReference { .. }
            | Self::AmbiguousReference { .. }
            | Self::RetractedReference { .. }
            | Self::DependentJoinNotAll { .. } => "composition",
        }
    }
}

fn format_fragment_errors(errors: &[ir::FragmentValidationError]) -> String {
    match errors.first() {
        Some(first) if errors.len() == 1 => first.to_string(),
        Some(first) => format!("{first} (and {} more)", errors.len() - 1),
        None => "invalid fragment".to_string(),
    }
}

// ── The prepared shape ────────────────────────────────────────────────────

/// One seed for a spliced entry: the synthetic incoming edge, the node it
/// feeds, and the token placed on it.
pub(crate) struct PreparedSeed {
    pub edge:       EdgeId,
    pub entry:      NodeId,
    pub generation: Generation,
    pub payload:    Value,
}

/// A splice with every identifier remapped and every check complete: the only
/// input the applicator accepts.
pub(crate) struct PreparedSplice {
    /// The node whose firing produced this batch: the expansion source, or the
    /// uploader. Stamped on the record — ownership is core-derived.
    pub owner:              NodeId,
    pub cancel_scope:       CancelScopeId,
    /// The scope the fresh batch scope nests under: the producing firing's
    /// current cancel scope.
    pub parent_scope:       CancelScopeId,
    /// Fully formed live-space nodes, ids contiguous with the live graph.
    pub nodes:              Vec<Node>,
    /// Expressions appended to the live table, ids contiguous with it: the
    /// fragment's own, then the synthesized attachment guards.
    pub exprs:              Vec<Expr>,
    /// Resource scopes appended to the live list, ids contiguous with it.
    pub scopes:             Vec<Scope>,
    /// `item` / `index` bindings for expansion clones; empty for uploads.
    pub bindings:           BTreeMap<NodeId, BTreeMap<SmolStr, Value>>,
    /// Seed tokens for entries fed by no real edge (`ForEach` clones). Uploaded
    /// fragments attach through real select groups instead and seed nothing.
    pub seeds:              Vec<PreparedSeed>,
    /// Select groups appended to nodes once they are live: the uploader's entry
    /// groups, and `depends_on` edges on not-final or earlier-planned
    /// references.
    pub routing_extensions: Vec<(NodeId, SelectGroup)>,
    pub origin:             SpliceOrigin,
    pub policy:             BatchPolicy,
    /// State changes applied with the graph additions and recorded on the
    /// [`AppliedSplice`].
    pub effects:            Vec<SpliceEffect>,
}

/// Every request in one final outcome, prepared: the transaction plan. Commit
/// consumes it in order; nothing partial ever commits.
pub(crate) struct SplicePlan {
    pub batches:    Vec<PreparedSplice>,
    pub allocators: Allocators,
}

// ── Preparation ───────────────────────────────────────────────────────────

/// Prepare every request in order against an evolving scratch view. Canonical
/// state is read, never written.
pub(crate) fn prepare_outcome_splices(
    state: &EngineState,
    uploader: &Node,
    generation: Generation,
    parent_scope: CancelScopeId,
    requests: &[SpliceRequest],
) -> Result<SplicePlan, SplicePreparationError> {
    SpliceTransaction::new(state, uploader, generation, parent_scope).prepare(requests)
}

/// Scratch state for one outcome's atomic splice transaction. Every field is a
/// prepared view; canonical [`EngineState`] remains borrowed and unchanged
/// until the resulting [`SplicePlan`] commits.
struct SpliceTransaction<'a> {
    state:                &'a EngineState,
    uploader:             &'a Node,
    generation:           Generation,
    parent_scope:         CancelScopeId,
    allocators:           Allocators,
    expr_cursor:          u32,
    scope_cursor:         u32,
    batches:              Vec<PreparedSplice>,
    names:                BTreeMap<SmolStr, NodeId>,
    retracted:            BTreeSet<AdmissionKey>,
    loop_nodes:           OnceCell<BTreeSet<NodeId>>,
    pending_keys:         OnceCell<Vec<AdmissionKey>>,
    owned_nodes:          OnceCell<BTreeSet<NodeId>>,
    ambiguous_references: BTreeMap<NodeId, bool>,
}

/// The remapped graph data and attachment effects for one request before it is
/// sealed as a [`PreparedSplice`].
struct BatchDraft {
    node_base:          u32,
    expr_base:          u32,
    nodes:              Vec<Node>,
    exprs:              Vec<Expr>,
    scopes:             Vec<Scope>,
    routing_extensions: Vec<(NodeId, SelectGroup)>,
    retracted:          Vec<AdmissionKey>,
    inherit_bindings:   bool,
}

impl<'a> SpliceTransaction<'a> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`ExprId` and `ScopeId` are `u32`, so the live tables they index can never \
                  hold more entries than a `u32` counts"
    )]
    fn new(
        state: &'a EngineState,
        uploader: &'a Node,
        generation: Generation,
        parent_scope: CancelScopeId,
    ) -> Self {
        Self {
            state,
            uploader,
            generation,
            parent_scope,
            allocators: state.allocator_snapshot(),
            expr_cursor: state.graph.exprs.len() as u32,
            scope_cursor: state.graph.scopes.len() as u32,
            batches: Vec::new(),
            names: state
                .graph
                .nodes
                .iter()
                .map(|node| (node.name.clone(), node.id))
                .collect(),
            retracted: BTreeSet::new(),
            loop_nodes: OnceCell::new(),
            pending_keys: OnceCell::new(),
            owned_nodes: OnceCell::new(),
            ambiguous_references: BTreeMap::new(),
        }
    }

    fn prepare(mut self, requests: &[SpliceRequest]) -> Result<SplicePlan, SplicePreparationError> {
        for (request_index, request) in requests.iter().enumerate() {
            self.prepare_request(request_index, request)?;
        }
        Ok(SplicePlan {
            batches:    self.batches,
            allocators: self.allocators,
        })
    }

    fn prepare_request(
        &mut self,
        request_index: usize,
        request: &SpliceRequest,
    ) -> Result<(), SplicePreparationError> {
        self.authorize_and_validate(request_index, request)?;
        let retracted = self.prepare_retractions(request);
        let mut draft = self.remap_fragment(request, retracted);
        self.register_names(request_index, &draft.nodes)?;
        self.attach_entries(&request.fragment, &mut draft);
        self.attach_dependents(request_index, &request.fragment, &mut draft)?;
        self.attach_dependencies(request_index, request, &mut draft)?;
        self.push_batch(draft);
        Ok(())
    }

    /// Policy runs before validation, preserving the transaction's rejection
    /// order. Excess authority rejects; nothing is clamped.
    fn authorize_and_validate(
        &self,
        request_index: usize,
        request: &SpliceRequest,
    ) -> Result<(), SplicePreparationError> {
        let reject = |source| Self::rejection(request_index, source);
        if !self.uploader.splice_policy.authorizes(&request.mode) {
            return Err(reject(PreparationRejection::Policy {
                policy: self.uploader.splice_policy,
                mode:   request.mode,
            }));
        }
        for node in &request.fragment.nodes {
            if !self.uploader.splice_policy.may_delegate(node.splice_policy) {
                return Err(reject(PreparationRejection::Delegation {
                    node:     node.name.clone(),
                    declared: node.splice_policy,
                    cap:      self.uploader.splice_policy,
                }));
            }
        }
        validate_request(request).map_err(|errors| reject(PreparationRejection::Fragment(errors)))
    }

    /// Compute this request's retractions against the same canonical snapshot
    /// as every other request, excluding keys an earlier request already
    /// took.
    fn prepare_retractions(&mut self, request: &SpliceRequest) -> Vec<AdmissionKey> {
        let mut batch = Vec::new();
        let SpliceMode::Replace { scope } = request.mode else {
            return batch;
        };
        let state = self.state;
        let owner = self.uploader.id;
        for key in self
            .pending_keys
            .get_or_init(|| state.pending_admission_keys())
        {
            if self.retracted.contains(key) {
                continue;
            }
            let qualifies = match scope {
                ir::ReplaceScope::AllPending => true,
                ir::ReplaceScope::OwnBatches => self
                    .owned_nodes
                    .get_or_init(|| {
                        state
                            .splices()
                            .iter()
                            .filter(|batch| batch.owner == owner)
                            .flat_map(|batch| batch.nodes.iter().copied())
                            .collect()
                    })
                    .contains(&key.node),
            };
            if qualifies {
                self.retracted.insert(*key);
                batch.push(*key);
            }
        }
        batch
    }

    /// Remap one fragment into fresh live ids. All allocator movement remains
    /// on the transaction copy.
    fn remap_fragment(
        &mut self,
        request: &SpliceRequest,
        retracted: Vec<AdmissionKey>,
    ) -> BatchDraft {
        let fragment = &request.fragment;
        let node_base = self.allocators.reserve_nodes(fragment.nodes.len());
        let expr_base = self.expr_cursor;
        let inherited = match request.context {
            SpliceContext::Isolated => None,
            SpliceContext::InheritUploader(scope) => Some(scope),
        };
        let mut next_scope = self.scope_cursor;
        let scope_map: Vec<ScopeId> = fragment
            .scopes
            .iter()
            .map(|scope| {
                if Some(scope.id) == inherited {
                    self.uploader.scope
                } else {
                    let id = ScopeId::new(next_scope);
                    next_scope += 1;
                    id
                }
            })
            .collect();
        BatchDraft {
            node_base,
            expr_base,
            exprs: fragment
                .exprs
                .iter()
                .map(|(_, expr)| shift_expr(expr, expr_base))
                .collect(),
            scopes: fragment
                .scopes
                .iter()
                .filter(|scope| Some(scope.id) != inherited)
                .map(|scope| remap_scope(scope, scope_map[scope.id.index()], expr_base))
                .collect(),
            nodes: fragment
                .nodes
                .iter()
                .map(|node| {
                    remap_node(node, node_base, &scope_map, expr_base, &mut self.allocators)
                })
                .collect(),
            routing_extensions: Vec::new(),
            retracted,
            inherit_bindings: inherited.is_some(),
        }
    }

    fn register_names(
        &mut self,
        request_index: usize,
        nodes: &[Node],
    ) -> Result<(), SplicePreparationError> {
        for node in nodes {
            if self.names.contains_key(&node.name) {
                return Err(Self::rejection(
                    request_index,
                    PreparationRejection::NameCollision {
                        name: node.name.clone(),
                    },
                ));
            }
            self.names.insert(node.name.clone(), node.id);
        }
        Ok(())
    }

    /// Attach fragment entries to the uploader, gated by success-like status
    /// and the exact uploading generation.
    fn attach_entries(&mut self, fragment: &ir::GraphFragment, draft: &mut BatchDraft) {
        if fragment.entry.is_empty() {
            return;
        }
        let outcome = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Var(SmolStr::new("outcome")),
        );
        let success_like = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Field(outcome, SmolStr::new("success_like")),
        );
        let generation = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Var(SmolStr::new("generation")),
        );
        let expected = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Lit(Value::from(self.generation.raw())),
        );
        let same_generation = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Binary(BinOp::Eq, generation, expected),
        );
        let guard = push_expr(
            &mut draft.exprs,
            draft.expr_base,
            Expr::Binary(BinOp::And, success_like, same_generation),
        );
        for entry in &fragment.entry {
            draft.routing_extensions.push((
                self.uploader.id,
                SelectGroup::new(vec![Edge::when(
                    self.allocators.take_edge(),
                    NodeId::new(draft.node_base + entry.raw()),
                    guard,
                )]),
            ));
        }
    }

    /// Extend each existing forward dependent of the uploader through every
    /// fragment exit.
    fn attach_dependents(
        &mut self,
        request_index: usize,
        fragment: &ir::GraphFragment,
        draft: &mut BatchDraft,
    ) -> Result<(), SplicePreparationError> {
        if fragment.exits.is_empty() {
            return Ok(());
        }
        let mut seen = BTreeSet::new();
        let dependents: Vec<NodeId> = self
            .uploader
            .routing
            .edges()
            .filter(|edge| !edge.back)
            .filter(|edge| seen.insert(edge.to))
            .map(|edge| edge.to)
            .filter(|node| self.state.graph.node(*node).is_some())
            .filter(|node| !self.state.is_superseded(*node))
            .collect();
        for dependent in dependents {
            if self.retracted.contains(&AdmissionKey {
                node:       dependent,
                generation: self.generation,
            }) {
                continue;
            }
            let node = self.state.graph.node(dependent).expect("filtered above");
            if node.join != JoinPolicy::All {
                return Err(Self::rejection(
                    request_index,
                    PreparationRejection::DependentJoinNotAll {
                        name: node.name.clone(),
                        join: node.join,
                    },
                ));
            }
            for exit in &fragment.exits {
                draft.nodes[exit.index()]
                    .routing
                    .groups
                    .push(SelectGroup::new(vec![Edge::always(
                        self.allocators.take_edge(),
                        dependent,
                    )]));
            }
        }
        Ok(())
    }

    /// Resolve explicit `depends_on` attachments against live and
    /// earlier-planned names in the transaction.
    fn attach_dependencies(
        &mut self,
        request_index: usize,
        request: &SpliceRequest,
        draft: &mut BatchDraft,
    ) -> Result<(), SplicePreparationError> {
        for attachment in &request.attachments {
            let ir::Attachment::DependsOn { node, on } = attachment;
            let dependent_index = node.index();
            let dependent = NodeId::new(draft.node_base + node.raw());
            let name = SmolStr::new(on.as_str());
            let Some(&target) = self.names.get(name.as_str()) else {
                return Err(Self::rejection(
                    request_index,
                    PreparationRejection::UnknownReference { name },
                ));
            };
            if self.state.graph.node(target).is_some() {
                if self.retracted.iter().any(|key| key.node == target) {
                    return Err(Self::rejection(
                        request_index,
                        PreparationRejection::RetractedReference { name },
                    ));
                }
                let loop_nodes = &self.loop_nodes;
                let state = self.state;
                let ambiguous = *self.ambiguous_references.entry(target).or_insert_with(|| {
                    loop_nodes
                        .get_or_init(|| validate::loop_reachable(&state.graph))
                        .contains(&target)
                        || state.has_multiple_admission_generations(target)
                });
                if ambiguous {
                    return Err(Self::rejection(
                        request_index,
                        PreparationRejection::AmbiguousReference { name },
                    ));
                }
                if self.state.run_context().node(name.as_str()).is_some() {
                    let guard = record_exists_guard(&mut draft.exprs, draft.expr_base, &name);
                    conjoin_precondition(
                        &mut draft.nodes[dependent_index],
                        &mut draft.exprs,
                        draft.expr_base,
                        guard,
                    );
                    continue;
                }
            }
            // A non-final live node or any planned node is live by the time this
            // batch applies, because batches commit in request order.
            draft.routing_extensions.push((
                target,
                SelectGroup::new(vec![Edge::always(self.allocators.take_edge(), dependent)]),
            ));
        }
        Ok(())
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a batch appends to the same `u32`-indexed expression and scope tables, so \
                  neither length can outgrow a `u32`"
    )]
    fn push_batch(&mut self, draft: BatchDraft) {
        let BatchDraft {
            expr_base,
            nodes,
            exprs,
            scopes,
            routing_extensions,
            retracted,
            inherit_bindings,
            ..
        } = draft;
        self.expr_cursor = expr_base + exprs.len() as u32;
        self.scope_cursor += scopes.len() as u32;
        let bindings = if inherit_bindings {
            self.state
                .clone_bindings_for(self.uploader.id)
                .map(|bindings| {
                    nodes
                        .iter()
                        .map(|node| (node.id, bindings.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        self.batches.push(PreparedSplice {
            owner: self.uploader.id,
            cancel_scope: self.allocators.take_cancel_scope(),
            parent_scope: self.parent_scope,
            nodes,
            exprs,
            scopes,
            bindings,
            seeds: Vec::new(),
            routing_extensions,
            origin: SpliceOrigin::Outcome,
            policy: BatchPolicy::default(),
            effects: retracted.into_iter().map(SpliceEffect::Retract).collect(),
        });
    }

    fn rejection(request_index: usize, source: PreparationRejection) -> SplicePreparationError {
        SplicePreparationError {
            request_index,
            source,
        }
    }
}

/// Commit a prepared transaction: adopt the allocator movement, then apply the
/// batches in order. Nothing here can fail; nothing partial ever commits.
pub(crate) fn commit_splice_plan(
    state: &mut EngineState,
    plan: SplicePlan,
    queue: &mut VecDeque<Event>,
) {
    state.adopt_allocators(plan.allocators);
    for prepared in plan.batches {
        apply_prepared_splice(state, prepared, queue);
    }
}

// ── The applicator ────────────────────────────────────────────────────────

/// Apply one prepared splice: mutate the live graph, record the batch, seed the
/// entries. Infallible — everything that can be refused was refused during
/// preparation, before this type could exist.
pub(crate) fn apply_prepared_splice(
    state: &mut EngineState,
    prepared: PreparedSplice,
    queue: &mut VecDeque<Event>,
) {
    let scopes = prepared.scopes.iter().map(|scope| scope.id).collect();
    for expr in prepared.exprs {
        state.graph.body.exprs.push(expr);
    }
    for scope in prepared.scopes {
        debug_assert_eq!(scope.id.index(), state.graph.scopes.len());
        state.graph.body.scopes.push(scope);
    }

    let batch = state.next_splice_batch();
    let mut batch_nodes = BTreeSet::new();
    let mut bindings = prepared.bindings;
    for node in prepared.nodes {
        let id = node.id;
        state.push_spliced_node(node, prepared.cancel_scope, batch, bindings.remove(&id));
        batch_nodes.insert(id);
    }
    for seed in &prepared.seeds {
        state.register_seed_edge(seed.edge, seed.entry);
        // The template's join already admitted the expansion, so a clone's
        // entry starts on its seed without applying that join again: one seed
        // token cannot satisfy a `Quorum { n >= 2 }`. Only the seed's
        // generation is forced; a loop inside the body joins as usual.
        state.force_entry(seed.entry, seed.generation);
    }
    for (target, group) in prepared.routing_extensions {
        if let Some(node) = state.graph.body.node_mut(target) {
            node.routing.groups.push(group);
        }
    }

    state.add_cancel_scope(
        prepared.cancel_scope,
        prepared.parent_scope,
        batch_nodes.clone(),
    );
    state.add_cancel_groups(prepared.cancel_scope, &batch_nodes);

    let mut retracted = Vec::new();
    for effect in &prepared.effects {
        match effect {
            SpliceEffect::Supersede(node) => state.supersede(*node),
            SpliceEffect::Retract(admission) => retracted.push(*admission),
        }
    }
    if !retracted.is_empty() {
        state.retract_admissions(&retracted);
    }

    // Batch identity is apply-time bookkeeping, like ownership: the id is the
    // record's position in the applied list, stamped here for both producers.
    state.push_splice(AppliedSplice {
        batch,
        owner: prepared.owner,
        nodes: batch_nodes,
        scopes,
        cancel_scope: prepared.cancel_scope,
        origin: prepared.origin,
        policy: prepared.policy,
        effects: prepared.effects,
        live_count: 0,
    });

    for seed in prepared.seeds {
        queue.push_back(Event::TokenEmitted {
            token: Token::seeded(seed.edge, seed.generation, seed.payload),
        });
    }
}

// ── Remapping helpers ─────────────────────────────────────────────────────

/// Append a synthesized expression to the batch's segment and return its live
/// id.
fn push_expr(exprs: &mut Vec<Expr>, expr_base: u32, expr: Expr) -> ExprId {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`ExprId` is a `u32`, so the expression table can never hold more entries \
                  than a `u32` counts"
    )]
    let id = ExprId::new(expr_base + exprs.len() as u32);
    exprs.push(expr);
    id
}

/// `nodes.<name>.status != null`: the record exists. What a final `depends_on`
/// reference lowers to — satisfied by the record that made it final, kept in
/// the graph so the dependency is visible and replayable.
fn record_exists_guard(exprs: &mut Vec<Expr>, expr_base: u32, name: &SmolStr) -> ExprId {
    let nodes = push_expr(exprs, expr_base, Expr::Var(SmolStr::new("nodes")));
    let record = push_expr(exprs, expr_base, Expr::Field(nodes, name.clone()));
    let status = push_expr(
        exprs,
        expr_base,
        Expr::Field(record, SmolStr::new("status")),
    );
    let null = push_expr(exprs, expr_base, Expr::Lit(Value::Null));
    push_expr(exprs, expr_base, Expr::Binary(BinOp::Ne, status, null))
}

/// Conjoin a guard onto a node's precondition.
fn conjoin_precondition(node: &mut Node, exprs: &mut Vec<Expr>, expr_base: u32, guard: ExprId) {
    node.precondition = Some(match node.precondition {
        None => guard,
        Some(existing) => push_expr(exprs, expr_base, Expr::Binary(BinOp::And, guard, existing)),
    });
}

/// Shift one fragment expression into the live table at `offset`.
fn shift_expr(expr: &Expr<Local>, offset: u32) -> Expr {
    let id = |e: ExprId<Local>| ExprId::new(e.raw() + offset);
    match expr {
        Expr::Lit(v) => Expr::Lit(v.clone()),
        Expr::Var(name) => Expr::Var(name.clone()),
        Expr::Field(base, field) => Expr::Field(id(*base), field.clone()),
        Expr::Index(base, index) => Expr::Index(id(*base), id(*index)),
        Expr::Unary(op, arg) => Expr::Unary(*op, id(*arg)),
        Expr::Binary(op, lhs, rhs) => Expr::Binary(*op, id(*lhs), id(*rhs)),
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => Expr::Cond {
            cond:      id(*cond),
            then:      id(*then),
            otherwise: id(*otherwise),
        },
        Expr::Array(items) => Expr::Array(items.iter().map(|i| id(*i)).collect()),
        Expr::Object(fields) => {
            Expr::Object(fields.iter().map(|(k, v)| (k.clone(), id(*v))).collect())
        }
        Expr::Call(name, args) => Expr::Call(name.clone(), args.iter().map(|a| id(*a)).collect()),
    }
}

fn remap_scope(scope: &Scope<Local>, live_id: ScopeId, expr_base: u32) -> Scope {
    let shift_env = |env: &BTreeMap<SmolStr, ExprOrValue<Local>>| {
        env.iter()
            .map(|(key, value)| {
                let value = match value {
                    ExprOrValue::Value(v) => ExprOrValue::Value(v.clone()),
                    ExprOrValue::Expr(id) => ExprOrValue::Expr(ExprId::new(id.raw() + expr_base)),
                };
                (key.clone(), value)
            })
            .collect()
    };
    // Exhaustive destructuring: a new `Scope` or `ServiceSpec` field fails to
    // compile here instead of silently taking its default in every spliced scope.
    let Scope {
        id: _,
        env,
        runtime,
        workspace,
        services,
    } = scope;
    let mut out = Scope::new(live_id);
    out.env = shift_env(env);
    out.runtime = runtime.clone();
    out.workspace = *workspace;
    out.services = services
        .iter()
        .map(|service| {
            let ir::ServiceSpec {
                name,
                image,
                env,
                options,
                credentials,
            } = service;
            let mut s = ir::ServiceSpec::new(name, image);
            s.env = shift_env(env);
            s.options.clone_from(options);
            s.credentials.clone_from(credentials);
            s
        })
        .collect();
    out
}

fn remap_node(
    source: &Node<Local>,
    node_base: u32,
    scope_map: &[ScopeId],
    expr_base: u32,
    alloc: &mut Allocators,
) -> Node {
    let shift = |id: ExprId<Local>| ExprId::new(id.raw() + expr_base);
    // Exhaustive destructuring: a new `Node` field fails to compile here instead
    // of silently taking its default in every spliced node.
    let Node {
        id,
        name,
        scope,
        step,
        join,
        precondition,
        routing,
        budget,
        retry,
        run_on_cancel,
        cancel_group,
        tolerates_failure,
        splice_policy,
        meta,
        // Stays `None`: fragments are executable IR, validated as such.
        expand: _,
    } = source;
    let mut node = Node::new(
        NodeId::new(node_base + id.raw()),
        name,
        scope_map[scope.index()],
        StepRef::new(
            step.kind.clone(),
            placeholder::map_expr_ids(&step.config, &mut |id| id + u64::from(expr_base)),
        ),
    );
    node.join = *join;
    node.precondition = precondition.map(shift);
    node.budget = *budget;
    node.retry = retry.clone();
    node.run_on_cancel = *run_on_cancel;
    node.cancel_group = cancel_group.map(|id| NodeId::new(node_base + id.raw()));
    node.tolerates_failure = *tolerates_failure;
    node.splice_policy = *splice_policy;
    node.meta = meta.clone();
    for group in &routing.groups {
        let mut edge_map = BTreeMap::new();
        let arms: Vec<Edge> = group
            .arms
            .iter()
            .map(|arm| {
                let id = alloc.take_edge();
                edge_map.insert(arm.id, id);
                Edge {
                    id,
                    to: NodeId::new(node_base + arm.to.raw()),
                    guard: match arm.guard {
                        Guard::Always => Guard::Always,
                        Guard::Expr(id) => Guard::Expr(shift(id)),
                    },
                    map: arm.map.map(shift),
                    back: arm.back,
                    weight: arm.weight,
                    label: arm.label.clone(),
                    transition: arm.transition,
                }
            })
            .collect();
        let policy = remap_selection_policy(
            &group.policy,
            |edge| {
                edge_map
                    .get(&edge)
                    .copied()
                    .unwrap_or_else(|| EdgeId::new(edge.raw()))
            },
            shift,
        );
        node.routing.groups.push(SelectGroup {
            policy,
            arms,
            fallthrough: group.fallthrough,
        });
    }
    node
}
