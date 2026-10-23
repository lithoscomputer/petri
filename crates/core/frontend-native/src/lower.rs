//! Native document → HIR.

use std::collections::{HashMap, HashSet};
use std::mem;

use frontend::diag::{Diagnostics, Lowered, Span};
use frontend::expr::lower::{LowerError, Roots, strict};
use frontend::expr::{Segment, parse, split_template};
use frontend::yaml::{Document, Mapping, Node};
use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, Backoff, Budget, ExpandTarget, ExprId, ExprOrValue, ExprTable, Fallthrough, GraphBuilder,
    JoinPolicy, NodeId, RetryOn, RetryPolicy, RuntimeSpec, Scope, ScopeId, StatusKind, StepRef,
    WorkspacePolicy,
};
use serde_json::{Map, Value};
use smol_str::SmolStr;

use crate::duration;
use crate::model::{
    ARM_KEYS, BACKOFF_KEYS, BUDGET_KEYS, FOR_EACH_KEYS, KNOWN_BINDINGS, NODE_KEYS, RETRY_KEYS,
    RETRY_ON_KEYS, SCOPE_KEYS, TOP_KEYS,
};

/// Resolves identifiers for the native format: engine bindings by name, with
/// `item` / `index` rewritten to the loop-state shape inside a sequential body.
struct NativeRoots {
    /// Inside a sequential `for_each` body, the current element lives on the
    /// token.
    sequential_body: bool,
    /// Only `params` may appear here (scope env).
    params_only:     bool,
}

impl Roots for NativeRoots {
    fn root(&mut self, name: &str, table: &mut ExprTable) -> Option<ExprId> {
        if self.params_only && name != "params" {
            return None;
        }
        if !KNOWN_BINDINGS.contains(&name) {
            return None;
        }
        if self.sequential_body {
            match name {
                "item" => {
                    let items = table.path("input", &["items"]);
                    let idx = table.path("input", &["idx"]);
                    return Some(table.index(items, idx));
                }
                "index" => return Some(table.path("input", &["idx"])),
                _ => {}
            }
        }
        Some(table.var(name))
    }
}

struct Ctx<'a> {
    diags:             Diagnostics,
    b:                 GraphBuilder,
    ids:               HashMap<String, NodeId>,
    spans:             HashMap<NodeId, Span>,
    scope_ids:         HashMap<String, ScopeId>,
    /// Nodes that are the body of a sequential `for_each`, so their expressions
    /// get the loop-state rewrite.
    sequential_bodies: HashSet<NodeId>,
    _doc:              &'a Document,
}

pub fn lower(doc: &Document, diags: Diagnostics) -> Lowered {
    let mut ctx = Ctx {
        diags,
        b: GraphBuilder::bare(),
        ids: HashMap::new(),
        spans: HashMap::new(),
        scope_ids: HashMap::new(),
        sequential_bodies: HashSet::new(),
        _doc: doc,
    };
    let root = doc.root();
    let Some(top) = root.expect_mapping(&mut ctx.diags, "the document") else {
        return Lowered::rejected(ctx.diags);
    };
    top.reject_unknown_keys(TOP_KEYS, &mut ctx.diags, "the document");

    ctx.params(top.get("params"));
    ctx.scopes(top.get("scopes"));

    let Some(nodes) = top.get("nodes") else {
        ctx.diags.error(
            "native.no_nodes",
            top.span(),
            "a workflow needs a `nodes:` mapping",
        );
        return Lowered::rejected(ctx.diags);
    };
    let Some(nodes) = nodes.expect_mapping(&mut ctx.diags, "`nodes`") else {
        return Lowered::rejected(ctx.diags);
    };

    // Pass 1: names and ids, in document order, so forward references resolve.
    for (name, node) in nodes.iter() {
        let Some(mapping) = node.expect_mapping(&mut ctx.diags, &format!("node `{name}`")) else {
            continue;
        };
        let scope = ctx.scope_of(&mapping, name);
        let id = ctx
            .b
            .add_node(name, scope, StepRef::new("noop", Value::Null));
        ctx.ids.insert(name.to_string(), id);
        ctx.spans.insert(id, node.span());
    }

    // Pass 2: which nodes sit inside a sequential loop body, before any expression
    // in them is lowered.
    for (name, node) in nodes.iter() {
        let Some(mapping) = node.as_mapping() else {
            continue;
        };
        let Some(fe) = mapping.get("for_each").and_then(|n| n.as_mapping()) else {
            continue;
        };
        let parallel = fe
            .get("parallel")
            .and_then(|n| n.as_scalar())
            .and_then(|s| s.as_bool())
            .unwrap_or(true);
        if parallel {
            continue;
        }
        let head = ctx.ids[name];
        let tail = fe
            .get("until")
            .and_then(|n| n.as_str())
            .and_then(|t| ctx.ids.get(t).copied())
            .unwrap_or(head);
        for id in ctx.chain(head, tail, &nodes) {
            ctx.sequential_bodies.insert(id);
        }
    }

    // Pass 3: node bodies.
    for (name, node) in nodes.iter() {
        let Some(mapping) = node.as_mapping() else {
            continue;
        };
        let id = ctx.ids[name];
        ctx.node_body(id, name, &mapping);
    }

    // Pass 4: routing, then loops (which rewrite routing on their source and tail).
    for (name, node) in nodes.iter() {
        let Some(mapping) = node.as_mapping() else {
            continue;
        };
        let id = ctx.ids[name];
        ctx.routing(id, name, &mapping);
    }
    for (name, node) in nodes.iter() {
        let Some(mapping) = node.as_mapping() else {
            continue;
        };
        if let Some(fe) = mapping.get("for_each") {
            let id = ctx.ids[name];
            ctx.for_each(id, name, fe, &nodes);
        }
    }

    if let Some(entry) = top.get("entry")
        && let Some(seq) = entry.expect_sequence(&mut ctx.diags, "`entry`")
    {
        for item in seq.iter() {
            match item.as_str().and_then(|n| ctx.ids.get(n)) {
                Some(id) => ctx.b.mark_entry(*id),
                None => ctx.diags.error(
                    "native.unknown_node",
                    item.span(),
                    format!(
                        "`entry` names an unknown node `{}`",
                        item.as_str().unwrap_or("?")
                    ),
                ),
            }
        }
    }

    if ctx.diags.has_errors() {
        return Lowered::rejected(ctx.diags);
    }

    let builder = mem::replace(&mut ctx.b, GraphBuilder::bare());
    let mut graph = builder.build();
    // Normalize, don't relax: `Quorum{1}` on a loop head becomes `Any`.
    graph.normalize_loop_heads();
    let report = ir::check(&graph);
    ctx.diags.extend_from_report(
        &report,
        |node| ctx.spans.get(&node).cloned(),
        &Span::file(doc.file()),
    );
    Lowered::from_parts(graph, ctx.diags)
}

impl Ctx<'_> {
    // ── Top level ──────────────────────────────────────────────────────────

    fn params(&mut self, node: Option<Node<'_>>) {
        let Some(node) = node else { return };
        if node.expect_mapping(&mut self.diags, "`params`").is_none() {
            return;
        }
        let value = node.to_json();
        // Defaults baked into the graph; a host overrides them before the run.
        self.b
            .graph_mut()
            .params
            .insert(SmolStr::new("params"), value);
    }

    fn scopes(&mut self, node: Option<Node<'_>>) {
        let Some(node) = node else {
            let id = self.b.add_scope(Scope::new(ScopeId::new(0)));
            self.scope_ids.insert("main".to_string(), id);
            return;
        };
        let Some(mapping) = node.expect_mapping(&mut self.diags, "`scopes`") else {
            return;
        };
        for (name, spec) in mapping.iter() {
            let Some(spec) = spec.expect_mapping(&mut self.diags, &format!("scope `{name}`"))
            else {
                continue;
            };
            spec.reject_unknown_keys(SCOPE_KEYS, &mut self.diags, &format!("scope `{name}`"));
            let mut scope = Scope::new(ScopeId::new(0));

            if spec.get("runtime").is_some() && spec.get("source").is_some() {
                self.diags.error(
                    "native.conflicting_source",
                    spec.span(),
                    "use either `source` or the legacy `runtime`, not both",
                );
            }
            if let Some(runtime) = spec.get("runtime") {
                scope.runtime = self.runtime(runtime);
            }
            if let Some(source) = spec.get("source") {
                scope.runtime = self.source(source);
            }
            if let Some(services) = spec.get("services") {
                scope.services = self.services(services);
            }
            if let Some(reqs) = spec.get("requirements").and_then(|n| n.as_sequence()) {
                scope.runtime.requirements = reqs
                    .iter()
                    .filter_map(|r| r.as_str())
                    .map(SmolStr::new)
                    .collect();
            }
            if let Some(ws) = spec.get("workspace").and_then(|n| n.as_str()) {
                scope.workspace = match ws {
                    "shared" => WorkspacePolicy::Shared,
                    "per_node" => WorkspacePolicy::PerNode,
                    other => {
                        self.diags.error(
                            "native.bad_workspace",
                            spec.get("workspace").map(|n| n.span()).unwrap_or_default(),
                            format!("workspace must be `shared` or `per_node`, not `{other}`"),
                        );
                        WorkspacePolicy::Shared
                    }
                };
            }
            if let Some(env) = spec.get("env")
                && let Some(env) = env.expect_mapping(&mut self.diags, "scope env")
            {
                for (key, value) in env.iter() {
                    let lowered = self.scalar_or_expr(value, true);
                    scope.env.insert(SmolStr::new(key), lowered);
                }
            }
            if let Some(grace) = spec.get("grace") {
                // Grace is a driver knob per scope; the IR has no field for it yet.
                self.diags.warning(
                    "native.grace_ignored",
                    grace.span(),
                    "per-scope `grace` is not carried by the IR; the driver's run-wide grace applies",
                );
            }
            let id = self.b.add_scope(scope);
            self.scope_ids.insert(name.to_string(), id);
        }
        if self.scope_ids.is_empty() {
            let id = self.b.add_scope(Scope::new(ScopeId::new(0)));
            self.scope_ids.insert("main".to_string(), id);
        }
    }

    fn source(&mut self, node: Node<'_>) -> RuntimeSpec {
        if node.as_str() == Some("host_directory") {
            return RuntimeSpec::host_process();
        }
        if let Some(source) = node.as_mapping() {
            source.reject_unknown_keys(&["image"], &mut self.diags, "`source`");
            if let Some(image) = source.get("image").and_then(|image| image.as_str())
                && !image.trim().is_empty()
            {
                return RuntimeSpec::container(image);
            }
        }
        self.diags.error("native.bad_source", node.span(),
            "`source` must be `host_directory` or `{ image: <nonempty image> }`; other sources require the shared sandbox API");
        RuntimeSpec::host_process()
    }

    fn services(&mut self, node: Node<'_>) -> Vec<ir::ServiceSpec> {
        let Some(services) = node.expect_mapping(&mut self.diags, "`services`") else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for (name, value) in services.iter() {
            let Some(service) = value.expect_mapping(&mut self.diags, "service") else {
                continue;
            };
            service.reject_unknown_keys(&["image", "env"], &mut self.diags, "service");
            if name.is_empty()
                || name.len() > 63
                || name.starts_with('-')
                || name.ends_with('-')
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            {
                self.diags.error(
                    "native.bad_service",
                    value.span(),
                    "service names must be lowercase DNS labels",
                );
            }
            let Some(image) = service
                .get("image")
                .and_then(|image| image.as_str())
                .filter(|image| !image.trim().is_empty())
            else {
                self.diags.error(
                    "native.bad_service",
                    value.span(),
                    "a service needs a nonempty `image`",
                );
                continue;
            };
            let mut lowered = ir::ServiceSpec::new(name, image);
            if let Some(env) = service
                .get("env")
                .and_then(|env| env.expect_mapping(&mut self.diags, "service env"))
            {
                for (key, value) in env.iter() {
                    lowered
                        .env
                        .insert(SmolStr::new(key), self.scalar_or_expr(value, true));
                }
            }
            result.push(lowered);
        }
        result
    }

    fn runtime(&mut self, node: Node<'_>) -> RuntimeSpec {
        const SHAPE: &str = "runtime must be `host` or `{ container: { image: … } }`";
        if let Some(text) = node.as_str() {
            return match text {
                "host" => RuntimeSpec::host_process(),
                other => {
                    self.diags.error(
                        "native.bad_runtime",
                        node.span(),
                        format!("{SHAPE}, not `{other}`"),
                    );
                    RuntimeSpec::host_process()
                }
            };
        }
        let Some(mapping) = node.expect_mapping(&mut self.diags, "`runtime`") else {
            return RuntimeSpec::host_process();
        };
        mapping.reject_unknown_keys(&["container"], &mut self.diags, "`runtime`");
        let Some(container) = mapping.get("container") else {
            self.diags.error("native.bad_runtime", node.span(), SHAPE);
            return RuntimeSpec::host_process();
        };
        let Some(container) = container.expect_mapping(&mut self.diags, "`container`") else {
            return RuntimeSpec::host_process();
        };
        // Only the image: what runs the container, and with which flags, is the
        // executor's configuration, not the workflow's.
        container.reject_unknown_keys(&["image"], &mut self.diags, "`container`");
        let Some(image) = container.get("image").and_then(|n| n.as_str()) else {
            self.diags.error(
                "native.bad_runtime",
                container.span(),
                "`container` needs an `image`",
            );
            return RuntimeSpec::host_process();
        };
        RuntimeSpec::container(image)
    }

    fn scope_of(&mut self, node: &Mapping<'_>, name: &str) -> ScopeId {
        match node.get("scope") {
            None => *self.scope_ids.values().min().expect("at least one scope"),
            Some(s) => {
                let text = s.as_str().unwrap_or("");
                if let Some(id) = self.scope_ids.get(text) {
                    *id
                } else {
                    self.diags.error(
                        "native.unknown_scope",
                        s.span(),
                        format!("node `{name}` names an unknown scope `{text}`"),
                    );
                    ScopeId::new(0)
                }
            }
        }
    }

    // ── Nodes ──────────────────────────────────────────────────────────────

    fn node_body(&mut self, id: NodeId, name: &str, m: &Mapping<'_>) {
        m.reject_unknown_keys(NODE_KEYS, &mut self.diags, &format!("node `{name}`"));
        let in_loop = self.sequential_bodies.contains(&id);

        // Step kind and config.
        let has_run = m.contains_key("run");
        let kind = match m.get("step").and_then(|n| n.as_str()) {
            Some("noop") => "noop",
            Some("process") => "process",
            Some(other) => {
                self.diags.error(
                    "native.unknown_step",
                    m.get("step").map(|n| n.span()).unwrap_or_default(),
                    format!("unknown step kind `{other}`; known kinds are `noop` and `process`"),
                );
                "noop"
            }
            None if has_run => "process",
            None => "noop",
        };
        let mut config = match m.get("config") {
            Some(c) => match self.config_value(c, in_loop) {
                Value::Object(map) => map,
                other => {
                    let mut map = Map::new();
                    map.insert("value".into(), other);
                    map
                }
            },
            None => Map::new(),
        };
        if let Some(run) = m.get("run") {
            config.insert("run".into(), self.config_value(run, in_loop));
        }
        if let Some(shell) = m.get("shell") {
            config.insert("shell".into(), self.config_value(shell, in_loop));
        }
        if kind == "process" && !config.contains_key("run") {
            self.diags.error(
                "native.no_run",
                m.span(),
                format!("process node `{name}` needs `run:`"),
            );
        }
        let config = if config.is_empty() {
            Value::Null
        } else {
            Value::Object(config)
        };
        self.b.node_mut(id).step = StepRef::new(kind, config);

        if let Some(join) = m.get("join") {
            let policy = self.join(join);
            self.b.set_join(id, policy);
        }
        if let Some(cond) = m.get("if")
            && let Some(expr) = self.condition(cond, in_loop)
        {
            self.b.set_precondition(id, expr);
        }
        if let Some(budget) = m.get("budget") {
            let b = self.budget(budget);
            self.b.set_budget(id, b);
        }
        if let Some(retry) = m.get("retry") {
            let policy = self.retry(retry);
            self.b.node_mut(id).retry = policy;
        }
    }

    fn join(&mut self, node: Node<'_>) -> JoinPolicy {
        if let Some(text) = node.as_str() {
            return match text {
                "all" => JoinPolicy::All,
                "any" => JoinPolicy::Any,
                other => {
                    self.diags.error(
                        "native.bad_join",
                        node.span(),
                        format!("join must be `all`, `any` or `{{ quorum: n }}`, not `{other}`"),
                    );
                    JoinPolicy::All
                }
            };
        }
        let n = node
            .as_mapping()
            .and_then(|m| m.get("quorum"))
            .and_then(|q| q.as_scalar())
            .and_then(|s| s.as_i64());
        match n.and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n >= 1 => JoinPolicy::Quorum { n },
            _ => {
                self.diags.error(
                    "native.bad_join",
                    node.span(),
                    "join must be `all`, `any` or `{ quorum: n }` with n >= 1",
                );
                JoinPolicy::All
            }
        }
    }

    fn budget(&mut self, node: Node<'_>) -> Budget {
        let mut budget = Budget::once();
        let Some(m) = node.expect_mapping(&mut self.diags, "`budget`") else {
            return budget;
        };
        m.reject_unknown_keys(BUDGET_KEYS, &mut self.diags, "`budget`");
        if let Some(n) = m.get("max_firings") {
            match n
                .as_scalar()
                .and_then(|s| s.as_i64())
                .and_then(|v| u32::try_from(v).ok())
            {
                Some(v) if v >= 1 => budget.max_firings = v,
                _ => self.diags.error(
                    "native.bad_budget",
                    n.span(),
                    "`max_firings` must be an integer >= 1",
                ),
            }
        }
        if let Some(t) = m.get("timeout") {
            match t.as_str().and_then(duration::parse) {
                Some(d) => budget.timeout = d,
                None => self.diags.error(
                    "native.bad_duration",
                    t.span(),
                    "`timeout` must be a duration like `30s`, `10m` or `2h`",
                ),
            }
        }
        budget
    }

    fn retry(&mut self, node: Node<'_>) -> RetryPolicy {
        let mut policy = RetryPolicy::none();
        let Some(m) = node.expect_mapping(&mut self.diags, "`retry`") else {
            return policy;
        };
        m.reject_unknown_keys(RETRY_KEYS, &mut self.diags, "`retry`");
        if let Some(n) = m.get("max_attempts") {
            match n
                .as_scalar()
                .and_then(|s| s.as_i64())
                .and_then(|v| u32::try_from(v).ok())
            {
                Some(v) if v >= 1 => policy = RetryPolicy::attempts(v),
                _ => self.diags.error(
                    "native.bad_retry",
                    n.span(),
                    "`max_attempts` must be an integer >= 1",
                ),
            }
        }
        if let Some(b) = m.get("backoff")
            && let Some(bm) = b.expect_mapping(&mut self.diags, "`backoff`")
        {
            {
                bm.reject_unknown_keys(BACKOFF_KEYS, &mut self.diags, "`backoff`");
                let mut backoff = Backoff::default();
                if let Some(d) = bm.get("initial") {
                    match d.as_str().and_then(duration::parse) {
                        Some(v) => backoff.initial = v,
                        None => self.diags.error(
                            "native.bad_duration",
                            d.span(),
                            "`initial` must be a duration",
                        ),
                    }
                }
                if let Some(d) = bm.get("max") {
                    match d.as_str().and_then(duration::parse) {
                        Some(v) => backoff.max = v,
                        None => self.diags.error(
                            "native.bad_duration",
                            d.span(),
                            "`max` must be a duration",
                        ),
                    }
                }
                if let Some(f) = bm.get("factor") {
                    match f
                        .as_scalar()
                        .and_then(|s| s.as_f64().or_else(|| s.as_i64().map(|i| i as f64)))
                    {
                        Some(v) if v >= 1.0 => backoff.factor = v,
                        _ => self.diags.error(
                            "native.bad_retry",
                            f.span(),
                            "`factor` must be a number >= 1",
                        ),
                    }
                }
                if let Some(j) = bm.get("jitter") {
                    match j.as_scalar().and_then(|s| s.as_bool()) {
                        Some(v) => backoff.jitter = v,
                        None => self.diags.error(
                            "native.bad_retry",
                            j.span(),
                            "`jitter` must be true or false",
                        ),
                    }
                }
                policy.backoff = backoff;
            }
        }
        if let Some(r) = m.get("retry_on")
            && let Some(rm) = r.expect_mapping(&mut self.diags, "`retry_on`")
        {
            {
                rm.reject_unknown_keys(RETRY_ON_KEYS, &mut self.diags, "`retry_on`");
                let mut on = RetryOn {
                    statuses:        Vec::new(),
                    failure_classes: Vec::new(),
                };
                if let Some(list) = rm.get("statuses").and_then(|n| n.as_sequence()) {
                    for item in list.iter() {
                        match item.as_str() {
                            Some("failure") => on.statuses.push(StatusKind::Failure),
                            Some("timed_out") => on.statuses.push(StatusKind::TimedOut),
                            Some("cancelled") => on.statuses.push(StatusKind::Cancelled),
                            Some(other) => self.diags.error(
                                "native.bad_retry",
                                item.span(),
                                format!("`{other}` is not a retryable status; use failure, timed_out or cancelled"),
                            ),
                            None => self.diags.error("native.bad_retry", item.span(), "status must be a name"),
                        }
                    }
                }
                if let Some(list) = rm.get("classes").and_then(|n| n.as_sequence()) {
                    on.failure_classes = list
                        .iter()
                        .filter_map(|c| c.as_str())
                        .map(ir::FailureClass::new)
                        .collect();
                }
                policy.retry_on = on;
            }
        }
        if let Some(e) = m.get("on_exhaustion") {
            match e.as_str() {
                Some("fail") => {}
                Some("accept_partial") => policy = policy.accepting_partial(),
                _ => self.diags.error(
                    "native.bad_retry",
                    e.span(),
                    "`on_exhaustion` must be `fail` or `accept_partial`",
                ),
            }
        }
        policy
    }

    // ── Routing ────────────────────────────────────────────────────────────

    fn routing(&mut self, id: NodeId, name: &str, m: &Mapping<'_>) {
        let in_loop = self.sequential_bodies.contains(&id);
        let present: Vec<&str> = ["next", "select", "parallel"]
            .into_iter()
            .filter(|k| m.contains_key(k))
            .collect();
        if present.len() > 1 {
            self.diags.error(
                "native.routing_conflict",
                m.span(),
                format!(
                    "node `{name}` has {}; a node routes in exactly one of these ways",
                    present.join(" and ")
                ),
            );
            return;
        }
        if let Some(next) = m.get("next") {
            if let Some(arm) = self.arm(next, in_loop, true) {
                self.b.select(id, vec![arm]);
            }
        } else if let Some(select) = m.get("select") {
            let (arms_node, fallthrough) = match select.as_mapping() {
                Some(sm) => {
                    let ft = match sm.get("fallthrough").and_then(|n| n.as_str()) {
                        Some("error") => Fallthrough::Error,
                        Some("no_emit") | None => Fallthrough::NoEmit,
                        Some(other) => {
                            self.diags.error(
                                "native.bad_select",
                                select.span(),
                                format!("fallthrough must be `error` or `no_emit`, not `{other}`"),
                            );
                            Fallthrough::NoEmit
                        }
                    };
                    (sm.get("arms"), ft)
                }
                None => (Some(select), Fallthrough::NoEmit),
            };
            let Some(arms_node) = arms_node else {
                self.diags
                    .error("native.bad_select", select.span(), "`select` needs `arms:`");
                return;
            };
            if let Some(arms) = self.arms(arms_node, in_loop) {
                self.b.select_with(id, arms, fallthrough);
            }
        } else if let Some(parallel) = m.get("parallel") {
            let Some(seq) = parallel.expect_sequence(&mut self.diags, "`parallel`") else {
                return;
            };
            let mut groups = Vec::new();
            for group in seq.iter() {
                if group.is_sequence() {
                    if let Some(arms) = self.arms(group, in_loop) {
                        groups.push(arms);
                    }
                } else if let Some(arm) = self.arm(group, in_loop, false) {
                    groups.push(vec![arm]);
                }
            }
            self.b.fan_out_groups(id, groups);
        }
    }

    fn arms(&mut self, node: Node<'_>, in_loop: bool) -> Option<Vec<Arm>> {
        let seq = node.expect_sequence(&mut self.diags, "select arms")?;
        let mut arms = Vec::new();
        let count = seq.len();
        for (i, arm) in seq.iter().enumerate() {
            let Some(arm) = self.arm(arm, in_loop, false) else {
                continue;
            };
            if arm.guard.is_none() && i + 1 < count {
                self.diags.error(
                    "native.unguarded_arm_not_last",
                    seq.span(),
                    "an arm without `when:` matches always, so it must be the last arm of its group",
                );
            }
            arms.push(arm);
        }
        Some(arms)
    }

    /// One arm: a bare node id, or `{ to, when, map, back }`.
    fn arm(&mut self, node: Node<'_>, in_loop: bool, plain_next: bool) -> Option<Arm> {
        if let Some(target) = node.as_str() {
            let to = self.node_ref(target, node.span())?;
            return Some(Arm::always(to));
        }
        let m = node.expect_mapping(&mut self.diags, "an arm")?;
        m.reject_unknown_keys(ARM_KEYS, &mut self.diags, "an arm");
        let to_node = m.get("to")?;
        let to = self.node_ref(to_node.as_str().unwrap_or(""), to_node.span())?;
        let mut arm = Arm::always(to);
        if let Some(when) = m.get("when") {
            if plain_next {
                self.diags.error(
                    "native.bad_next",
                    when.span(),
                    "`next:` is unconditional; use `select:` for a guarded arm",
                );
            }
            if let Some(expr) = self.condition(when, in_loop) {
                arm.guard = Some(expr);
            }
        }
        if let Some(map) = m.get("map")
            && let Some(expr) = self.expression(map, in_loop)
        {
            arm.map = Some(expr);
        }
        if let Some(back) = m.get("back") {
            match back.as_scalar().and_then(|s| s.as_bool()) {
                Some(true) => arm.back = true,
                Some(false) => {}
                None => self.diags.error(
                    "native.bad_arm",
                    back.span(),
                    "`back` must be true or false",
                ),
            }
        }
        Some(arm)
    }

    fn node_ref(&mut self, name: &str, span: Span) -> Option<NodeId> {
        if let Some(id) = self.ids.get(name) {
            Some(*id)
        } else {
            self.diags.error(
                "native.unknown_node",
                span,
                format!("unknown node `{name}`"),
            );
            None
        }
    }

    // ── for_each ───────────────────────────────────────────────────────────

    fn for_each(&mut self, head: NodeId, name: &str, node: Node<'_>, nodes: &Mapping<'_>) {
        let Some(m) = node.expect_mapping(&mut self.diags, "`for_each`") else {
            return;
        };
        m.reject_unknown_keys(FOR_EACH_KEYS, &mut self.diags, "`for_each`");
        let Some(items_node) = m.get("items") else {
            self.diags.error(
                "native.bad_for_each",
                node.span(),
                "`for_each` needs `items:`",
            );
            return;
        };
        let parallel = match m
            .get("parallel")
            .map(|p| p.as_scalar().and_then(|s| s.as_bool()))
        {
            None => true,
            Some(Some(b)) => b,
            Some(None) => {
                self.diags.error(
                    "native.bad_for_each",
                    node.span(),
                    "`parallel` must be true or false",
                );
                return;
            }
        };
        let tail = match m.get("until") {
            None => head,
            Some(u) => match self.node_ref(u.as_str().unwrap_or(""), u.span()) {
                Some(t) => t,
                None => return,
            },
        };

        if parallel {
            // Items are evaluated in the head's own firing context, before it is
            // cloned; `item` and `index` are meaningless there.
            let Some(items) = self.expression(items_node, false) else {
                return;
            };
            let max_parallel = m
                .get("max_parallel")
                .and_then(|n| n.as_scalar())
                .and_then(|s| s.as_i64())
                .map(|n| u32::try_from(n.max(1)).unwrap_or(u32::MAX));
            let fail_fast = m
                .get("fail_fast")
                .and_then(|n| n.as_scalar())
                .and_then(|s| s.as_bool())
                .unwrap_or(false);
            let target = if tail == head {
                ExpandTarget::Node
            } else {
                ExpandTarget::Subgraph {
                    entry: head,
                    exit:  tail,
                }
            };
            ir::parallel_for_each(&mut self.b, head, items, target, max_parallel, fail_fast);
            return;
        }

        // Sequential: the desugar needs the node that feeds the head, the tail, and
        // where the tail exits to.
        let source = {
            let feeders: Vec<NodeId> = self
                .b
                .graph()
                .nodes
                .iter()
                .filter(|n| n.routing.edges().any(|e| e.to == head && !e.back))
                .map(|n| n.id)
                .collect();
            match feeders.as_slice() {
                [one] => *one,
                [] => {
                    self.diags.error(
                        "native.bad_for_each",
                        node.span(),
                        format!("sequential `for_each` on `{name}` needs exactly one node whose `next:` feeds it; found none"),
                    );
                    return;
                }
                many => {
                    self.diags.error(
                        "native.bad_for_each",
                        node.span(),
                        format!("sequential `for_each` on `{name}` needs exactly one feeding node; found {}", many.len()),
                    );
                    return;
                }
            }
        };
        let source_routing = self
            .b
            .graph()
            .node(source)
            .map(|n| n.routing.clone())
            .unwrap_or_default();
        if source_routing.groups.len() != 1 || source_routing.groups[0].arms.len() != 1 {
            self.diags.error(
                "native.bad_for_each",
                node.span(),
                "the node feeding a sequential `for_each` must reach it with a plain `next:`",
            );
            return;
        }
        let tail_routing = self
            .b
            .graph()
            .node(tail)
            .map(|n| n.routing.clone())
            .unwrap_or_default();
        let collector = match tail_routing.groups.as_slice() {
            [g] if g.arms.len() == 1 && !g.arms[0].back => g.arms[0].to,
            _ => {
                self.diags.error(
                    "native.bad_for_each",
                    node.span(),
                    "the tail of a sequential `for_each` must exit with a plain `next:` to the node that collects the results",
                );
                return;
            }
        };
        let max_iterations = m
            .get("max_iterations")
            .and_then(|n| n.as_scalar())
            .and_then(|s| s.as_i64())
            .map_or(100, |n| u32::try_from(n.max(1)).unwrap_or(u32::MAX));
        // `items` is evaluated on the source's outcome, where `output` is the
        // source's output.
        let Some(items) = self.expression(items_node, false) else {
            return;
        };
        let _ = nodes;
        ir::sequential_for_each_over(
            &mut self.b,
            source,
            head,
            tail,
            collector,
            max_iterations,
            items,
        );
        // Every node in the body is capped like the head and tail.
        for id in self.chain(head, tail, nodes) {
            if id != head && id != tail {
                self.b.set_budget(id, Budget::looped(max_iterations));
            }
        }
    }

    /// The nodes from `head` to `tail` along plain `next:` edges, as declared.
    fn chain(&self, head: NodeId, tail: NodeId, nodes: &Mapping<'_>) -> Vec<NodeId> {
        let mut out = vec![head];
        let mut current = head;
        let mut guard = 0;
        while current != tail && guard < 10_000 {
            guard += 1;
            let name = self
                .b
                .graph()
                .node(current)
                .map(|n| n.name.to_string())
                .unwrap_or_default();
            let next = nodes
                .get(&name)
                .and_then(|n| n.as_mapping())
                .and_then(|m| m.get("next"))
                .and_then(|n| {
                    n.as_str().map(str::to_string).or_else(|| {
                        n.as_mapping()
                            .and_then(|m| m.get("to"))
                            .and_then(|t| t.as_str().map(str::to_string))
                    })
                })
                .and_then(|n| self.ids.get(&n).copied());
            match next {
                Some(id) => {
                    out.push(id);
                    current = id;
                }
                None => break,
            }
        }
        out
    }

    // ── Expressions ────────────────────────────────────────────────────────

    /// An `if:` or `when:`: a bare expression, or a single whole `${{ }}`.
    fn condition(&mut self, node: Node<'_>, in_loop: bool) -> Option<ExprId> {
        let scalar = node.expect_scalar(&mut self.diags, "a condition")?;
        if let Some(b) = scalar.as_bool() {
            return Some(self.b.exprs().lit(b));
        }
        let text = scalar.as_str();
        let Ok(segments) = split_template(text) else {
            self.diags
                .error("expr.unterminated", node.span(), "unterminated `${{`");
            return None;
        };
        let source = match segments.as_slice() {
            [Segment::Expr { source, .. }] => source.clone(),
            _ if !text.contains("${{") => text.to_string(),
            _ => {
                self.diags.error(
                    "expr.mixed_condition",
                    node.span(),
                    "a condition is one expression: either bare, or a single `${{ }}`",
                );
                return None;
            }
        };
        self.lower_source(&source, node.span(), in_loop)
    }

    /// A `map:` or `items:` value: same shape as a condition.
    fn expression(&mut self, node: Node<'_>, in_loop: bool) -> Option<ExprId> {
        self.condition(node, in_loop)
    }

    fn lower_source(&mut self, source: &str, span: Span, in_loop: bool) -> Option<ExprId> {
        let ast = match parse(source) {
            Ok(ast) => ast,
            Err(e) => {
                self.diags.error(
                    "expr.parse",
                    span,
                    format!("could not parse expression `{}`: {e}", source.trim()),
                );
                return None;
            }
        };
        let mut roots = NativeRoots {
            sequential_body: in_loop,
            params_only:     false,
        };
        match strict(&ast, self.b.exprs(), &mut roots) {
            Ok(id) => Some(id),
            Err(LowerError::UnknownIdent(name)) => {
                self.diags.push(
                    frontend::Diagnostic::error(
                        "expr.unknown_binding",
                        span,
                        format!("`{name}` is not a binding this format knows"),
                    )
                    .with_hint(format!("known bindings: {}", KNOWN_BINDINGS.join(", "))),
                );
                None
            }
            Err(e) => {
                self.diags.error("expr.lower", span, e.to_string());
                None
            }
        }
    }

    /// A scope env value: a literal, or an expression that may only read
    /// `params`.
    fn scalar_or_expr(&mut self, node: Node<'_>, params_only: bool) -> ExprOrValue {
        let Some(text) = node.as_str() else {
            return ExprOrValue::Value(node.to_json());
        };
        if !text.contains("${{") {
            return ExprOrValue::Value(node.to_json());
        }
        match self.template(text, node.span(), params_only, false) {
            Some(id) => ExprOrValue::Expr(id),
            None => ExprOrValue::Value(Value::Null),
        }
    }

    /// A config value: templated strings become `{"$expr": id}` placeholders.
    fn config_value(&mut self, node: Node<'_>, in_loop: bool) -> Value {
        if let Some(m) = node.as_mapping() {
            let mut out = Map::new();
            for (k, v) in m.iter() {
                out.insert(k.to_string(), self.config_value(v, in_loop));
            }
            return Value::Object(out);
        }
        if let Some(s) = node.as_sequence() {
            return Value::Array(s.iter().map(|n| self.config_value(n, in_loop)).collect());
        }
        let Some(text) = node.as_str() else {
            return Value::Null;
        };
        if !text.contains("${{") {
            return node.to_json();
        }
        match self.template(text, node.span(), false, in_loop) {
            Some(id) => serde_json::json!({ EXPR_PLACEHOLDER_KEY: id.raw() }),
            None => Value::Null,
        }
    }

    /// Lower a `${{ }}`-bearing string. A lone `${{ e }}` is `e` with its type;
    /// mixed text is a string concatenation.
    fn template(
        &mut self,
        text: &str,
        span: Span,
        params_only: bool,
        in_loop: bool,
    ) -> Option<ExprId> {
        let Ok(segments) = split_template(text) else {
            self.diags
                .error("expr.unterminated", span, "unterminated `${{`");
            return None;
        };
        let mut pieces: Vec<ExprId> = Vec::new();
        let whole = matches!(segments.as_slice(), [Segment::Expr { .. }]);
        for segment in &segments {
            match segment {
                Segment::Text(t) => {
                    let id = self.b.exprs().lit(t.as_str());
                    pieces.push(id);
                }
                Segment::Expr { source, .. } => {
                    let ast = match parse(source) {
                        Ok(ast) => ast,
                        Err(e) => {
                            self.diags.error(
                                "expr.parse",
                                span.clone(),
                                format!("could not parse expression `{}`: {e}", source.trim()),
                            );
                            return None;
                        }
                    };
                    let mut roots = NativeRoots {
                        sequential_body: in_loop,
                        params_only,
                    };
                    let id = match strict(&ast, self.b.exprs(), &mut roots) {
                        Ok(id) => id,
                        Err(LowerError::UnknownIdent(name)) => {
                            let hint = if params_only {
                                "scope env may only read `params`".to_string()
                            } else {
                                format!("known bindings: {}", KNOWN_BINDINGS.join(", "))
                            };
                            self.diags.push(
                                frontend::Diagnostic::error(
                                    "expr.unknown_binding",
                                    span.clone(),
                                    format!("`{name}` is not a binding this format knows"),
                                )
                                .with_hint(hint),
                            );
                            return None;
                        }
                        Err(e) => {
                            self.diags.error("expr.lower", span.clone(), e.to_string());
                            return None;
                        }
                    };
                    if whole {
                        return Some(id);
                    }
                    let as_string = self.b.exprs().call("to_string", vec![id]);
                    pieces.push(as_string);
                }
            }
        }
        // Fold into one string with `++`.
        let mut iter = pieces.into_iter();
        let mut acc = iter.next()?;
        for next in iter {
            acc = self.b.exprs().binary(ir::BinOp::Concat, acc, next);
        }
        Some(acc)
    }
}
