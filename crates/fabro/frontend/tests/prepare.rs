//! Preparation is selected as a whole section before it is lowered.

mod support;

use std::time::Duration;

use frontend::{CompileInputs, Frontend, Lowered};
use frontend_attractor::kinds::COMMAND_KIND;
use frontend_fabro::{Fabro, SETTINGS_FILE};
use serde_json::json;
use support::{dot, files, node, tiers};

const SERVER_PREPARE: &str = r#"
[run.prepare]
timeout = "17s"
steps = [{ script = "echo server one" }, { script = "echo server two" }]
"#;

fn lower(server: Option<&str>, workflow: Option<&str>, inputs: &CompileInputs) -> Lowered {
    let entries = workflow.map(|text| ("wf/workflow.toml", text));
    Fabro::new()
        .with_settings_toml(server.map(str::to_owned))
        .load(
            "wf/workflow.fabro",
            &dot(r#"a [shape=parallelogram, script="echo work"]
                start -> a -> exit"#),
            &files(entries.as_slice()),
            inputs,
        )
}

fn graph(server: Option<&str>, workflow: Option<&str>, inputs: &CompileInputs) -> ir::Graph {
    let lowered = lower(server, workflow, inputs);
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("valid preparation lowers")
}

fn prepare_count(graph: &ir::Graph) -> usize {
    graph
        .nodes
        .iter()
        .filter(|node| node.name.starts_with("run_prepare_"))
        .count()
}

#[test]
fn server_preparation_runs_before_work_with_or_without_workflow_settings() {
    for workflow in [None, Some("[run.inputs]\nmode = 'fast'")] {
        let graph = graph(Some(SERVER_PREPARE), workflow, &CompileInputs::new());
        assert_eq!(prepare_count(&graph), 2, "{workflow:?}");
        assert_eq!(
            tiers(&graph, "start")[0].1[0].0,
            "run_prepare_1",
            "{workflow:?}"
        );
        assert_eq!(tiers(&graph, "run_prepare_1")[0].1[0].0, "run_prepare_2");
        assert_eq!(tiers(&graph, "run_prepare_2")[0].1[0].0, "a");
        for (name, script) in [
            ("run_prepare_1", "echo server one"),
            ("run_prepare_2", "echo server two"),
        ] {
            let step = node(&graph, name);
            assert_eq!(step.step.kind, COMMAND_KIND);
            assert_eq!(step.step.config["script"], json!(script));
            assert_eq!(step.step.config["on_failure"], json!("exit"));
            assert_eq!(step.budget.timeout, Duration::from_secs(17));
        }
    }
}

#[test]
fn workflow_preparation_replaces_server_steps_and_timeout() {
    for (timeout, expected) in [("timeout = '41s'", 41), ("", 300)] {
        let workflow =
            format!("[run.prepare]\n{timeout}\nsteps = [{{ script = 'echo workflow' }}]");
        let graph = graph(Some(SERVER_PREPARE), Some(&workflow), &CompileInputs::new());
        assert_eq!(prepare_count(&graph), 1, "{workflow}");
        assert_eq!(tiers(&graph, "start")[0].1[0].0, "run_prepare_1");
        assert_eq!(tiers(&graph, "run_prepare_1")[0].1[0].0, "a");
        let step = node(&graph, "run_prepare_1");
        assert_eq!(step.step.config["script"], json!("echo workflow"));
        assert_eq!(
            step.budget.timeout,
            Duration::from_secs(expected),
            "{workflow}"
        );
    }
}

#[test]
fn explicit_empty_preparation_disables_inherited_steps() {
    for workflow in [
        "[run.prepare]",
        "[run.prepare]\nsteps = []",
        "[run.prepare]\ntimeout = '2s'",
    ] {
        let graph = graph(Some(SERVER_PREPARE), Some(workflow), &CompileInputs::new());
        assert_eq!(prepare_count(&graph), 0, "{workflow}");
        assert_eq!(tiers(&graph, "start")[0].1[0].0, "a");
    }
}

#[test]
fn absent_preparation_leaves_the_start_route_unchanged() {
    for workflow in [None, Some("[run.inputs]\nmode = 'fast'")] {
        let graph = graph(None, workflow, &CompileInputs::new());
        assert_eq!(prepare_count(&graph), 0, "{workflow:?}");
        assert_eq!(tiers(&graph, "start")[0].1[0].0, "a", "{workflow:?}");
    }
}

#[test]
fn inherited_preparation_renders_workflow_defaults_and_preserves_argv_env_and_secrets() {
    let server = r#"
[run.prepare]
steps = [
    { script = "echo {{ inputs.target }} {{ vars.mode }}", env = { TARGET = "{{ inputs.target }}", TOKEN = "{{ secrets.PREPARE_TOKEN }}" } },
    { command = ["printf", "%s", "{{ inputs.target }}", "it's quoted", "$(touch unwanted); *"] },
]
"#;
    for (supplied, expected) in [
        (None, "two words"),
        (Some("override value"), "override value"),
    ] {
        let mut inputs = CompileInputs::new();
        inputs.vars.insert("mode".into(), json!("fast"));
        if let Some(value) = supplied {
            inputs.inputs.insert("target".into(), json!(value));
        }
        let graph = graph(
            Some(server),
            Some("[run.inputs]\ntarget = 'two words'"),
            &inputs,
        );
        assert_eq!(prepare_count(&graph), 2, "{supplied:?}");
        let first = node(&graph, "run_prepare_1");
        assert_eq!(
            first.step.config["script"],
            json!(format!("echo {expected} fast"))
        );
        assert_eq!(first.step.config["env"]["TARGET"], json!(expected));
        assert_eq!(
            first.step.config["env"]["TOKEN"],
            json!({ "$secret": "PREPARE_TOKEN" })
        );
        let second = node(&graph, "run_prepare_2");
        let script = second.step.config["script"]
            .as_str()
            .expect("command script");
        assert_eq!(shlex::split(script).expect("safe shell words"), [
            "printf",
            "%s",
            expected,
            "it's quoted",
            "$(touch unwanted); *"
        ]);
        assert_eq!(second.budget.timeout, Duration::from_secs(300));
    }
}

#[test]
fn malformed_selected_preparation_is_rejected_at_its_source() {
    let malformed = [
        ("[run]\nprepare = false", "run.prepare"),
        (
            "[run.prepare]\nsteps = [{ script = 'true', command = false }]",
            "run.prepare.steps[0]",
        ),
        ("[run.prepare]\nsteps = 'bad'", "run.prepare.steps"),
        ("[run.prepare]\ntimeout = 'bad'", "run.prepare.timeout"),
        ("[run.prepare]\nsteps = [42]", "run.prepare.steps[0]"),
        (
            "[run.prepare]\nsteps = [{ script = 'true', command = ['false'] }]",
            "run.prepare.steps[0]",
        ),
        (
            "[run.prepare]\nsteps = [{ command = ['echo', 42] }]",
            "run.prepare.steps[0].command[1]",
        ),
        (
            "[run.prepare]\nsteps = [{ script = '{{ inputs.missing }}' }]",
            "run.prepare.steps[0].script",
        ),
        (
            "[run.prepare]\nsteps = [{ script = '{{ secrets.TOKEN }}' }]",
            "run.prepare.steps[0].script",
        ),
        (
            "[run.prepare]\nsteps = [{ script = 'true', env = { TOKEN = 'prefix {{ secrets.TOKEN }}' } }]",
            "run.prepare.steps[0].env.TOKEN",
        ),
        (
            "[run.prepare]\nsteps = [{ script = 'true', env = 42 }]",
            "run.prepare.steps[0].env",
        ),
        ("[run.prepare]\nunknown = true", "run.prepare.unknown"),
        ("[run.prepare]\nsteps = [", "not valid TOML"),
    ];
    for (text, message) in malformed {
        for (server, workflow, source) in [
            (Some(text), None, SETTINGS_FILE),
            (Some(SERVER_PREPARE), Some(text), "wf/workflow.toml"),
        ] {
            let lowered = lower(server, workflow, &CompileInputs::new());
            assert!(lowered.graph.is_none(), "malformed {source}: {text}");
            assert!(
                lowered.diagnostics.iter().any(|diag| diag.is_error()
                    && diag.span.file == source
                    && diag.message.contains(message)),
                "expected {message} at {source}: {:?}",
                lowered.diagnostics
            );
        }
    }
}

#[test]
fn workflow_override_does_not_parse_or_render_discarded_server_preparation() {
    for server in [
        "[run.prepare]\ntimeout = 'invalid'\nsteps = [{ script = '{{ inputs.missing }}' }]",
        "[run]\nprepare = false",
    ] {
        let graph = graph(
            Some(server),
            Some("[run.prepare]\nsteps = [{ script = 'echo workflow' }]"),
            &CompileInputs::new(),
        );
        assert_eq!(prepare_count(&graph), 1, "{server}");
        assert_eq!(
            node(&graph, "run_prepare_1").step.config["script"],
            json!("echo workflow")
        );
    }
}
