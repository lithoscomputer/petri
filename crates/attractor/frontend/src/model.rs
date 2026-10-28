//! The semantic workflow: what the DOT text means once defaults, subgraph
//! classes and edge chains are applied. Mirrors Fabro's own semantic pass,
//! with positions kept for diagnostics.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use frontend::{Diagnostics, Span};

use crate::dot::{AstValue, AttrBlock, DotGraph, Ident, Statement};

/// A typed attribute value.
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl AttrValue {
    fn from_ast(value: &AstValue) -> Self {
        match value {
            AstValue::Str(s) | AstValue::Ident(s) => Self::Str(s.clone()),
            AstValue::Int(n) => Self::Int(*n),
            AstValue::Float(f) => Self::Float(*f),
            AstValue::Bool(b) => Self::Bool(*b),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value as text, however it was written.
    pub fn as_text(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(n) => n.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Str(_) => "a string",
            Self::Int(_) => "an integer",
            Self::Float(_) => "a number",
            Self::Bool(_) => "a boolean",
        }
    }
}

/// One attribute with the position of its key.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub value: AttrValue,
    pub span:  Span,
}

/// Attributes that together make one choice: a layer setting any of them
/// replaces all of them, so a node's `acp.config` never pairs with an
/// inherited `acp.command`.
const ONE_CHOICE: &[&str] = &["acp.command", "acp.config"];

/// An attribute map, with typed accessors that diagnose a wrong type instead
/// of ignoring it: Fabro reads a mistyped attribute as absent, and a value
/// that silently does nothing is exactly the failure a frontend must not
/// reproduce.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attrs {
    map: BTreeMap<String, Attr>,
}

impl Attrs {
    pub fn get(&self, key: &str) -> Option<&Attr> {
        self.map.get(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn insert(&mut self, key: &str, value: AttrValue, span: Span) {
        self.map.insert(key.to_string(), Attr { value, span });
    }

    pub fn remove(&mut self, key: &str) -> Option<Attr> {
        self.map.remove(key)
    }

    /// Clear what a layer setting `keys` replaces as a whole, before the
    /// layer's own values go in: both ACP agent attributes when it sets
    /// either.
    pub fn replace_choices<'k>(&mut self, keys: impl IntoIterator<Item = &'k str>) {
        if keys.into_iter().any(|key| ONE_CHOICE.contains(&key)) {
            for key in ONE_CHOICE {
                self.map.remove(*key);
            }
        }
    }

    /// Lay `layer` over these attributes: its values win, and a choice it
    /// makes replaces the inherited one whole.
    pub fn overlay(&mut self, layer: &Self) {
        self.replace_choices(layer.keys());
        for (key, attr) in layer.iter() {
            self.insert(key, attr.value.clone(), attr.span.clone());
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Attr)> {
        self.map.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// The position of `key`, or `fallback` when the attribute is absent.
    pub fn span_of(&self, key: &str, fallback: &Span) -> Span {
        self.map
            .get(key)
            .map_or_else(|| fallback.clone(), |a| a.span.clone())
    }

    /// A string attribute. A non-string value is diagnosed and read as absent.
    pub fn str(&self, key: &str, diags: &mut Diagnostics) -> Option<&str> {
        let attr = self.map.get(key)?;
        match &attr.value {
            AttrValue::Str(s) => Some(s),
            other => {
                wrong_type(diags, key, other, "a string", &attr.span);
                None
            }
        }
    }

    /// A string attribute, accepting any scalar as its text: a bare `LR`, a
    /// number or a boolean all read as the text they were written as.
    pub fn text(&self, key: &str) -> Option<String> {
        self.map.get(key).map(|a| a.value.as_text())
    }

    pub fn int(&self, key: &str, diags: &mut Diagnostics) -> Option<i64> {
        let attr = self.map.get(key)?;
        match &attr.value {
            AttrValue::Int(n) => Some(*n),
            AttrValue::Str(s) if s.trim().parse::<i64>().is_ok() => s.trim().parse().ok(),
            other => {
                wrong_type(diags, key, other, "an integer", &attr.span);
                None
            }
        }
    }

    pub fn bool(&self, key: &str, diags: &mut Diagnostics) -> Option<bool> {
        let attr = self.map.get(key)?;
        match &attr.value {
            AttrValue::Bool(b) => Some(*b),
            AttrValue::Str(s) if s == "true" => Some(true),
            AttrValue::Str(s) if s == "false" => Some(false),
            other => {
                wrong_type(diags, key, other, "a boolean", &attr.span);
                None
            }
        }
    }

    /// A duration attribute in Fabro's spelling: an integer followed by `ms`,
    /// `s`, `m`, `h` or `d`.
    pub fn duration(&self, key: &str, diags: &mut Diagnostics) -> Option<Duration> {
        let attr = self.map.get(key)?;
        let text = attr.value.as_text();
        let duration = parse_duration(&text);
        if duration.is_none() {
            diags.error(
                "attractor.bad_duration",
                attr.span.clone(),
                format!(
                    "`{key}` must be a duration such as `900s`, `15m`, `2h` or `250ms`, not \
                     `{text}`"
                ),
            );
        }
        duration
    }
}

fn wrong_type(diags: &mut Diagnostics, key: &str, got: &AttrValue, want: &str, span: &Span) {
    diags.error(
        "attractor.bad_attribute_type",
        span.clone(),
        format!("`{key}` must be {want}, not {}", got.kind()),
    );
}

/// Fabro's duration grammar: an unsigned integer and one unit.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if let Some(n) = text.strip_suffix("ms") {
        return n.parse::<u64>().ok().map(Duration::from_millis);
    }
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let number: u64 = number.parse().ok()?;
    let per_unit = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(Duration::from_millis(number.checked_mul(per_unit)?))
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeDecl {
    pub id:       String,
    pub attrs:    Attrs,
    /// From the `class` attribute and enclosing subgraph labels, first first.
    pub classes:  Vec<String>,
    pub span:     Span,
    /// Declared with a node statement, as opposed to named only by an edge.
    pub declared: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdgeDecl {
    pub from:    String,
    pub to:      String,
    pub attrs:   Attrs,
    /// The position of the `from` id.
    pub span:    Span,
    /// The position of the `to` id.
    pub to_span: Span,
}

/// The workflow, in declaration order.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    pub name:       String,
    pub attrs:      Attrs,
    pub nodes:      Vec<NodeDecl>,
    pub edges:      Vec<EdgeDecl>,
    pub span:       Span,
    node_index:     HashMap<String, usize>,
    outgoing_index: HashMap<String, Vec<usize>>,
    incoming_index: HashMap<String, Vec<usize>>,
}

impl Workflow {
    /// A workflow from its parts, with the indexes rebuilt: what an import
    /// expansion produces after it splices nodes and edges.
    pub fn from_parts(
        name: String,
        attrs: Attrs,
        nodes: Vec<NodeDecl>,
        edges: Vec<EdgeDecl>,
        span: Span,
    ) -> Self {
        let mut workflow = Self {
            name,
            attrs,
            nodes: Vec::new(),
            edges: Vec::new(),
            span,
            node_index: HashMap::new(),
            outgoing_index: HashMap::new(),
            incoming_index: HashMap::new(),
        };
        for node in nodes {
            workflow
                .node_index
                .insert(node.id.clone(), workflow.nodes.len());
            workflow.nodes.push(node);
        }
        for edge in edges {
            workflow.push_edge(edge);
        }
        workflow
    }

    pub fn node(&self, id: &str) -> Option<&NodeDecl> {
        self.node_index.get(id).map(|index| &self.nodes[*index])
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut NodeDecl> {
        let index = *self.node_index.get(id)?;
        self.nodes.get_mut(index)
    }

    pub fn outgoing(&self, id: &str) -> Vec<&EdgeDecl> {
        self.outgoing_index
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
            .collect()
    }

    pub fn incoming(&self, id: &str) -> Vec<&EdgeDecl> {
        self.incoming_index
            .get(id)
            .into_iter()
            .flatten()
            .map(|index| &self.edges[*index])
            .collect()
    }

    fn push_edge(&mut self, edge: EdgeDecl) {
        let index = self.edges.len();
        self.outgoing_index
            .entry(edge.from.clone())
            .or_default()
            .push(index);
        self.incoming_index
            .entry(edge.to.clone())
            .or_default()
            .push(index);
        self.edges.push(edge);
    }
}

/// Build the workflow from its AST.
pub fn build(dot: &DotGraph) -> Workflow {
    let mut declared = HashSet::new();
    collect_declared(&dot.statements, &mut declared);
    let mut state = Builder {
        workflow: Workflow {
            name:           dot.name.name.clone(),
            attrs:          Attrs::default(),
            nodes:          Vec::new(),
            edges:          Vec::new(),
            span:           dot.name.span.clone(),
            node_index:     HashMap::new(),
            outgoing_index: HashMap::new(),
            incoming_index: HashMap::new(),
        },
        declared,
        node_defaults: Attrs::default(),
        edge_defaults: Attrs::default(),
    };
    state.statements(&dot.statements, None);
    state.workflow
}

fn collect_declared(statements: &[Statement], out: &mut HashSet<String>) {
    for statement in statements {
        match statement {
            Statement::Node(node) => {
                out.insert(node.id.name.clone());
            }
            Statement::Subgraph(sub) => collect_declared(&sub.statements, out),
            _ => {}
        }
    }
}

struct Builder {
    workflow:      Workflow,
    declared:      HashSet<String>,
    node_defaults: Attrs,
    edge_defaults: Attrs,
}

impl Builder {
    fn ensure_node(&mut self, id: &Ident, declared: bool) -> &mut NodeDecl {
        let index = self
            .workflow
            .node_index
            .get(&id.name)
            .copied()
            .unwrap_or_else(|| {
                let index = self.workflow.nodes.len();
                self.workflow.nodes.push(NodeDecl {
                    id: id.name.clone(),
                    attrs: self.node_defaults.clone(),
                    classes: Vec::new(),
                    span: id.span.clone(),
                    declared,
                });
                self.workflow.node_index.insert(id.name.clone(), index);
                index
            });
        let node = &mut self.workflow.nodes[index];
        node.declared |= declared;
        node
    }

    fn apply_block(attrs: &mut Attrs, block: &AttrBlock) {
        attrs.replace_choices(block.iter().map(|attr| attr.key.as_str()));
        for attr in block {
            attrs.insert(
                &attr.key,
                AttrValue::from_ast(&attr.value),
                attr.span.clone(),
            );
        }
    }

    fn statements(&mut self, statements: &[Statement], subgraph_class: Option<&str>) {
        let saved_node = self.node_defaults.clone();
        let saved_edge = self.edge_defaults.clone();
        for statement in statements {
            match statement {
                Statement::GraphAttrs(block) => Self::apply_block(&mut self.workflow.attrs, block),
                Statement::GraphAttr(attr) => {
                    self.workflow.attrs.replace_choices([attr.key.as_str()]);
                    self.workflow.attrs.insert(
                        &attr.key,
                        AttrValue::from_ast(&attr.value),
                        attr.span.clone(),
                    );
                }
                Statement::NodeDefaults(block) => Self::apply_block(&mut self.node_defaults, block),
                Statement::EdgeDefaults(block) => Self::apply_block(&mut self.edge_defaults, block),
                Statement::Node(stmt) => {
                    let node = self.ensure_node(&stmt.id, true);
                    if let Some(block) = &stmt.attrs {
                        Self::apply_block(&mut node.attrs, block);
                    }
                    if let Some(class) = subgraph_class {
                        add_class(&mut node.classes, class);
                    }
                    if let Some(AttrValue::Str(classes)) =
                        node.attrs.get("class").map(|a| a.value.clone())
                    {
                        for class in split_classes(&classes) {
                            add_class(&mut node.classes, class);
                        }
                    }
                }
                Statement::Edge(stmt) => {
                    for id in &stmt.nodes {
                        let known = self.declared.contains(&id.name);
                        let node = self.ensure_node(id, false);
                        if known && let Some(class) = subgraph_class {
                            add_class(&mut node.classes, class);
                        }
                    }
                    let mut attrs = self.edge_defaults.clone();
                    if let Some(block) = &stmt.attrs {
                        Self::apply_block(&mut attrs, block);
                    }
                    for pair in stmt.nodes.windows(2) {
                        self.workflow.push_edge(EdgeDecl {
                            from:    pair[0].name.clone(),
                            to:      pair[1].name.clone(),
                            attrs:   attrs.clone(),
                            span:    pair[0].span.clone(),
                            to_span: pair[1].span.clone(),
                        });
                    }
                }
                Statement::Subgraph(sub) => {
                    let class = sub.statements.iter().find_map(|s| match s {
                        Statement::GraphAttr(attr) if attr.key == "label" => {
                            Some(class_from_label(&attr.value.as_text()))
                        }
                        Statement::GraphAttrs(block) => block
                            .iter()
                            .find(|a| a.key == "label")
                            .map(|a| class_from_label(&a.value.as_text())),
                        _ => None,
                    });
                    // A subgraph's label is a class for its members, never a
                    // graph attribute of the workflow: only the members see
                    // the subgraph's own `graph [...]` block.
                    let inner: Vec<Statement> = sub
                        .statements
                        .iter()
                        .filter(|s| {
                            !matches!(s, Statement::GraphAttr(_) | Statement::GraphAttrs(_))
                        })
                        .cloned()
                        .collect();
                    self.statements(&inner, class.as_deref().or(subgraph_class));
                }
            }
        }
        self.node_defaults = saved_node;
        self.edge_defaults = saved_edge;
    }
}

fn add_class(classes: &mut Vec<String>, class: &str) {
    let class = class.trim();
    if !class.is_empty() && !classes.iter().any(|c| c == class) {
        classes.push(class.to_string());
    }
}

/// Classes are separated by whitespace; commas are accepted too.
fn split_classes(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').flat_map(str::split_whitespace)
}

/// A subgraph label as a class name: lowercase, spaces to dashes, nothing but
/// ASCII alphanumerics and dashes.
fn class_from_label(label: &str) -> String {
    label
        .to_lowercase()
        .chars()
        .map(|c| if c == ' ' { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dot::parse;

    fn workflow(text: &str) -> Workflow {
        build(&parse("w.fabro", text).unwrap_or_else(|d| panic!("{d}")))
    }

    #[test]
    fn chains_expand_and_defaults_apply() {
        let w = workflow(
            r#"digraph G {
                node [timeout="900s"]
                a [label="A", timeout="10s"]
                b
                a -> b -> c [weight=2]
            }"#,
        );
        assert_eq!(w.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), [
            "a", "b", "c"
        ]);
        assert_eq!(w.edges.len(), 2);
        assert_eq!(
            w.edges[1].attrs.get("weight").map(|a| a.value.clone()),
            Some(AttrValue::Int(2))
        );
        let mut diags = Diagnostics::new();
        assert_eq!(
            w.node("a")
                .expect("a")
                .attrs
                .duration("timeout", &mut diags),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            w.node("b")
                .expect("b")
                .attrs
                .duration("timeout", &mut diags),
            Some(Duration::from_secs(900))
        );
        assert!(!w.node("c").expect("c").declared);
        assert!(diags.is_empty());
    }

    #[test]
    fn subgraph_labels_become_classes_and_scope_defaults() {
        let w = workflow(
            r#"digraph G {
                subgraph cluster_a {
                    label = "Loop A"
                    node [thread_id="loop-a"]
                    x [class="verify, hard"]
                }
                y
            }"#,
        );
        let x = w.node("x").expect("x");
        assert_eq!(x.classes, ["loop-a", "verify", "hard"]);
        assert!(x.attrs.contains("thread_id"));
        assert!(!w.node("y").expect("y").attrs.contains("thread_id"));
        assert!(!w.attrs.contains("label"));
    }

    #[test]
    fn durations_follow_the_fabro_grammar() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_duration("900"), None);
        assert_eq!(parse_duration("1.5s"), None);
    }
}
