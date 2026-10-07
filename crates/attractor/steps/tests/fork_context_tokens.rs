//! Fork feature pins (fabro-e71b): `{{ context.NAME }}` resolves in a
//! second, narrow pass at stage dispatch — prompts and agent prompts
//! against the node's visible context projection (exactly the `## Context`
//! rows), command scripts against the run context without the
//! rendered-dedup. Strict: an unresolved token fails the stage
//! (`context_token`) naming the token and the visible keys.

use std::sync::Arc;
use std::time::Duration;

use attractor_steps::fork_preamble_policy::{
    PreamblePolicy, PreamblePolicyHandle, PreamblePolicySource,
};
use attractor_steps::pebble::PebbleClient;
use attractor_steps::register;
use execution::host::{self, HostRun};
use ir::{Graph, RunStatus};
use pebble_coding_agent::test_support::{
    ScriptedCompletion, ScriptedProvider, client_from, message_text, text_response,
};
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of};

fn lower(text: &str) -> Graph {
    let lowered = frontend_attractor::load("test.fabro", text, &NoFiles, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("valid workflow")
}

fn dot(body: &str) -> String {
    format!(
        r#"digraph T {{
        graph [default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        {body}
    }}"#
    )
}

/// A policy source that answers one fixed policy for every node.
struct FixedPolicy(PreamblePolicy);

impl PreamblePolicySource for FixedPolicy {
    fn policy(&self, _node: &str) -> PreamblePolicy {
        self.0.clone()
    }
}

async fn run(
    label: &str,
    body: &str,
    policy: Option<PreamblePolicy>,
) -> (ExecutionReport, Arc<ScriptedProvider>) {
    let dir = RunDir::new(label);
    let (client, provider) = client_from(
        ScriptedProvider::new(Vec::new())
            .completing(vec![ScriptedCompletion::response(text_response("done."))]),
    );
    let mut runtime = register(Runtime::standard())
        .capability(PebbleClient(client))
        .options({
            let mut options = RunOptions::new(dir.path());
            options.grace = Duration::from_millis(100);
            options.retention = Retention::Never;
            options.echo = false;
            options
        });
    if let Some(policy) = policy {
        runtime = runtime.capability(PreamblePolicyHandle(Arc::new(FixedPolicy(policy))));
    }
    let graph = lower(&dot(body));
    let report = host::run_configured(&runtime, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    (report, provider)
}

/// The seeding stage: a routing command that writes the run context.
const SEEDER: &str = r#"a [shape=parallelogram, output_schema="routing", script="echo '{\"context_updates\": {\"seed_id\": \"fabro-1234\", \"mode\": \"fast\"}}'"]"#;

/// prompt-resolve: a prompt's `{{ context.NAME }}` token resolves against
/// the run context a prior stage wrote — the sent prompt carries the value,
/// not the token.
#[tokio::test]
async fn a_prompt_resolves_context_tokens_from_the_run_context() {
    let body = SEEDER.to_owned()
        + r#"
        p [shape=tab, prompt="Work {{ context.seed_id }} now"]
        start -> a -> p -> exit
    "#;
    let (report, provider) = run("ctx-prompt-resolve", &body, None).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let sent = message_text(&provider.completion_requests()[0].messages()[0]);
    assert!(sent.contains("Work fabro-1234 now"), "{sent}");
    assert!(!sent.contains("{{ context"), "the token resolved: {sent}");
}

/// prompt-strict: an unresolved token fails the stage with the
/// `context_token` class, naming the token and the visible keys — a typo
/// never renders empty.
#[tokio::test]
async fn an_unresolved_prompt_token_fails_the_stage_naming_it() {
    let body = SEEDER.to_owned()
        + r#"
        p [shape=tab, prompt="Work {{ context.seed_id_typo }} now", on_failure="exit"]
        start -> a -> p -> exit
    "#;
    let (report, _provider) = run("ctx-prompt-strict", &body, None).await;
    assert_ne!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "p");
    assert_eq!(output["failure_class"], json!("context_token"), "{output}");
    let reason = output["failure_reason"].as_str().expect("reason");
    assert!(reason.contains("`{{ context.seed_id_typo }}`"), "{reason}");
    assert!(
        reason.contains("seed_id") && reason.contains("mode"),
        "names the visible keys: {reason}"
    );
}

/// command-resolve: a script's `{{ context.NAME }}` token resolves before
/// the process spawns — the command consumes the value.
#[tokio::test]
async fn a_script_resolves_context_tokens_before_the_spawn() {
    let body = SEEDER.to_owned()
        + r#"
        c [shape=parallelogram, script="echo seed={{ context.seed_id }}"]
        start -> a -> c -> exit
    "#;
    let (report, _provider) = run("ctx-command-resolve", &body, None).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "c");
    assert_eq!(output["stdout"], json!("seed=fabro-1234\n"), "{output}");
}

/// command-strict: an unresolved token in a script fails the stage with
/// the `context_token` class.
#[tokio::test]
async fn an_unresolved_script_token_fails_the_stage() {
    let body = SEEDER.to_owned()
        + r#"
        c [shape=parallelogram, script="echo {{ context.nope }}", on_failure="exit"]
        start -> a -> c -> exit
    "#;
    let (report, _provider) = run("ctx-command-strict", &body, None).await;
    assert_ne!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "c");
    assert_eq!(output["failure_class"], json!("context_token"), "{output}");
    assert!(
        output["failure_reason"]
            .as_str()
            .expect("reason")
            .contains("`{{ context.nope }}`"),
        "{output}"
    );
}

/// allow-key-envelope: the token resolves ONLY against the node's
/// `x.context_allow_keys` projection — a key the policy hides is not
/// resolvable even though the run context carries it.
#[tokio::test]
async fn the_allow_key_envelope_bounds_the_token_projection() {
    let body = SEEDER.to_owned()
        + r#"
        p [shape=tab, prompt="Work {{ context.mode }}", on_failure="exit"]
        start -> a -> p -> exit
    "#;
    let policy = PreamblePolicy {
        context_allow_keys: Some(vec!["seed_id".to_owned()]),
        ..PreamblePolicy::default()
    };
    let (report, _provider) = run("ctx-allow-keys", &body, Some(policy)).await;
    assert_ne!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "p");
    assert_eq!(output["failure_class"], json!("context_token"), "{output}");
    let reason = output["failure_reason"].as_str().expect("reason");
    assert!(reason.contains("`{{ context.mode }}`"), "{reason}");
    assert!(
        reason.ends_with("visible context keys: seed_id"),
        "the visible keys are the allowed projection: {reason}"
    );
}
