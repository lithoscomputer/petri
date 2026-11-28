//! Fork presence pin (denkhaus/work#1716, ADR-0052): the `[run.grants]`
//! block a project's workflow definition declares for its runs sits in
//! the frontend validator's ignored run sections, so run-create accepts
//! the bundle instead of refusing the key. The standalone runner opens
//! no engine doors, so the section is inert here — but a refusal would
//! stop every workflow that declares a grant. A `just vendor import`
//! from a wrong fork state rewrites the tree cleanly and would silently
//! drop the entry — this file is the pin that fails when that happens.

use std::collections::BTreeMap;

use frontend::{CompileInputs, MapFiles};
use frontend_fabro::load;

/// A workflow definition carrying every key of the `[run.grants]` block.
const GRANTS_DEFINITION: &str = r#"
_version = 1

[workflow]
graph = "workflow.fabro"

[run.grants]
verdict_fields = ["closable"]
observation_triage = true
work_item_close = true
merge = true
resolve_push = true
"#;

#[test]
fn a_workflow_definition_with_declared_grants_validates() {
    let files = MapFiles(BTreeMap::from([(
        "workflow.toml".to_string(),
        GRANTS_DEFINITION.to_string(),
    )]));
    let text =
        "digraph T {\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n  start -> exit\n}\n";
    let lowered = load("w.fabro", text, &files, &CompileInputs::new());
    let refusals: Vec<String> = lowered
        .diagnostics
        .iter()
        .filter(|d| d.code == "unsupported.workflow_toml.key")
        .map(ToString::to_string)
        .collect();
    assert!(
        refusals.is_empty(),
        "a `[run.grants]` declaration must validate, refused: {refusals:?}"
    );
    assert!(
        lowered.graph.is_some(),
        "the bundle must lower: {:?}",
        lowered.diagnostics
    );
}
