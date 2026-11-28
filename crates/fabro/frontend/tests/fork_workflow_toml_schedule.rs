//! Fork presence pin (denkhaus/work#1716, ADR-0052): the top-level
//! `[schedule]` block — where a project's workflow definition declares
//! when a scheduled fire is skipped — is a known top-level key of the
//! frontend validator, so run-create accepts the bundle instead of
//! refusing the key. The Fabro scheduler reads the block; nothing here
//! does. A `just vendor import` from a wrong fork state rewrites the tree
//! cleanly and would silently drop the entry — this file is the pin that
//! fails when that happens.

use std::collections::BTreeMap;

use frontend::{CompileInputs, MapFiles};
use frontend_fabro::load;

const SCHEDULE_DEFINITION: &str = r#"
_version = 1

[workflow]
graph = "workflow.fabro"

[[schedule.when_empty]]
kind = "labeled_pull_requests"
label = "merge:enabled"
"#;

#[test]
fn a_workflow_definition_with_a_schedule_block_validates() {
    let files = MapFiles(BTreeMap::from([(
        "workflow.toml".to_string(),
        SCHEDULE_DEFINITION.to_string(),
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
        "a `[schedule]` declaration must validate, refused: {refusals:?}"
    );
    assert!(
        lowered.graph.is_some(),
        "the bundle must lower: {:?}",
        lowered.diagnostics
    );
}
