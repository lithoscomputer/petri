//! Model fallback through the public event contract: a host that consumes
//! `RunEvent`s alone can rebuild a stage's fallback plan, the routes it ran
//! on, the failover decision with its typed error, the accounting per route,
//! and the terminal outcome. The plan is Petri's own event
//! (`attractor.fallback.plan`); every other fact is Pebble's, on the
//! `agent_activity` stream: `SessionStarted` names each route, `RouteFailover`
//! the move with the failed route's usage and the typed error,
//! `RouteFailoverStopped` the stop and its reason, `AssistantMessage` each
//! answer's usage. The finished step's metrics carry the stage's usage and
//! its breakdown by route (`pebble.usage_by_model`). The stage is a real
//! native agent on a scripted model client; the chain comes from
//! `workflow.toml`.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use lithos_llm::types::ErrorKind;
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedFailure, scripted_client, text_response, tool_call_response,
};
use petri::attractor::fallback::PLAN_EVENT;
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::engine::Event;
use petri::execution::events::{CollectingSink, EventProjector, RunEvent};
use petri::execution::host::{self, HostRun};
use petri::execution::{CoordinatorEvent, ExecutionObserver};
use petri::executor::Retention;
use petri::frontend::CompileInputs;
use petri::frontend::fabro::Fabro;
use petri::ir::{RunStatus, Value};
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, backend_event};

const WORKFLOW: &str = r#"digraph Fallback {
    graph [backend="api", goal="Answer"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello.", model="model", provider="test", on_failure="exit"]
    start -> agent -> exit
}"#;

/// The scripted catalog's `model` on provider `test`, with `small` as the
/// one fallback target.
const WORKFLOW_TOML: &str = "[run.model.fallbacks]\n\"model\" = [\"test:small\"]\n";

/// What a host rebuilds from the stream, and nothing else.
#[derive(Debug, Default)]
struct Reconstructed {
    plan_routes:  Vec<String>,
    notices:      usize,
    /// Every route a session started on, in order: the primary, then each
    /// route a failover moved to.
    routes:       Vec<String>,
    /// `(from, to, error kind, continuation)` per failover.
    failovers:    Vec<(String, String, String, String)>,
    /// What the failed route spent, per failover, as Pebble reported it.
    failed_usage: Vec<(u64, u64)>,
    stops:        Vec<(String, String)>,
    /// Answer usage summed per route, from `AssistantMessage`.
    usage:        BTreeMap<String, (u64, u64)>,
    attempt:      Option<String>,
    /// The finished step's `pebble.usage` and `pebble.usage_by_model`.
    stage_usage:  Value,
    by_model:     Value,
    run_status:   Option<RunStatus>,
    node_attempt: Option<u64>,
}

impl Reconstructed {
    /// The route the stage ended on: the last one a session started on.
    fn final_route(&self) -> Option<&str> {
        self.routes.last().map(String::as_str)
    }
}

fn route_of(value: &Value) -> String {
    format!(
        "{}/{}",
        value["provider"].as_str().unwrap_or(""),
        value["model"].as_str().unwrap_or("")
    )
}

/// Pebble's event as `(variant, payload)`; a bare variant has a null payload.
fn pebble_event(envelope: &Value) -> Option<(String, Value)> {
    match &envelope["event"] {
        Value::String(name) => Some((name.clone(), Value::Null)),
        Value::Object(map) => map
            .iter()
            .next()
            .map(|(name, payload)| (name.clone(), payload.clone())),
        _ => None,
    }
}

fn reconstruct(events: &[RunEvent]) -> Reconstructed {
    let mut out = Reconstructed::default();
    let mut current_route: Option<String> = None;
    for event in events {
        if let Some(CoordinatorEvent::RunFinished { status, .. }) = event.coordinator() {
            out.run_status = Some(*status);
        }
        if let Some(Event::StepFinished { outcome, .. }) = event.engine() {
            out.attempt = Some(outcome.status.tag().to_owned());
            // The agent's step; `start` and `exit` finish with no usage.
            let metrics = &outcome.metrics.custom;
            if let Some(usage) = metrics.get("pebble.usage") {
                out.stage_usage = usage.clone();
                out.by_model = metrics
                    .get("pebble.usage_by_model")
                    .cloned()
                    .unwrap_or_default();
            }
        }
        let Some(value) = event.custom() else {
            continue;
        };
        let activity = backend_event(value);
        match activity {
            _ if value["kind"] == PLAN_EVENT => {
                out.plan_routes = value["routes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(route_of)
                    .collect();
                out.notices = value["notices"].as_array().map_or(0, Vec::len);
                out.node_attempt = value["attempt"].as_u64();
            }
            Some(activity) if activity.backend == "pebble" => {
                let Some((variant, payload)) = pebble_event(&activity.envelope) else {
                    continue;
                };
                match variant.as_str() {
                    // A child session starts too; only the root's route
                    // is a route of the stage.
                    "SessionStarted" if activity.parent_session.is_none() => {
                        let route = route_of(&payload);
                        current_route = Some(route.clone());
                        out.routes.push(route);
                    }
                    "RouteFailover" => {
                        out.failovers.push((
                            payload["from"].as_str().unwrap_or("").to_owned(),
                            payload["to"].as_str().unwrap_or("").to_owned(),
                            payload["error"]["llm_kind"]
                                .as_str()
                                .unwrap_or("")
                                .to_owned(),
                            payload["continuation"].as_str().unwrap_or("").to_owned(),
                        ));
                        out.failed_usage.push((
                            payload["usage"]["tokens"]["input"].as_u64().unwrap_or(0),
                            payload["usage"]["tokens"]["output"].as_u64().unwrap_or(0),
                        ));
                    }
                    "RouteFailoverStopped" => out.stops.push((
                        payload["route"].as_str().unwrap_or("").to_owned(),
                        payload["reason"].as_str().unwrap_or("").to_owned(),
                    )),
                    "AssistantMessage" if activity.parent_session.is_none() => {
                        let route = current_route.clone().unwrap_or_default();
                        let entry = out.usage.entry(route).or_default();
                        entry.0 += payload["usage"]["tokens"]["input"].as_u64().unwrap_or(0);
                        entry.1 += payload["usage"]["tokens"]["output"].as_u64().unwrap_or(0);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

async fn run(dir: &RunDir, calls: Vec<ScriptedCall>) -> (Reconstructed, Vec<String>) {
    let (client, provider) = scripted_client(calls);
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Never;
    options.echo = false;
    let rt = register(Runtime::standard().frontend(Fabro::new()))
        .capability(PebbleClient(client))
        .options(options);
    fs::write(dir.path().join("wf.fabro"), WORKFLOW).expect("workflow");
    fs::write(dir.path().join("workflow.toml"), WORKFLOW_TOML).expect("workflow.toml");
    let lowered = rt
        .check(
            &dir.path().join("wf.fabro"),
            None,
            None,
            &CompileInputs::new(),
        )
        .expect("loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("lowers: {:?}", lowered.diagnostics));
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let host_run = HostRun::new(graph)
        .with_children(lowered.children)
        .observe(projector.clone() as Arc<dyn ExecutionObserver>);
    let report = host::run_configured(&rt, host_run, |_, _| {})
        .await
        .expect("the run completes");
    let _ = projector.shutdown().await;
    let requested: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| request.model().to_owned())
        .collect();
    let mut reconstructed = reconstruct(&sink.events());
    reconstructed.run_status = reconstructed.run_status.or(Some(report.status));
    (reconstructed, requested)
}

/// A server error on the primary, the fallback answers: every fact of the
/// outcome and its accounting is in the public events.
#[tokio::test]
async fn a_failover_is_reconstructed_from_public_events() {
    let dir = RunDir::new("fallback-events-failover");
    let (facts, requested) = run(&dir, vec![
        ScriptedCall::Failure(ScriptedFailure::retryable(
            ErrorKind::Server,
            "primary down",
        )),
        ScriptedCall::response(text_response("Hello from small.")),
    ])
    .await;
    assert_eq!(requested, ["test/model", "test/small"], "{facts:?}");
    assert_eq!(facts.plan_routes, ["test/model", "test/small"]);
    assert_eq!(facts.notices, 0);
    assert_eq!(facts.routes, ["test/model", "test/small"], "{facts:?}");
    assert_eq!(
        facts.failovers,
        [(
            "test/model".to_owned(),
            "test/small".to_owned(),
            "server".to_owned(),
            "replay_prompt".to_owned()
        )],
        "{facts:?}"
    );
    assert!(facts.stops.is_empty());
    // The failed route accepted nothing; the fallback's answer is counted
    // on its own route.
    assert_eq!(facts.failed_usage, [(0, 0)]);
    assert!(!facts.usage.contains_key("test/model"), "{:?}", facts.usage);
    assert!(
        facts
            .usage
            .get("test/small")
            .is_some_and(|(input, _)| *input > 0),
        "{:?}",
        facts.usage
    );
    assert_eq!(facts.final_route(), Some("test/small"));
    assert_eq!(facts.attempt.as_deref(), Some("success"));
    assert_eq!(facts.run_status, Some(RunStatus::Success));
    assert_eq!(facts.node_attempt, Some(1));
    assert_eq!(
        facts.by_model,
        json!([{"provider": "test", "model": "small", "usage": facts.stage_usage}]),
        "{facts:?}"
    );
}

/// The primary answers once and then fails mid-prompt: the stage's usage is
/// split between the route that spent it first and the route the prompt
/// continued on, and the split sums to the stage's usage.
#[tokio::test]
async fn a_failover_mid_prompt_splits_the_stage_usage_by_route() {
    let dir = RunDir::new("fallback-events-usage-split");
    let (facts, requested) = run(&dir, vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "work",
            json!({"command": "true"}),
        )),
        ScriptedCall::Failure(ScriptedFailure::retryable(
            ErrorKind::Server,
            "primary down",
        )),
        ScriptedCall::response(text_response("Hello from small.")),
    ])
    .await;
    assert_eq!(
        requested,
        ["test/model", "test/model", "test/small"],
        "{facts:?}"
    );
    assert_eq!(facts.attempt.as_deref(), Some("success"), "{facts:?}");
    let entries = facts.by_model.as_array().expect("the breakdown");
    assert_eq!(entries.len(), 2, "{facts:?}");
    assert_eq!(
        (&entries[0]["provider"], &entries[0]["model"]),
        (&json!("test"), &json!("model"))
    );
    assert_eq!(
        (&entries[1]["provider"], &entries[1]["model"]),
        (&json!("test"), &json!("small"))
    );
    // Each entry is what the events counted on its route, and the failed
    // route's is what its failover reported.
    for (entry, route) in entries.iter().zip(["test/model", "test/small"]) {
        let (input, output) = facts.usage[route];
        assert!(input > 0, "{facts:?}");
        assert_eq!(entry["usage"]["tokens"]["input"], input, "{route}");
        assert_eq!(entry["usage"]["tokens"]["output"], output, "{route}");
    }
    assert_eq!(facts.failed_usage, [facts.usage["test/model"]]);
    // A breakdown of the stage's usage, not an addition to it.
    let sum = |bucket: &str| -> u64 {
        entries
            .iter()
            .map(|entry| entry["usage"]["tokens"][bucket].as_u64().unwrap_or(0))
            .sum()
    };
    for bucket in ["input", "output", "reasoning", "cache_read", "cache_write"] {
        assert_eq!(
            Some(sum(bucket)),
            facts.stage_usage["tokens"][bucket].as_u64(),
            "{bucket}: {facts:?}"
        );
    }
}

/// Every route fails: the stop decision and the terminal outcome are in the
/// events, with the last route's error.
#[tokio::test]
async fn exhaustion_is_reconstructed_from_public_events() {
    let dir = RunDir::new("fallback-events-exhausted");
    let (facts, requested) = run(&dir, vec![
        ScriptedCall::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "bad key",
        )),
        ScriptedCall::Failure(
            ScriptedFailure::terminal(ErrorKind::QuotaExceeded, "spent").with_status(429),
        ),
    ])
    .await;
    assert_eq!(requested, ["test/model", "test/small"]);
    assert_eq!(facts.failovers.len(), 1, "{facts:?}");
    assert_eq!(facts.failovers[0].2, "authentication");
    assert_eq!(
        facts.stops,
        [("test/small".to_owned(), "exhausted".to_owned())],
        "{facts:?}"
    );
    assert_eq!(facts.final_route(), Some("test/small"));
    assert_eq!(facts.attempt.as_deref(), Some("failure"));
    assert_eq!(facts.run_status, Some(RunStatus::Failed));
}

/// An ineligible error never starts the chain: one route, one stop.
#[tokio::test]
async fn an_ineligible_error_is_reconstructed_as_a_stop_at_the_primary() {
    let dir = RunDir::new("fallback-events-ineligible");
    let (facts, requested) = run(&dir, vec![ScriptedCall::Failure(
        ScriptedFailure::terminal(ErrorKind::InvalidRequest, "bad shape"),
    )])
    .await;
    assert_eq!(requested, ["test/model"]);
    assert!(facts.failovers.is_empty());
    assert_eq!(facts.routes, ["test/model"], "{facts:?}");
    assert_eq!(
        facts.stops,
        [("test/model".to_owned(), "ineligible".to_owned())],
        "{facts:?}"
    );
    assert_eq!(facts.final_route(), Some("test/model"));
    assert_eq!(facts.attempt.as_deref(), Some("failure"));
}
