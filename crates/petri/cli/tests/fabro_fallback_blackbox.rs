//! Readiness item 9a (milestone C1) through the shipped binary: model
//! fallback and failover on `[run.model.fallbacks]`, with the provider twins
//! injecting the failures. Every case runs the real `petri` binary with an
//! isolated environment, one twin per provider on loopback, and reads what
//! the run reported: its status, the workspace, the twins' request logs, and
//! the event log the run names, where the plan is Petri's `fabro.fallback.plan`
//! and every route fact is Pebble's own event (`SessionStarted`,
//! `RouteFailover`, `RouteFailoverStopped`, `AssistantMessage`).
//!
//! The expected request sequences were derived from the Fabro source at
//! `b6482910` (`handler/llm/api.rs`: `fallback_plan`,
//! `failover_agent_session`, `complete_one_shot_request`), the pin at the
//! time; the reference binary was not run against the twins. Since
//! `05ebd0f` Fabro runs its failover in Pebble (`handler/llm/pebble.rs`,
//! `fallback.rs`) and keeps the conversation across a model change as Petri
//! does; the differential `fallback-failover` cell compares the two live.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::fabro::failures::{self, any_request, error, hang, refusal, repeated};
use support::fabro::launch::{Case, Launch};
use support::fabro::twins::{
    Provider, Twin, model, requested_effort, scenario, shell_tool, text, tool_call,
};

const OPENAI: Provider = Provider::OpenAi;
const ANTHROPIC: Provider = Provider::Anthropic;
const OPENROUTER: Provider = Provider::OpenRouter;

/// One request each: the client's own retries are off unless a case turns
/// them on, so the twin request logs show exactly the fallback sequence.
fn no_client_retries() -> Launch {
    Launch {
        env: vec![("PETRI_LLM_RETRY_ATTEMPTS".into(), "1".into())],
        ..Launch::default()
    }
}

/// Pebble's `RouteFailover` events of `node`, in order: each move the
/// prompt made, with the failed route's usage and the typed error.
fn failovers(run_dir: &Path, node: &str) -> Vec<Value> {
    failures::pebble_events(run_dir, node, "RouteFailover")
}

/// Pebble's `RouteFailoverStopped` events of `node`: a model error that
/// ended the prompt although routes were named, with the reason.
fn stops(run_dir: &Path, node: &str) -> Vec<Value> {
    failures::pebble_events(run_dir, node, "RouteFailoverStopped")
}

/// The `provider/model` each of `node`'s sessions started on, in order: the
/// primary, then each route a failover moved to.
fn sessions(run_dir: &Path, node: &str) -> Vec<String> {
    failures::pebble_events(run_dir, node, "SessionStarted")
        .iter()
        .map(failures::route)
        .collect()
}

/// An agent workflow on `primary` with `workflow.toml` chains.
fn agent_workflow(case: &Case, primary: Provider, extra_attrs: &str, toml: &str) -> PathBuf {
    let model = model(primary);
    let provider = primary.id();
    case.workflow(
        &format!(
            r#"digraph Fallback {{
    graph [backend="api", goal="Answer the question"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello to the reviewer.", model="{model}", provider="{provider}", on_failure="exit"{extra_attrs}]
    start -> agent -> exit
}}"#
        ),
        Some(toml),
    )
}

fn chain_openai_to_anthropic() -> &'static str {
    "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"anthropic:claude-sonnet-5\"]\n"
}

/// The requests the twin logged for this case.
fn requests(twin: &Twin, case: &Case) -> Vec<Value> {
    twin.requests_for(&case.credential)
}

fn plan_routes(records: &[Value]) -> Vec<String> {
    plan_routes_of(records, "agent")
}

fn plan_routes_of(records: &[Value], node: &str) -> Vec<String> {
    let plan = failures::of_node(records, node, "fabro.fallback.plan");
    assert_eq!(plan.len(), 1, "one plan per stage: {records:?}");
    plan[0]["routes"]
        .as_array()
        .expect("routes")
        .iter()
        .map(failures::route)
        .collect()
}

/// The primary answers: the plan names the chain and nothing advances.
#[tokio::test]
async fn a_successful_primary_request_never_leaves_its_route() {
    let mut case = Case::new("fallback-primary-ok");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary",
        model(OPENAI),
        "Say hello",
        text("Hello from the primary."),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary"]);
    assert!(
        requests(&anthropic, &case).is_empty(),
        "the fallback never ran"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Hello from the primary.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    // Nothing moved: Pebble reports no failover and no stop, and the one
    // session ran on the primary.
    assert!(failovers(&finished.run_dir, "agent").is_empty());
    assert!(stops(&finished.run_dir, "agent").is_empty());
    assert_eq!(sessions(&finished.run_dir, "agent"), ["openai/gpt-5.6-sol"]);
    let thread = &failures::of_node(&records, "agent", "fabro.thread")[0];
    assert_eq!(thread["reused"], json!(false));
    assert!(
        !finished.stderr.contains("model fallback:"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A server error on the primary is eligible: the same prompt goes to the
/// next provider, which answers, and the stage succeeds on that route.
#[tokio::test]
async fn a_qualifying_failure_moves_the_prompt_to_the_next_provider() {
    let mut case = Case::new("fallback-qualifying");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(500, "server_error", "server_error", "the primary is down"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "fallback-answers",
            model(ANTHROPIC),
            "Say hello",
            text("Hello from the fallback."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["fallback-answers"]);
    assert_eq!(requests(&openai, &case).len(), 1, "no client retry");
    let fallback = requests(&anthropic, &case);
    assert_eq!(fallback.len(), 1);
    let sent = serde_json::to_string(&fallback[0]).expect("request");
    assert!(
        sent.contains("Say hello to the reviewer."),
        "the fallback gets the prompt itself, not a continuation: {sent}"
    );
    assert!(
        !sent.contains("moved to another model"),
        "no continuation text when no work happened: {sent}"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Hello from the fallback.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    // Pebble's own account of the move: one failover with the failed
    // route's typed error and its spend, no stop, a session on each route
    // in order, and the answer on the fallback's session.
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    let failover = &moved[0];
    assert_eq!(failover["attempt"], json!(1));
    assert_eq!(failover["from"], json!("openai/gpt-5.6-sol"));
    assert_eq!(failover["to"], json!("anthropic/claude-sonnet-5"));
    assert_eq!(failover["error"]["llm_kind"], json!("server"));
    assert_eq!(failover["error"]["status"], json!(500));
    assert_eq!(failover["continuation"], json!("replay_prompt"));
    assert_eq!(
        failover["usage"]["input"],
        json!(0),
        "the failed route accepted nothing: {failover}"
    );
    assert!(stops(&finished.run_dir, "agent").is_empty());
    assert_eq!(sessions(&finished.run_dir, "agent"), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    let answers = failures::pebble_events(&finished.run_dir, "agent", "AssistantMessage");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["model"], json!("claude-sonnet-5"));
    assert!(
        answers[0]["usage"]["input"].as_u64().is_some_and(|n| n > 0),
        "{}",
        answers[0]
    );
    assert!(
        finished
            .stderr
            .contains("model fallback: openai/gpt-5.6-sol failed (server); continuing on anthropic/claude-sonnet-5 (attempt 1 of the plan)"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// An invalid request is deterministic: no fallback starts, the stage fails
/// with the typed class, and the next provider sees nothing.
#[tokio::test]
async fn a_non_qualifying_failure_fails_the_stage_without_fallback() {
    let mut case = Case::new("fallback-ineligible");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "bad-request",
        model(OPENAI),
        "Say hello",
        error(
            400,
            "invalid_request_error",
            "invalid_request",
            "bad request shape",
        ),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["bad-request"]);
    assert!(requests(&anthropic, &case).is_empty());
    let context = finished.final_context();
    assert_eq!(
        context["failure_class"],
        json!("llm:invalid_request"),
        "{context:?}"
    );
    assert!(failovers(&finished.run_dir, "agent").is_empty());
    let stopped = stops(&finished.run_dir, "agent");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["reason"], json!("ineligible"));
    assert_eq!(stopped[0]["attempt"], json!(0));
    assert_eq!(stopped[0]["route"], json!("openai/gpt-5.6-sol"));
    assert_eq!(stopped[0]["error"]["llm_kind"], json!("invalid_request"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Three providers: authentication fails on the first, the second is
/// overloaded, the third answers. The failover events chain, each `to` the
/// next `from`.
#[tokio::test]
async fn a_later_provider_succeeds_after_two_failures() {
    let mut case = Case::new("fallback-third");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "no-auth",
        model(OPENAI),
        "Say hello",
        error(401, "authentication_error", "invalid_api_key", "bad key"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![
        any_request(
            OPENROUTER,
            &case.credential,
            "overloaded",
            "moonshotai/kimi-k3",
            error(503, "server_error", "service_unavailable", "overloaded"),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "third-answers",
            model(ANTHROPIC),
            "Say hello",
            text("Third time lucky."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    case.redirect(&anthropic);
    let workflow = agent_workflow(
        &case,
        OPENAI,
        "",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:kimi-k3\", \"anthropic:claude-sonnet-5\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["no-auth"]);
    assert_eq!(openrouter.consumed(), ["overloaded"]);
    assert_eq!(anthropic.consumed(), ["third-answers"]);
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Third time lucky.")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "openrouter/kimi-k3",
        "anthropic/claude-sonnet-5"
    ]);
    // The failovers chain: one event's `to` is the next event's `from`.
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 2, "{moved:?}");
    assert_eq!(moved[0]["error"]["llm_kind"], json!("authentication"));
    assert_eq!(moved[0]["to"], json!("openrouter/kimi-k3"));
    assert_eq!(moved[1]["from"], json!("openrouter/kimi-k3"));
    assert_eq!(moved[1]["error"]["llm_kind"], json!("server"));
    assert_eq!(moved[1]["to"], json!("anthropic/claude-sonnet-5"));
    assert_eq!(moved[1]["attempt"], json!(2));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
    anthropic.stop();
}

/// Every route fails: the stage fails with the last route's typed error,
/// the plan reports exhaustion, and the outcome keeps the position reached.
#[tokio::test]
async fn chain_exhaustion_fails_the_stage_with_the_last_error() {
    let mut case = Case::new("fallback-exhausted");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "no-auth",
        model(OPENAI),
        "Say hello",
        error(401, "authentication_error", "invalid_api_key", "bad key"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "no-quota",
            model(ANTHROPIC),
            "Say hello",
            error(
                429,
                "rate_limit_error",
                "insufficient_quota",
                "credit spent",
            ),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["no-auth"]);
    assert_eq!(anthropic.consumed(), ["no-quota"]);
    let context = finished.final_context();
    assert_eq!(
        context["failure_class"],
        json!("llm:quota_exceeded"),
        "{context:?}"
    );
    assert_eq!(failovers(&finished.run_dir, "agent").len(), 1);
    let stopped = stops(&finished.run_dir, "agent");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["reason"], json!("exhausted"));
    assert_eq!(stopped[0]["attempt"], json!(1));
    assert_eq!(stopped[0]["route"], json!("anthropic/claude-sonnet-5"));
    assert_eq!(stopped[0]["error"]["llm_kind"], json!("quota_exceeded"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A provider interruption after a non-idempotent tool effect: the primary
/// asks for an append, the append runs, the primary's next request fails,
/// and the fallback continues from the tool result. The append happens once
/// and the next model sees its output. Petri keeps the conversation, as the
/// reference Fabro does since `05ebd0f` (before that it rebuilt the session
/// from the original prompt, which ran the tool again).
#[tokio::test]
async fn a_tool_effect_is_not_repeated_across_a_failover() {
    let mut case = Case::new("fallback-tool-effect");
    let shell = shell_tool(OPENAI);
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "append",
            model(OPENAI),
            "Say hello",
            tool_call(
                "append",
                shell,
                json!({ "command": "echo appended >> effects.log && echo APPEND_DONE" }),
            ),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "primary-down",
            model(OPENAI),
            "APPEND_DONE",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "continues",
            model(ANTHROPIC),
            "APPEND_DONE",
            text("Appended once, as asked."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["append", "primary-down"]);
    assert_eq!(anthropic.consumed(), ["continues"]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("effects.log")).expect("effects.log"),
        "appended\n",
        "the append ran once"
    );
    let fallback = requests(&anthropic, &case);
    assert_eq!(fallback.len(), 1);
    let sent = serde_json::to_string(&fallback[0]).expect("request");
    assert!(
        sent.contains("APPEND_DONE"),
        "the tool result reached the next model: {sent}"
    );
    assert!(
        sent.contains("Say hello to the reviewer."),
        "the original prompt is in the conversation: {sent}"
    );
    assert!(
        !sent.contains("do not run those tools again"),
        "no continuation text: the next model continues from the tool result itself: {sent}"
    );
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["continuation"], json!("continue_turn"));
    assert_eq!(moved[0]["error"]["llm_kind"], json!("server"));
    assert!(
        moved[0]["usage"]["input"].as_u64().is_some_and(|n| n > 0),
        "the tool-call answer the primary gave before it failed is the failed route's spend: {}",
        moved[0]
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Appended once, as asked.")
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Cancellation while the fallback route is active: the fallback's tool
/// call marks the workspace, the person interrupts, and the run is
/// cancelled without another decision or request.
#[tokio::test]
async fn cancellation_during_fallback_cancels_the_run() {
    let mut case = Case::new("fallback-cancel");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "slow-work",
            model(ANTHROPIC),
            "Say hello",
            tool_call(
                "slow",
                shell_tool(ANTHROPIC),
                json!({ "command": "touch fallback-active.txt && sleep 30 && echo NEVER" }),
            ),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        interrupt_when: Some(case.workspace().join("fallback-active.txt")),
        ..no_client_retries()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["slow-work"]);
    assert_eq!(
        requests(&anthropic, &case).len(),
        1,
        "no request after the interrupt"
    );
    assert_eq!(failovers(&finished.run_dir, "agent").len(), 1);
    assert!(
        stops(&finished.run_dir, "agent").is_empty(),
        "a cancelled prompt reports no stop decision"
    );
    // The tool-call response the fallback accepted before the interrupt is
    // on the stream, on the fallback's session.
    let answers = failures::pebble_events(&finished.run_dir, "agent", "AssistantMessage");
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0]["model"], json!("claude-sonnet-5"));
    assert_eq!(answers[0]["usage"]["input"], json!(10), "{}", answers[0]);
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A refusal is the one content filter the reference fails over on; any
/// other content filter is deterministic and ends the stage.
#[tokio::test]
async fn a_refusal_is_eligible_but_another_content_filter_is_not() {
    // Anthropic primary refuses; OpenAI answers.
    let mut case = Case::new("fallback-refusal");
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "refuses",
            model(ANTHROPIC),
            "Say hello",
            refusal(ANTHROPIC),
        ),
    ])
    .await;
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "answers",
        model(OPENAI),
        "Say hello",
        text("Hello, no refusal here."),
    )])
    .await;
    case.redirect(&anthropic);
    case.redirect(&openai);
    let workflow = agent_workflow(
        &case,
        ANTHROPIC,
        "",
        "[run.model.fallbacks]\n\"claude-sonnet-5\" = [\"openai:gpt-5.6-sol\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(anthropic.consumed(), ["refuses"]);
    assert_eq!(openai.consumed(), ["answers"]);
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["error"]["llm_kind"], json!("content_filter"));
    assert_eq!(moved[0]["error"]["provider_code"], json!("refusal"));
    finished.assert_no_leaked_processes().await;
    anthropic.stop();
    openai.stop();

    // OpenAI primary filtered without a refusal code: no fallback.
    let mut case = Case::new("fallback-content-filter");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "filtered",
        model(OPENAI),
        "Say hello",
        error(
            400,
            "invalid_request_error",
            "content_filter",
            "blocked by policy",
        ),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert!(requests(&anthropic, &case).is_empty());
    assert_eq!(
        finished.final_context()["failure_class"],
        json!("llm:content_filter")
    );
    let stopped = stops(&finished.run_dir, "agent");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["reason"], json!("ineligible"));
    assert_eq!(
        stopped[0]["error"]["provider_code"],
        json!("content_filter")
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A request that exceeds the client's call budget is a timeout, which is
/// eligible: the next provider answers.
#[tokio::test]
async fn a_request_timeout_is_eligible() {
    let mut case = Case::new("fallback-timeout");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "hangs",
        model(OPENAI),
        "Say hello",
        hang(),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Answered in time."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        env: vec![
            ("PETRI_LLM_RETRY_ATTEMPTS".into(), "1".into()),
            ("PETRI_LLM_TIMEOUT_MS".into(), "1500".into()),
        ],
        ..Launch::default()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    finished.assert_code(0);
    assert_eq!(openai.consumed(), ["hangs"]);
    assert_eq!(anthropic.consumed(), ["answers"]);
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(
        moved[0]["error"]["llm_kind"],
        json!("timeout"),
        "{}",
        moved[0]
    );
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// The client's own retries run before any fallback decision: with two
/// attempts, the primary is asked twice, then the chain advances once.
#[tokio::test]
async fn client_retries_are_spent_before_the_chain_advances() {
    let mut case = Case::new("fallback-client-retries");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![repeated(
        scenario(
            OPENAI,
            &case.credential,
            "flaky",
            model(OPENAI),
            "Say hello",
            error(503, "server_error", "service_unavailable", "flaky"),
        ),
        2,
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Steady."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = agent_workflow(&case, OPENAI, "", chain_openai_to_anthropic());
    let launch = Launch {
        env: vec![("PETRI_LLM_RETRY_ATTEMPTS".into(), "2".into())],
        ..Launch::default()
    };
    let finished = case.run_with(&workflow, &[], launch).await;
    finished.assert_code(0);
    assert_eq!(
        openai.consumed(),
        ["flaky", "flaky"],
        "two sends of one request"
    );
    assert_eq!(requests(&openai, &case).len(), 2);
    assert_eq!(anthropic.consumed(), ["answers"]);
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(
        moved.len(),
        1,
        "one fallback decision, whatever the client retried: {moved:?}"
    );
    // The client's own retry is on the agent's event stream: Petri installs
    // Pebble's `RetryEventObserver` on the middleware it builds.
    let retries = failures::pebble_events(&finished.run_dir, "agent", "LlmRetry");
    assert_eq!(retries.len(), 1, "one client retry reported: {retries:?}");
    assert_eq!(retries[0]["provider"], OPENAI.id());
    assert_eq!(retries[0]["model"], model(OPENAI));
    assert_eq!(retries[0]["phase"], "open");
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A workflow retry is a new firing with its own plan at position 0, not a
/// failover: a stage whose failed outcome routes back to itself asks the
/// primary again and never reaches the chain, whatever the chain says.
#[tokio::test]
async fn a_workflow_retry_is_not_a_failover() {
    let mut case = Case::new("fallback-workflow-retry");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "first-firing",
            model(OPENAI),
            "Say hello",
            text(r#"{"outcome": "failed", "failure_reason": "not yet"}"#),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "second-firing",
            model(OPENAI),
            "Say hello",
            text(r#"{"outcome": "succeeded"}"#),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let model = model(OPENAI);
    let workflow = case.workflow(
        &format!(
            r#"digraph Retry {{
    graph [backend="api", goal="Answer the question"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Say hello to the reviewer.", model="{model}", provider="openai", output_schema="routing", max_visits=3]
    start -> agent
    agent -> agent [condition="outcome=failed"]
    agent -> exit
}}"#
        ),
        Some(chain_openai_to_anthropic()),
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["first-firing", "second-firing"]);
    assert!(
        requests(&anthropic, &case).is_empty(),
        "the chain never ran"
    );
    let records = failures::records(&finished.run_dir);
    let plans = failures::of_node(&records, "agent", "fabro.fallback.plan");
    assert_eq!(plans.len(), 2, "one plan per firing: {records:?}");
    assert_ne!(plans[0]["firing"], plans[1]["firing"]);
    assert!(failovers(&finished.run_dir, "agent").is_empty());
    assert!(stops(&finished.run_dir, "agent").is_empty());
    // Two sessions, both on the primary: one per firing.
    assert_eq!(sessions(&finished.run_dir, "agent"), [
        "openai/gpt-5.6-sol",
        "openai/gpt-5.6-sol"
    ]);
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// Reasoning effort maps per target through the catalog: a target with no
/// nearby level is skipped with a `NoNearbyReasoningLevel` warning, one
/// that advertises no levels keeps the request, and a chain left with no
/// usable target warns `ChainEmpty` and runs on the primary alone.
#[tokio::test]
async fn reasoning_effort_maps_per_target_and_unfit_targets_are_skipped() {
    let mut case = Case::new("fallback-effort");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![]).await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "answers",
            model(ANTHROPIC),
            "Say hello",
            text("Thought hard."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    case.redirect(&anthropic);
    // nemotron on OpenRouter advertises no reasoning at all; Claude Sonnet 5
    // advertises reasoning with unspecified levels.
    let workflow = agent_workflow(
        &case,
        OPENAI,
        ", reasoning_effort=\"high\"",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:nemotron-3-super-120b-a12b\", \"anthropic:claude-sonnet-5\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert!(
        requests(&openrouter, &case).is_empty(),
        "the unfit target never ran"
    );
    assert_eq!(anthropic.consumed(), ["answers"]);
    let fallback = requests(&anthropic, &case);
    assert_eq!(requested_effort(ANTHROPIC, &fallback[0]), Some("high"));
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    let plan = &failures::of_node(&records, "agent", "fabro.fallback.plan")[0];
    assert_eq!(plan["routes"][1]["reasoning_effort"], json!("high"));
    let notices = plan["notices"].as_array().expect("notices");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["code"], json!("model_fallback_skipped"));
    assert!(
        notices[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("no reasoning level near `high`")),
        "{notices:?}"
    );
    assert!(
        finished.stderr.contains("warn: Model fallback `openrouter:nemotron-3-super-120b-a12b` for requested model `gpt-5.6-sol` was skipped because it has no reasoning level near `high`."),
        "{}",
        finished.stderr
    );
    // The move went to the one usable target; its effort is the plan's
    // (asserted above) and the request's (asserted above).
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["to"], json!("anthropic/claude-sonnet-5"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
    anthropic.stop();

    // Only the unfit target: the chain is empty and the failure stands.
    let mut case = Case::new("fallback-chain-empty");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&openrouter);
    let workflow = agent_workflow(
        &case,
        OPENAI,
        ", reasoning_effort=\"high\"",
        "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"openrouter:nemotron-3-super-120b-a12b\"]\n",
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert!(requests(&openrouter, &case).is_empty());
    assert_eq!(
        finished.final_context()["failure_class"],
        json!("llm:server")
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes(&records), ["openai/gpt-5.6-sol"]);
    let plan = &failures::of_node(&records, "agent", "fabro.fallback.plan")[0];
    let codes: Vec<&str> = plan["notices"]
        .as_array()
        .expect("notices")
        .iter()
        .filter_map(|n| n["code"].as_str())
        .collect();
    assert_eq!(codes, [
        "model_fallback_skipped",
        "model_fallback_chain_empty"
    ]);
    assert!(
        finished
            .stderr
            .contains("warn: No usable model fallbacks remain for requested model `gpt-5.6-sol`"),
        "{}",
        finished.stderr
    );
    // With no usable target the plan names no fallback route to Pebble, so
    // Pebble has nothing to say about routes: no failover, no stop; the
    // stage fails with the primary's error (asserted above).
    assert!(failovers(&finished.run_dir, "agent").is_empty());
    assert!(stops(&finished.run_dir, "agent").is_empty());
    finished.assert_no_leaked_processes().await;
    openai.stop();
    openrouter.stop();
}

/// Output-repair turns stay on the original model's plan, and advancing
/// never activates the target's own chain: the primary's answer misses the
/// contract, the repair turn on the primary fails, the fallback gets the
/// repair prompt and answers; when the fallback fails too, its own
/// configured chain is not consulted and the stage is exhausted.
#[tokio::test]
async fn repair_turns_stay_on_the_plan_and_a_target_chain_is_inert() {
    // Part one: the repair turn moves with the plan.
    let mut case = Case::new("fallback-repair");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "malformed",
            model(OPENAI),
            "Say hello",
            text("not json at all"),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "repair-down",
            model(OPENAI),
            "did not satisfy the output contract",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "repairs",
            model(ANTHROPIC),
            "did not satisfy the output contract",
            text(r#"{"outcome": "succeeded", "context_updates": {"fixed": "yes"}}"#),
        ),
    ])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![]).await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    case.redirect(&openrouter);
    let chains = "[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"anthropic:claude-sonnet-5\"]\n\"claude-sonnet-5\" = [\"openrouter:kimi-k3\"]\n";
    let workflow = agent_workflow(&case, OPENAI, ", output_schema=\"routing\"", chains);
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["malformed", "repair-down"]);
    assert_eq!(anthropic.consumed(), ["repairs"]);
    assert!(requests(&openrouter, &case).is_empty());
    let sent = serde_json::to_string(&requests(&anthropic, &case)[0]).expect("request");
    assert!(
        sent.contains("not json at all") && sent.contains("did not satisfy the output contract"),
        "the fallback continues the repair conversation: {sent}"
    );
    assert_eq!(finished.final_context()["fixed"], json!("yes"));
    let records = failures::records(&finished.run_dir);
    assert_eq!(
        plan_routes(&records),
        ["openai/gpt-5.6-sol", "anthropic/claude-sonnet-5"],
        "the plan is the original model's, not extended by the target's chain"
    );
    let moved = failovers(&finished.run_dir, "agent");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0]["continuation"], json!("replay_prompt"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
    openrouter.stop();

    // Part two: the target fails too; its own chain (to OpenRouter) is inert.
    let mut case = Case::new("fallback-target-chain-inert");
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model(OPENAI),
        "Say hello",
        error(503, "server_error", "service_unavailable", "gone away"),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "target-down",
            model(ANTHROPIC),
            "Say hello",
            error(529, "overloaded_error", "overloaded", "overloaded"),
        ),
    ])
    .await;
    let openrouter = Twin::start(OPENROUTER, &case.root.join("twin-openrouter"), vec![
        any_request(
            OPENROUTER,
            &case.credential,
            "never",
            "moonshotai/kimi-k3",
            text("Should not be asked."),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    case.redirect(&openrouter);
    let workflow = agent_workflow(&case, OPENAI, "", chains);
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(1);
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["target-down"]);
    assert!(
        requests(&openrouter, &case).is_empty(),
        "the target's own chain never activates"
    );
    let stopped = stops(&finished.run_dir, "agent");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["reason"], json!("exhausted"));
    assert_eq!(stopped[0]["route"], json!("anthropic/claude-sonnet-5"));
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
    openrouter.stop();
}

/// A retained thread carries its plan: after the first node fails over, the
/// second `full` node on the thread continues on the fallback route without
/// asking the primary, and a later node off the thread starts a new plan on
/// the primary.
#[tokio::test]
async fn a_retained_thread_continues_on_the_fallback_route() {
    let mut case = Case::new("fallback-thread");
    let model_openai = model(OPENAI);
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![
        scenario(
            OPENAI,
            &case.credential,
            "plan-down",
            model_openai,
            "Write a plan",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
        scenario(
            OPENAI,
            &case.credential,
            "review",
            model_openai,
            "Review the work",
            text("Reviewed on the primary."),
        ),
    ])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "plan",
            model(ANTHROPIC),
            "Write a plan",
            text("PLAN: add a health endpoint"),
        ),
        scenario(
            ANTHROPIC,
            &case.credential,
            "implement",
            model(ANTHROPIC),
            "Implement the plan",
            text("IMPLEMENTED on the fallback"),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = case.workflow(
        &format!(
            r#"digraph Threads {{
    graph [backend="api", goal="Ship a health endpoint"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [prompt="Write a plan.", model="{model_openai}", provider="openai", fidelity="full", thread_id="impl", on_failure="exit"]
    implement [prompt="Implement the plan.", model="{model_openai}", provider="openai", fidelity="full", thread_id="impl", on_failure="exit"]
    review [prompt="Review the work.", model="{model_openai}", provider="openai", fidelity="summary:low", on_failure="exit"]
    start -> plan -> implement -> review -> exit
}}"#
        ),
        Some(chain_openai_to_anthropic()),
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["plan-down", "review"]);
    assert_eq!(anthropic.consumed(), ["plan", "implement"]);
    let on_fallback = requests(&anthropic, &case);
    let second = serde_json::to_string(&on_fallback[1]).expect("request");
    assert!(
        second.contains("PLAN: add a health endpoint") && second.contains("Implement the plan"),
        "the thread's conversation continues on the fallback route: {second}"
    );
    let records = failures::records(&finished.run_dir);
    assert_eq!(
        failures::of_node(&records, "plan", "fabro.fallback.plan").len(),
        1
    );
    assert_eq!(failovers(&finished.run_dir, "plan").len(), 1);
    assert!(
        failures::of_node(&records, "implement", "fabro.fallback.plan").is_empty(),
        "a reused thread has no plan of its own: {records:?}"
    );
    let reused = &failures::of_node(&records, "implement", "fabro.thread")[0];
    assert_eq!(reused["reused"], json!(true));
    // The reused thread resumes on the route it reached and moves no further.
    assert_eq!(sessions(&finished.run_dir, "implement"), [
        "anthropic/claude-sonnet-5"
    ]);
    assert!(failovers(&finished.run_dir, "implement").is_empty());
    // A node off the thread starts a new plan on the primary.
    assert_eq!(
        failures::of_node(&records, "review", "fabro.fallback.plan").len(),
        1
    );
    assert_eq!(sessions(&finished.run_dir, "review"), [
        "openai/gpt-5.6-sol"
    ]);
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}

/// A prompt node (`tab`) runs its one-shot request on the same plan: the
/// primary's error moves the same messages to the next provider, and the
/// repair turn stays on that plan.
#[tokio::test]
async fn a_prompt_node_fails_over_and_repairs_on_its_plan() {
    let mut case = Case::new("fallback-prompt-node");
    let model_openai = model(OPENAI);
    let openai = Twin::start(OPENAI, &case.root.join("twin-openai"), vec![scenario(
        OPENAI,
        &case.credential,
        "primary-down",
        model_openai,
        "Summarize",
        error(
            429,
            "rate_limit_error",
            "insufficient_quota",
            "credit spent",
        ),
    )])
    .await;
    let anthropic = Twin::start(ANTHROPIC, &case.root.join("twin-anthropic"), vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "malformed",
            model(ANTHROPIC),
            "Summarize",
            text("not json"),
        ),
        scenario(
            ANTHROPIC,
            &case.credential,
            "repaired",
            model(ANTHROPIC),
            "did not satisfy the output contract",
            text(r#"{"outcome": "succeeded", "context_updates": {"summary": "short"}}"#),
        ),
    ])
    .await;
    case.redirect(&openai);
    case.redirect(&anthropic);
    let workflow = case.workflow(
        &format!(
            r#"digraph Prompted {{
    graph [backend="api", goal="Summarize the interview"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    summary [shape=tab, prompt="Summarize the interview.", model="{model_openai}", provider="openai", output_schema="routing", on_failure="exit"]
    start -> summary -> exit
}}"#
        ),
        Some(chain_openai_to_anthropic()),
    );
    let finished = case.run_with(&workflow, &[], no_client_retries()).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(openai.consumed(), ["primary-down"]);
    assert_eq!(anthropic.consumed(), ["malformed", "repaired"]);
    assert_eq!(finished.final_context()["summary"], json!("short"));
    let records = failures::records(&finished.run_dir);
    assert_eq!(plan_routes_of(&records, "summary"), [
        "openai/gpt-5.6-sol",
        "anthropic/claude-sonnet-5"
    ]);
    // A prompt node runs no session, so no Pebble event describes its
    // move: it is reported on the node's stderr, and the calls that
    // answered are counted on `fabro.prompt.completed`.
    assert!(
        finished.stderr.contains(
            "model fallback: openai/gpt-5.6-sol failed (quota_exceeded); continuing on \
             anthropic/claude-sonnet-5 (attempt 1 of the plan)"
        ),
        "{}",
        finished.stderr
    );
    let completed = &failures::of_node(&records, "summary", "fabro.prompt.completed")[0];
    assert_eq!(completed["outcome"], json!("succeeded"));
    assert_eq!(completed["calls"], json!(2), "{completed}");
    assert_eq!(completed["repairs"], json!(1), "{completed}");
    finished.assert_no_leaked_processes().await;
    openai.stop();
    anthropic.stop();
}
