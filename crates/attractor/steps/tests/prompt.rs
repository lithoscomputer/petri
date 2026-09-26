//! `attractor/prompt` against a scripted `lithos-llm` client: one model call
//! with no tools, the output contract and its repair turns, the result
//! mapping, the ACP rejection, the prompted fan-in, and the events a host
//! maps onto Fabro's `stage.prompt` and `prompt.completed`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use attractor_steps::fallback::PLAN_EVENT;
use attractor_steps::pebble::PebbleClient;
use attractor_steps::prompt::{COMPLETED_EVENT, PROMPT_EVENT};
use attractor_steps::register;
use execution::host::{self, HostRun};
use frontend_attractor::kinds::PROMPT_KIND;
use ir::{Graph, RunStatus, Value};
use lithos_llm::types::ReasoningEffort;
use pebble_coding_agent::test_support::{
    ScriptedCompletion, ScriptedProvider, client_from, message_text, text_response,
};
use runtime::driver::{EventObserver, ExecutionReport};
use runtime::engine::{EngineState, Event, EventRecord};
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::ir::StepEvent;
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};

fn lower_all(text: &str) -> (Graph, Vec<Graph>) {
    let lowered = frontend_attractor::load("test.fabro", text, &NoFiles, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    (lowered.graph.expect("valid workflow"), lowered.children)
}

fn lower(text: &str) -> Graph {
    lower_all(text).0
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

/// Every `StepEvent::Custom` the run emitted, in order.
#[derive(Default)]
struct CustomEvents(Mutex<Vec<Value>>);

impl EventObserver for CustomEvents {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
        if let Event::StepProgressRecorded {
            ev: StepEvent::Custom(value),
            ..
        } = &record.event
        {
            self.0.lock().expect("not poisoned").push(value.clone());
        }
    }
}

fn runtime(dir: &RunDir, client: lithos_llm::Client, events: Arc<CustomEvents>) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(100);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard())
        .capability(PebbleClient(client))
        .observe(events)
        .options(options)
}

/// Run one workflow against a provider that answers `completions`, in order,
/// to the prompt step's non-streaming calls.
async fn run(
    label: &str,
    body: &str,
    completions: Vec<ScriptedCompletion>,
) -> (ExecutionReport, Arc<ScriptedProvider>, Vec<Value>) {
    let dir = RunDir::new(label);
    let (client, provider) = client_from(ScriptedProvider::new(Vec::new()).completing(completions));
    let events = Arc::new(CustomEvents::default());
    let rt = runtime(&dir, client, events.clone());
    let (graph, children) = lower_all(&dot(body));
    // The host path: a parallel node's branches run as child invocations.
    let report = host::run_configured(&rt, HostRun::new(graph).with_children(children), |_, _| {})
        .await
        .expect("the run completes");
    let events = events.0.lock().expect("not poisoned").clone();
    (report, provider, events)
}

#[tokio::test]
async fn a_prompt_node_makes_one_tool_free_call_and_writes_the_response() {
    let (report, provider, events) = run(
        "prompt-plain",
        r#"
        p [shape=tab, prompt="Summarize the interview.", model="thinking", provider="test", reasoning_effort="high"]
        start -> p -> exit
    "#,
        vec![ScriptedCompletion::response(text_response("A tidy summary."))],
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let graph_node = report
        .state
        .graph()
        .nodes
        .iter()
        .find(|n| n.name == "p")
        .expect("the prompt node");
    assert_eq!(graph_node.step.kind, PROMPT_KIND);
    let requests = provider.completion_requests();
    assert_eq!(
        requests.len(),
        1,
        "one call, no agent loop: {}",
        output_of(&report, "p")
    );
    assert!(
        requests[0].tools().is_empty(),
        "a prompt node offers no tools"
    );
    assert_eq!(requests[0].model(), "test/thinking");
    assert_eq!(requests[0].reasoning_effort(), Some(ReasoningEffort::High));
    let sent = message_text(&requests[0].messages()[0]);
    assert!(sent.contains("Summarize the interview."), "{sent}");
    assert!(
        !sent.contains("final-output contract"),
        "no contract without an output_schema: {sent}"
    );
    let output = output_of(&report, "p");
    assert_eq!(output["text"], json!("A tidy summary."));
    assert_eq!(output["outcome"], json!("succeeded"));
    let kv = report.state.run_context();
    assert_eq!(kv.get("response.p"), Some(&json!("A tidy summary.")));
    assert_eq!(kv.get("last_response"), Some(&json!("A tidy summary.")));
    assert_eq!(kv.get("last_stage"), Some(&json!("p")));
    // The plan is the one fallback fact a prompt node reports; a prompt
    // node runs no Pebble session, so nothing else describes its route.
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    assert_eq!(kinds, [PLAN_EVENT, PROMPT_EVENT, COMPLETED_EVENT]);
    let events: Vec<serde_json::Value> = events
        .into_iter()
        .filter(|e| {
            !e["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("attractor.fallback."))
        })
        .collect();
    assert!(
        events[0]["prompt"]
            .as_str()
            .is_some_and(|p| p.contains("Summarize the interview.")),
        "{}",
        events[0]
    );
    assert_eq!(events[1]["outcome"], json!("succeeded"));
    assert_eq!(events[1]["calls"], json!(1));
    assert_eq!(events[1]["response"], json!("A tidy summary."));
    assert!(events[1]["usage"]["tokens"]["input"].as_u64().is_some());
    let metrics = &report
        .state
        .history()
        .iter()
        .find(|r| r.name == "p")
        .expect("record")
        .outcome
        .metrics
        .custom;
    assert_eq!(metrics["prompt.calls"], json!(1));
}

#[tokio::test]
async fn a_routing_contract_is_repaired_then_routes_and_exhaustion_fails() {
    let (report, provider, _) = run(
        "prompt-repair",
        r#"
        p [shape=tab, prompt="Decide.", output_schema="routing", output_retries=1]
        yes [shape=parallelogram, script="true"]
        no [shape=parallelogram, script="true"]
        start -> p
        p -> yes [label="[Y] Yes"]
        p -> no [label="[N] No"]
        yes -> exit
        no -> exit
    "#,
        vec![
            ScriptedCompletion::response(text_response("I am not sure.")),
            ScriptedCompletion::response(text_response(
                r#"{"preferred_next_label": "No", "context_updates": {"why": "risk"}}"#,
            )),
        ],
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = provider.completion_requests();
    assert_eq!(requests.len(), 2, "one repair turn");
    let repair = &requests[1].messages();
    assert_eq!(repair.len(), 3, "prompt, failed reply, repair message");
    assert!(message_text(&repair[2]).contains("did not satisfy the output contract"));
    assert_eq!(status_of(&report, "no").as_deref(), Some("success"));
    assert_eq!(status_of(&report, "yes"), None);
    assert_eq!(report.state.run_context().get("why"), Some(&json!("risk")));

    let (report, _, events) = run(
        "prompt-exhausted",
        r#"
        p [shape=tab, prompt="Decide.", output_schema="routing", output_retries=1, on_failure="exit"]
        start -> p -> exit
    "#,
        vec![ScriptedCompletion::response(text_response("Never JSON."))],
    )
    .await;
    assert_eq!(report.status, RunStatus::Failed);
    let output = output_of(&report, "p");
    assert_eq!(output["failure_class"], json!("bad_output"));
    assert!(
        output["failure_reason"]
            .as_str()
            .is_some_and(|r| r.contains("after 1 repair attempt(s)")),
        "{output}"
    );
    assert_eq!(
        events.last().expect("completed")["outcome"],
        json!("failed")
    );
    assert_eq!(events.last().expect("completed")["calls"], json!(2));
}

#[tokio::test]
async fn a_json_schema_contract_writes_the_structured_output() {
    let (report, provider, _) = run(
        "prompt-schema",
        r#"
        p [shape=tab, prompt="Count.", output_schema="{\"type\":\"object\",\"required\":[\"n\"],\"properties\":{\"n\":{\"type\":\"integer\"}}}"]
        start -> p -> exit
    "#,
        vec![ScriptedCompletion::response(text_response(r#"{"n": 3}"#))],
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        output_of(&report, "p")["structured"],
        json!({ "n": 3 }),
        "{}",
        output_of(&report, "p")
    );
    assert_eq!(
        report.state.run_context().get("output.p"),
        Some(&json!({ "n": 3 }))
    );
    let sent = message_text(&provider.completion_requests()[0].messages()[0]);
    assert!(sent.contains("<output_schema>"), "{sent}");
}

#[tokio::test]
async fn a_json_schema_contract_spreads_top_level_context_updates() {
    // Fabro's schema contracts promise routing-kind context_updates
    // semantics: a top-level context_updates object inside the validated
    // response reaches the run context like a directive's does, so a
    // later stage can read the keys through stdin_source/kv.
    let (report, _, _) = run(
        "prompt-schema-context",
        r#"
        p [shape=tab, prompt="Count.", output_schema="{\"type\":\"object\",\"required\":[\"n\"],\"properties\":{\"n\":{\"type\":\"integer\"},\"context_updates\":{\"type\":\"object\"}}}"]
        start -> p -> exit
    "#,
        vec![ScriptedCompletion::response(text_response(
            r#"{"n": 3, "context_updates": {"seed": "seeds-1", "brief": "do it"}}"#,
        ))],
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let kv = report.state.run_context();
    assert_eq!(kv.get("output.p"), Some(&json!({ "n": 3, "context_updates": {"seed": "seeds-1", "brief": "do it"} })));
    assert_eq!(kv.get("seed"), Some(&json!("seeds-1")));
    assert_eq!(kv.get("brief"), Some(&json!("do it")));
}

#[test]
fn acp_on_a_prompt_node_is_refused_as_fabro_refuses_it() {
    let lowered = frontend_attractor::load(
        "test.fabro",
        &dot(r#"
        p [shape=tab, prompt="x", backend="acp", acp.command="agent"]
        start -> p -> exit
    "#),
        &NoFiles,
        &CompileInputs::new(),
    );
    let codes: Vec<String> = lowered
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect();
    assert!(
        codes.contains(&"attractor.prompt_backend".to_string()),
        "{codes:?}"
    );
    assert!(
        lowered
            .diagnostics
            .iter()
            .any(|d| d.message.contains("prompt nodes are API-only")),
        "{:?}",
        lowered.diagnostics
    );
    // The graph's ACP default does not reach a prompt node.
    let graph = lower(
        r#"digraph T {
        graph [backend="acp", acp.command="agent", default_model="test/model"]
        start [shape=Mdiamond]
        exit [shape=Msquare]
        a [prompt="x"]
        p [shape=tab, prompt="y"]
        start -> a -> p -> exit
    }"#,
    );
    let node = |name: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.name == name)
            .expect("node")
            .step
            .config
            .clone()
    };
    assert_eq!(node("a")["backend"], json!("acp"));
    assert!(node("p").get("backend").is_none(), "{}", node("p"));
    assert!(node("p").get("acp").is_none());
}

#[tokio::test]
async fn a_prompted_fan_in_joins_in_order_then_prompts_over_the_results() {
    let (report, provider, events) = run(
        "prompt-fan-in",
        r#"
        fork [shape=component]
        a [shape=parallelogram, script="sleep 0.2; echo from-a"]
        b [shape=parallelogram, script="echo from-b"]
        merge [shape=tripleoctagon, prompt="Combine the branch results."]
        start -> fork
        fork -> a
        fork -> b
        a -> merge
        b -> merge
        merge -> exit
    "#,
        vec![ScriptedCompletion::response(text_response("Combined."))],
    )
    .await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "merge");
    assert_eq!(
        output["sources"],
        json!(["a", "b"]),
        "{output} / {:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>()
    );
    assert_eq!(output["branch_count"], json!(2));
    assert_eq!(output["text"], json!("Combined."));
    let sent = message_text(&provider.completion_requests()[0].messages()[0]);
    let a_at = sent.find("Branch 0: a").expect("branch a in the prompt");
    let b_at = sent.find("Branch 1: b").expect("branch b in the prompt");
    assert!(a_at < b_at, "branch order, not completion order:\n{sent}");
    assert!(sent.contains("from-a") && sent.contains("from-b"), "{sent}");
    assert!(sent.contains("Combine the branch results."), "{sent}");
    // The branch steps emit events of their own; the prompt event is the
    // one with the prompt kind.
    let prompt_event = events
        .iter()
        .find(|event| event["kind"] == PROMPT_EVENT)
        .expect("the prompt event");
    assert_eq!(prompt_event["sources"], json!(["a", "b"]));
    assert_eq!(
        report.state.run_context().get("response.merge"),
        Some(&json!("Combined."))
    );
}
