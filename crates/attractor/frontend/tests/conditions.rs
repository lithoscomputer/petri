//! Acceptance §7 item 4: the condition grammar against Fabro's documented
//! semantics, evaluated through the engine's own `eval`.

mod support;

use serde_json::{Value, json};
use support::*;

/// Lower one condition on an edge and evaluate it for `status`, `output`
/// and `kv`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "call sites read as the table they are: a literal outcome per row"
)]
fn holds(condition: &str, status: &str, output: Value, kv: &[(&str, Value)]) -> bool {
    let text = dot(&format!(
        "a [prompt=\"x\"]\nb [prompt=\"x\"]\nstart -> a\na -> exit [condition=\"{}\"]\na -> b\nb -> exit",
        condition.replace('"', "\\\"")
    ));
    let graph = lower_ok(&text);
    let tiers = tiers(&graph, "a");
    let guard = tiers[0].1[0].1;
    eval_guard(&graph, guard, &statics(status, &output), kv)
}

fn rejected(condition: &str) -> Vec<String> {
    codes(&dot(&format!(
        "a [prompt=\"x\"]\nb [prompt=\"x\"]\nstart -> a\na -> exit [condition=\"{condition}\"]\na -> b\nb -> exit"
    )))
}

#[test]
fn outcome_comparisons_cover_the_four_values_and_fold_engine_statuses() {
    assert!(holds("outcome=succeeded", "success", json!({}), &[]));
    assert!(!holds(
        "outcome=succeeded",
        "partial_success",
        json!({}),
        &[]
    ));
    assert!(holds(
        "outcome=partially_succeeded",
        "partial_success",
        json!({}),
        &[]
    ));
    assert!(holds("outcome=skipped", "skipped", json!({}), &[]));
    for tag in ["failure", "cancelled", "timed_out"] {
        assert!(
            holds("outcome=failed", tag, json!({}), &[]),
            "{tag} is failed"
        );
        assert!(!holds("outcome!=failed", tag, json!({}), &[]));
    }
    assert!(holds("outcome!=failed", "success", json!({}), &[]));
    assert!(holds("outcome=\"succeeded\"", "success", json!({}), &[]));
}

#[test]
fn unknown_outcome_values_are_rejected_and_custom_signals_ride_context() {
    assert!(rejected("outcome=error").contains(&"unsupported.outcome_value".to_string()));
    assert!(rejected("outcome > 1").contains(&"attractor.condition.outcome_op".to_string()));
    assert!(holds("context.verdict=error", "success", json!({}), &[(
        "verdict",
        json!("error")
    )]));
    assert!(holds("verdict=error", "success", json!({}), &[(
        "verdict",
        json!("error")
    )]));
}

#[test]
fn context_keys_compare_as_fabro_text() {
    assert!(holds("context.tests_passed=true", "success", json!({}), &[
        ("tests_passed", json!(true))
    ]));
    assert!(holds("context.tests_passed=true", "success", json!({}), &[
        ("tests_passed", json!("true"))
    ]));
    assert!(holds("context.n=42", "success", json!({}), &[(
        "n",
        json!(42)
    )]));
    assert!(!holds("missing=something", "success", json!({}), &[]));
    assert!(holds("missing=", "success", json!({}), &[]));
    assert!(holds(
        "context.loop_state=exhausted",
        "success",
        json!({}),
        &[("loop_state", json!("exhausted"))]
    ));
    assert!(holds(
        "context.msg=\"hello world\"",
        "success",
        json!({}),
        &[("msg", json!("hello world"))]
    ));
}

#[test]
fn preferred_label_reads_the_reported_label() {
    assert!(holds(
        "preferred_label=Fix",
        "success",
        json!({ "preferred_label": "Fix" }),
        &[]
    ));
    assert!(!holds("preferred_label=Fix", "success", json!({}), &[]));
}

#[test]
fn truthiness_follows_the_three_fabro_rules() {
    assert!(holds("flag", "success", json!({}), &[(
        "flag",
        json!("yes")
    )]));
    assert!(!holds("flag", "success", json!({}), &[]));
    assert!(!holds("flag", "success", json!({}), &[(
        "flag",
        json!("false")
    )]));
    assert!(!holds("flag", "success", json!({}), &[(
        "flag",
        json!("0")
    )]));
    assert!(!holds("flag", "success", json!({}), &[(
        "flag",
        json!(false)
    )]));
    assert!(holds("flag", "success", json!({}), &[("flag", json!(1))]));
    assert!(holds("!missing", "success", json!({}), &[]));
}

#[test]
fn numeric_comparisons_parse_both_sides() {
    assert!(holds("context.score > 80", "success", json!({}), &[(
        "score",
        json!(90)
    )]));
    assert!(!holds("context.score > 80", "success", json!({}), &[(
        "score",
        json!(70)
    )]));
    assert!(holds("context.score >= 80", "success", json!({}), &[(
        "score",
        json!("80")
    )]));
    assert!(holds("context.count < 5", "success", json!({}), &[(
        "count",
        json!(3)
    )]));
    assert!(holds("context.ratio <= 0.75", "success", json!({}), &[(
        "ratio",
        json!(0.75)
    )]));
    assert!(!holds("context.score > 80", "success", json!({}), &[(
        "score",
        json!("nope")
    )]));
    assert!(
        !holds("context.score > 80", "success", json!({}), &[]),
        "missing is not a number"
    );
}

#[test]
fn contains_and_matches() {
    assert!(holds(
        "context.message contains error",
        "success",
        json!({}),
        &[("message", json!("an error occurred"))]
    ));
    assert!(!holds(
        "context.message contains Error",
        "success",
        json!({}),
        &[("message", json!("an error occurred"))]
    ));
    assert!(holds(
        "context.tags contains urgent",
        "success",
        json!({}),
        &[("tags", json!(["urgent", "low"]))]
    ));
    assert!(!holds(
        "context.tags contains critical",
        "success",
        json!({}),
        &[("tags", json!(["urgent"]))]
    ));
    assert!(holds(
        "context.version matches ^v\\d+",
        "success",
        json!({}),
        &[("version", json!("v2.0"))]
    ));
    assert!(!holds(
        "context.version matches ^v\\d+",
        "success",
        json!({}),
        &[("version", json!("beta"))]
    ));
    assert!(rejected("x matches [bad").contains(&"attractor.condition.regex".to_string()));
}

#[test]
fn boolean_operators_and_precedence() {
    let kv = [("a", json!("0")), ("b", json!("2")), ("c", json!("3"))];
    assert!(holds("a=1 && b=2 || c=3", "success", json!({}), &kv));
    assert!(!holds("a=1 || b=2 && c=0", "success", json!({}), &kv));
    assert!(holds("outcome=succeeded && b=2", "success", json!({}), &kv));
    assert!(holds("!outcome=failed && c=3", "success", json!({}), &kv));
    assert!(rejected("a=1 b=2").contains(&"attractor.condition.syntax".to_string()));
}

/// REMOVE AFTER 2026-10-04 with the alias itself.
#[test]
fn outcome_success_is_read_as_succeeded_with_a_dated_warning() {
    assert!(holds("outcome=success", "success", json!({}), &[]));
    assert!(!holds("outcome=success", "failure", json!({}), &[]));
    let found = codes(&dot(
        "a [prompt=\"x\"]\nb [prompt=\"x\"]\nstart -> a\na -> exit [condition=\"outcome=success\"]\na -> b\nb -> exit",
    ));
    assert!(
        found.contains(&"deprecated.outcome_alias".to_string()),
        "{found:?}"
    );
    assert!(
        !found.contains(&"unsupported.outcome_value".to_string()),
        "{found:?}"
    );
    assert!(rejected("outcome=failure").contains(&"unsupported.outcome_value".to_string()));
}

/// fabro-51ad: `nodes.<id>.generation` (and the other record fields) lower
/// onto the run-context node record, so a loop's deadlock guard reads the
/// counter. Before the fix the key lowered onto a literal `kv` lookup that
/// never exists, and the guard could not fire.
#[test]
fn nodes_record_fields_read_the_run_context_record() {
    let text = dot(
        "flaky [prompt=\"x\"]\n  start -> flaky\n  flaky -> exit [condition=\"nodes.flaky.generation >= 2\"]\n  flaky -> flaky [label=\"again\"]",
    );
    let graph = lower_ok(&text);
    let tiers = tiers(&graph, "flaky");
    let guard = tiers[0].1[0].1;

    let with_generation = |generation: u32| {
        let mut run = ir::RunContext::new();
        run.record(
            smol_str::SmolStr::new("flaky"),
            ir::NodeRecord {
                status:     ir::Status::Failure(ir::FailureInfo::default()),
                output:     serde_json::json!(null),
                generation: ir::Generation::new(generation),
                attempts:   1,
            },
        );
        let ir::Guard::Expr(id) = guard else {
            panic!("a condition guard");
        };
        let bound = statics("failure", &serde_json::json!(null));
        let env = ir::EvalEnv::new(&serde_json::Value::Null, &run, &bound);
        ir::eval(&graph.exprs, id, &env).unwrap_or_else(|e| panic!("{e}"))
    };

    assert_eq!(
        with_generation(2),
        serde_json::Value::Bool(true),
        "generation 2 satisfies >= 2"
    );
    assert_eq!(
        with_generation(1),
        serde_json::Value::Bool(false),
        "generation 1 does not"
    );
    assert_eq!(
        with_generation(9),
        serde_json::Value::Bool(true),
        "generation 9 satisfies >= 2"
    );
}
