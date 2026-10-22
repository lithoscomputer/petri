mod support;

use frontend::{CompileInputs, Frontend};
use frontend_fabro::Fabro;
use serde_json::json;
use support::{dot, files};

#[test]
fn removed_preparation_is_rejected_in_every_consumed_settings_layer() {
    let workflow = dot("work [shape=parallelogram, script=\"true\"]\nstart -> work -> exit");
    for source in ["workflow.toml", ".fabro/project.toml", "settings.toml"] {
        for syntax in [
            "[run.prepare]",
            "[run.prepare]\nsteps = []",
            "[run.prepare]\ntimeout = '5m'",
            "run.prepare = false",
            "run = { prepare = {} }",
            "['run'.'prepare']",
            "[[run.prepare.steps]]\nscript = 'secret-command-value'",
        ] {
            let mut inputs = CompileInputs::new();
            let bundle = if source == "settings.toml" {
                inputs
                    .vars
                    .insert(frontend_fabro::SETTINGS_HOOKS_VAR.into(), json!(syntax));
                files(&[])
            } else {
                files(&[(source, syntax)])
            };
            let lowered = Fabro::new().load("workflow.fabro", &workflow, &bundle, &inputs);
            assert!(
                lowered.graph.is_none(),
                "removed setting admitted in {source}"
            );
            let error = lowered
                .diagnostics
                .errors()
                .find(|d| d.code == "unsupported.workflow_toml.run.prepare")
                .expect("specific removal diagnostic");
            assert!(error.message.contains(source));
            assert!(
                error
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("command") && hint.contains("start"))
            );
            assert!(!error.message.contains("secret-command-value"));
        }
    }
    let lower = Fabro::new()
        .with_settings_toml(Some("run.prepare = {}".to_string()))
        .load(
            "workflow.fabro",
            &workflow,
            &files(&[("workflow.toml", "[run]\nprepare = { steps = [] }")]),
            &CompileInputs::new(),
        );
    assert_eq!(
        lower
            .diagnostics
            .errors()
            .filter(|d| d.code == "unsupported.workflow_toml.run.prepare")
            .count(),
        2
    );
}

#[test]
fn malformed_settings_fail_admission_without_echoing_source_values() {
    let workflow = dot("start -> exit");
    for source in ["workflow.toml", ".fabro/project.toml", "settings.toml"] {
        let text = "run.prepare = 'secret-value' broken";
        let bundle = if source == "settings.toml" {
            files(&[])
        } else {
            files(&[(source, text)])
        };
        let frontend = if source == "settings.toml" {
            Fabro::new().with_settings_toml(Some(text.to_string()))
        } else {
            Fabro::new()
        };
        let lowered = frontend.load("workflow.fabro", &workflow, &bundle, &CompileInputs::new());
        assert!(lowered.graph.is_none(), "invalid {source} must fail");
        for diagnostic in lowered.diagnostics.iter() {
            assert!(
                !diagnostic.message.contains("secret-value"),
                "diagnostics must omit source values"
            );
        }
        let syntax = lowered
            .diagnostics
            .errors()
            .find(|d| d.code == "fabro.settings.toml")
            .expect("the syntax error");
        assert_eq!(syntax.span.line, 1, "{syntax:?}");
        assert!(syntax.span.column > 1, "{syntax:?}");
    }
}

#[test]
fn preparation_mentions_are_not_settings_and_former_names_are_ordinary() {
    let workflow = dot(
        r#"run_prepare_1 [shape=parallelogram, script="echo run.prepare"]
        start -> run_prepare_1 -> exit"#,
    );
    let lowered = Fabro::new().load(
        "workflow.fabro",
        &workflow,
        &files(&[(
            "workflow.toml",
            "# [run.prepare]\n[run]\ngoal = 'explain run.prepare'",
        )]),
        &CompileInputs::new(),
    );
    assert!(lowered.graph.is_some(), "{:?}", lowered.diagnostics);
}
