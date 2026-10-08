//! After routing: the goal gate in front of `exit`, back-edge
//! classification, joins and budgets.

use std::collections::HashSet;

use ir::validate::loop_reachable;
use ir::{Budget, Edge, EdgeId, ExprId, JoinPolicy, NodeId, Routing, StepRef};
use serde_json::{Value, json};
use smol_str::SmolStr;

use super::{Ctx, Kind, MAX_FIRINGS, STRUCTURAL_TIMEOUT};
use crate::kinds::GOAL_CHECK_NODE;
use crate::model::{NodeDecl, Workflow};

impl Ctx<'_> {
    /// The `goal_check` node in front of `exit`, when any node is a goal
    /// gate: every gate must have a success-like record, or the run routes
    /// back to the gate's retry target — the first that exists of the node's
    /// `retry_target`, its `fallback_retry_target`, the graph's, and the
    /// graph's fallback — and a gate with no target ends the run failed.
    /// Every later pass routes an edge to the exit through it.
    pub(super) fn insert_goal_check(&mut self, workflow: &Workflow) {
        let mut gates: Vec<&NodeDecl> = Vec::new();
        for node in &workflow.nodes {
            if node
                .attrs
                .bool("goal_gate", &mut self.diags)
                .unwrap_or(false)
            {
                gates.push(node);
            }
        }
        if gates.is_empty() {
            return;
        }
        gates.sort_by(|a, b| a.id.cmp(&b.id));
        let exit = self.exit();
        let exit_span = self.spans[&exit].clone();
        let check = self.b.add_node(
            GOAL_CHECK_NODE,
            self.scope,
            StepRef::new("noop", Value::Null),
        );
        self.spans.insert(check, exit_span.clone());
        self.b.set_meta(
            check,
            json!({
                "label": "Goal check",
                "shape": "diamond",
                "kind": "goal_check",
                "classes": [],
                "synthetic": true,
            }),
        );

        let mut arms = Vec::new();
        let mut all_ok: Option<ExprId> = None;
        for gate in &gates {
            let ok = {
                let record = self.b.exprs().path("nodes", &[&gate.id, "success_like"]);
                let falsy = self.b.exprs().lit(false);
                self.b.exprs().call("default", vec![record, falsy])
            };
            all_ok = Some(match all_ok {
                None => ok,
                Some(acc) => self.b.exprs().binary(ir::BinOp::And, acc, ok),
            });
            let failing = self.b.exprs().unary(ir::UnOp::Not, ok);
            let target = [
                gate.attrs.text("retry_target"),
                gate.attrs.text("fallback_retry_target"),
                workflow.attrs.text("retry_target"),
                workflow.attrs.text("fallback_retry_target"),
            ]
            .into_iter()
            .flatten()
            .find(|t| self.nodes.contains_key(t));
            match target {
                Some(target) => {
                    let id = self.b.next_edge_id();
                    let mut edge = Edge::when(id, self.nodes[&target].id, failing);
                    edge.back = true;
                    edge.label = Some(SmolStr::new(format!("goal_gate:{}", gate.id)));
                    arms.push(edge);
                }
                None => self.diags.warning(
                    "attractor.goal_gate_without_target",
                    gate.span.clone(),
                    format!(
                        "goal gate `{}` has no retry target that exists; when it fails the run ends failed",
                        gate.id
                    ),
                ),
            }
        }
        let all_ok = all_ok.expect("at least one gate");
        let id = self.b.next_edge_id();
        arms.push(Edge::when(id, exit, all_ok));
        self.b.node_mut(check).routing = Routing::select(arms);
        self.goal_check = Some(check);
    }

    /// A depth-first search from `start` over the lowered edges marks every
    /// cycle-closing edge `back`.
    pub(super) fn back_edges(&mut self, start: NodeId) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        struct Frame {
            node:  NodeId,
            next:  usize,
            edges: Vec<(EdgeId, NodeId)>,
        }
        let count = self.b.graph().nodes.len();
        let mut color = vec![Color::White; count];
        let mut back: HashSet<EdgeId> = HashSet::new();
        let successors = |graph: &ir::Graph, node: NodeId| -> Vec<(EdgeId, NodeId)> {
            graph
                .node(node)
                .map(|n| n.routing.edges().map(|e| (e.id, e.to)).collect())
                .unwrap_or_default()
        };
        let mut stack = vec![Frame {
            node:  start,
            next:  0,
            edges: successors(self.b.graph(), start),
        }];
        color[start.index()] = Color::Gray;
        while let Some(frame) = stack.last_mut() {
            if frame.next >= frame.edges.len() {
                color[frame.node.index()] = Color::Black;
                stack.pop();
                continue;
            }
            let (edge, to) = frame.edges[frame.next];
            frame.next += 1;
            match color[to.index()] {
                Color::Gray => {
                    back.insert(edge);
                }
                Color::White => {
                    color[to.index()] = Color::Gray;
                    stack.push(Frame {
                        node:  to,
                        next:  0,
                        edges: successors(self.b.graph(), to),
                    });
                }
                Color::Black => {}
            }
        }
        for node in &mut self.b.graph_mut().body.nodes {
            for group in &mut node.routing.groups {
                for arm in &mut group.arms {
                    if back.contains(&arm.id) {
                        arm.back = true;
                    }
                }
            }
        }
    }

    pub(super) fn joins_and_budgets(&mut self, workflow: &Workflow) {
        let looped = loop_reachable(&self.b.graph().body);
        let global = workflow
            .attrs
            .int("max_node_visits", &mut self.diags)
            .filter(|n| *n > 0);
        if let Some(limit) = global
            && limit > i64::from(MAX_FIRINGS)
        {
            self.diags.error(
                "attractor.max_visits_too_large",
                workflow.attrs.span_of("max_node_visits", &workflow.span),
                format!("`max_node_visits={limit}` exceeds the hard maximum of {MAX_FIRINGS}"),
            );
        }
        for node in &workflow.nodes {
            let res = self.nodes[&node.id];
            let join = if res.kind == Kind::FanIn {
                JoinPolicy::All
            } else {
                JoinPolicy::Any
            };
            self.b.set_join(res.id, join);
            let visits = node
                .attrs
                .int("max_visits", &mut self.diags)
                .filter(|n| *n > 0);
            if let Some(limit) = visits
                && limit > i64::from(MAX_FIRINGS)
            {
                self.diags.error(
                    "attractor.max_visits_too_large",
                    node.attrs.span_of("max_visits", &node.span),
                    format!("`max_visits={limit}` exceeds the hard maximum of {MAX_FIRINGS}"),
                );
            }
            if !looped.contains(&res.id) {
                continue;
            }
            let explicit = [visits, global]
                .into_iter()
                .flatten()
                .filter_map(|n| u32::try_from(n).ok())
                .min();
            let max_firings = explicit.map_or(MAX_FIRINGS, |n| n.min(MAX_FIRINGS));
            if explicit.is_none() && !self.budget_defaulted {
                self.budget_defaulted = true;
                self.diags.warning(
                    "info.budget.default",
                    node.span.clone(),
                    format!(
                        "`{}` is in a loop with no `max_visits`; Fabro's unlimited visits lower to \
                         the hard maximum of {MAX_FIRINGS} firings",
                        node.id
                    ),
                );
            }
            let budget = self
                .b
                .graph()
                .node(res.id)
                .map_or_else(|| Budget::new(1, STRUCTURAL_TIMEOUT), |n| n.budget);
            self.b.set_budget(res.id, Budget {
                max_firings,
                ..budget
            });
        }
        // The synthetic goal check, when it exists, loops too.
        if let Some(check) = self.goal_check {
            self.b.set_join(check, JoinPolicy::Any);
            if looped.contains(&check) {
                let limit = global
                    .and_then(|n| u32::try_from(n).ok())
                    .map_or(MAX_FIRINGS, |n| n.min(MAX_FIRINGS));
                self.b
                    .set_budget(check, Budget::new(limit, STRUCTURAL_TIMEOUT));
            }
        }
    }
}
