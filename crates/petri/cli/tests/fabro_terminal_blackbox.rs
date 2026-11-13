//! Readiness item 2, the terminal presentation and interaction items, through
//! the shipped binary: retry notices on the firing that retries, branch
//! output attributed to its invocation with secrets masked, a bounded echo
//! per stage while the log keeps everything, `--interactive` for every
//! question type with invalid input re-asked and missing input failing
//! closed, and container (Docker) retention with the reported
//! `petri sandbox prune` command removing the sandbox after success, failure,
//! and cancellation, with the success case repeated on Daytona in the live
//! tier. No Fabro, no database, no server.

mod support;

use std::fs;
use std::process::Stdio;

use serde_json::json;
use support::fabro::interview;
use support::fabro::launch::{Case, Launch};
use tokio::process::Command;

/// A gate whose answer is withheld past its deadline fails with Fabro's
/// retry outcome; the retry is announced under the gate's own tag, and the
/// second attempt's answer routes the run.
#[tokio::test]
async fn a_retry_is_announced_on_the_firing_that_retries() {
    let case = Case::new("terminal-retry");
    let workflow = case.workflow(
        r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no", timeout="300ms", max_retries=1]
    ship [shape=parallelogram, script="echo shipped"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    ship -> exit
}"#,
        None,
    );
    let script = interview::write(&case.root, "retry", &[
        json!({
            "id": "first-ask-withheld",
            "match": { "node": "gate", "ask": 1 },
            "action": { "kind": "withhold" }
        }),
        json!({
            "id": "second-ask-answered",
            "match": { "node": "gate", "ask": 2 },
            "action": { "kind": "choice", "value": "Y" }
        }),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let notices: Vec<_> = finished
        .echoed()
        .into_iter()
        .filter(|(node, line)| node == "gate" && line.starts_with("retry: attempt 2 of 2 in "))
        .collect();
    assert_eq!(notices.len(), 1, "one retry notice: {}", finished.stderr);
    let receipt = finished.receipt();
    let asks: Vec<u64> = receipt["questions"]
        .as_array()
        .expect("questions")
        .iter()
        .map(|q| q["ask"].as_u64().unwrap_or(0))
        .collect();
    assert_eq!(asks, [1, 2], "{receipt}");
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    finished.assert_no_leaked_processes().await;
}

/// Two branches print interleaved lines: every line carries the branch's
/// invocation ahead of its node, the run's own stages carry none, and a
/// secret a branch prints is masked before it reaches the terminal.
#[tokio::test]
async fn branch_output_is_attributed_to_its_invocation_and_secrets_stay_masked() {
    let case = Case::new("terminal-branches");
    let workflow = case.workflow(
        r#"digraph Par {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    fork [shape=component]
    a [shape=parallelogram, script="for i in 1 2 3; do echo a$i token=$TOKEN; sleep 0.05; done"]
    b [shape=parallelogram, script="for i in 1 2 3; do echo b$i; sleep 0.05; done"]
    join [shape=tripleoctagon]
    after [shape=parallelogram, script="echo after"]
    start -> fork
    fork -> a
    fork -> b
    a -> join
    b -> join
    join -> after -> exit
}"#,
        Some(
            "[run.environment]\nid = \"branchy\"\n\n[environments.branchy]\nprovider = \"local\"\n\n[environments.branchy.env]\nTOKEN = \"{{ secrets.BRANCH_TOKEN }}\"\n",
        ),
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            env: vec![(
                "PETRI_SECRET_BRANCH_TOKEN".into(),
                "very-secret-9876".into(),
            )],
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let tags = finished.echoed_tags();
    let of = |node: &str| -> Vec<String> {
        tags.iter()
            .filter(|(tag, _)| {
                let name = tag.rsplit('/').next().unwrap_or(tag);
                name.split('#').next() == Some(node)
            })
            .map(|(tag, _)| tag.split('/').next().unwrap_or("").to_owned())
            .collect()
    };
    let a = of("a");
    let b = of("b");
    assert_eq!(a.len(), 3, "{tags:?}");
    assert_eq!(b.len(), 3, "{tags:?}");
    assert!(
        a.iter().all(|p| p.starts_with("invocation-") && p == &a[0]),
        "every line of `a` names one invocation: {a:?}"
    );
    assert!(
        b.iter().all(|p| p.starts_with("invocation-") && p == &b[0]),
        "every line of `b` names one invocation: {b:?}"
    );
    assert_ne!(a[0], b[0], "the two branches are different invocations");
    let after: Vec<_> = tags
        .iter()
        .filter(|(tag, _)| tag.starts_with("after#"))
        .collect();
    assert_eq!(
        after.len(),
        1,
        "the run's own stage carries no prefix: {tags:?}"
    );
    assert!(
        !finished.stderr.contains("very-secret-9876"),
        "the secret never reaches the terminal: {}",
        finished.stderr
    );
    assert!(
        tags.iter()
            .any(|(_, text)| text.starts_with("a1 token=") && !text.ends_with("token=")),
        "the masked value stands in for the secret: {tags:?}"
    );
    finished.assert_no_leaked_processes().await;
}

/// A stage that prints far more than the terminal should carry: the echo
/// stops at the bound with one marker naming the log, and the log holds
/// every line the run captured (the step's own `command.output`, kept under
/// the offload threshold so it stays inline). Under a loaded machine the
/// sandbox output path can lose lines before they reach the run; the
/// assertions follow what the run received, so the bound is proven whenever
/// enough output arrived and the log invariant always.
#[tokio::test]
async fn a_stages_echo_is_bounded_while_its_log_keeps_everything() {
    let case = Case::new("terminal-bounded");
    // 450 lines of 200 digits: 90 KB, past the 64 KiB echo bound and under
    // the 100 KiB offload threshold.
    let workflow = case.workflow(
        r#"digraph Big {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    big [shape=parallelogram, script="seq -f '%0200g' 1 450"]
    small [shape=parallelogram, script="echo small"]
    start -> big -> small -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    let document = finished.inspect();
    let nodes = &document["executions"][0]["engine"]["context"]["nodes"];
    let captured: Vec<&str> = nodes["big"]["output"]["stdout"]
        .as_str()
        .expect("the big stage's stdout stays inline")
        .lines()
        .collect();
    let captured_bytes: usize = captured.iter().map(|l| l.len()).sum();
    let echoed = finished.echoed();
    let big: Vec<_> = echoed.iter().filter(|(node, _)| node == "big").collect();
    let markers: Vec<_> = big
        .iter()
        .filter(|(_, line)| line.contains("[echo truncated after 65536 bytes; the full log is "))
        .collect();
    if captured_bytes > 65_536 {
        assert_eq!(markers.len(), 1, "one marker: {}", finished.stderr);
        assert!(
            big.len() < captured.len() && big.len() > 100,
            "the echo stopped at the bound: {} of {} lines",
            big.len(),
            captured.len()
        );
        assert!(
            big.iter().any(|(_, line)| line.ends_with("0001"))
                && !big.iter().any(|(_, line)| line.ends_with("0450")),
            "the first lines reached the terminal, the last did not"
        );
    } else {
        // Output was lost upstream of the run (a loaded machine): nothing to
        // bound, and every received line was echoed.
        assert!(markers.is_empty(), "{}", finished.stderr);
        assert_eq!(big.len(), captured.len());
    }
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "small" && line == "small"),
        "the next stage's echo is unaffected: {echoed:?}"
    );
    let path = markers.first().map_or_else(
        || {
            finished
                .run_dir
                .join("logs")
                .join("big-2.log")
                .to_string_lossy()
                .into_owned()
        },
        |(_, line)| {
            line.rsplit("the full log is ")
                .next()
                .and_then(|rest| rest.strip_suffix(']'))
                .expect("the marker names the log")
                .to_owned()
        },
    );
    let log = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert_eq!(
        log.lines().count(),
        captured.len(),
        "the persisted log keeps every line the run captured"
    );
    assert!(log.lines().all(|l| l.starts_with("[out] ")));
    finished.assert_no_leaked_processes().await;
}

const PICK: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    pick [shape=hexagon, label="Which?", question_type="multi_select"]
    apply [shape=parallelogram, script="echo apply"]
    review [shape=parallelogram, script="echo review"]
    start -> pick
    pick -> apply [label="[A] Apply"]
    pick -> review [label="[R] Review"]
    apply -> exit
    review -> exit
}"#;

const GATE: &str = r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#;

/// `--interactive` on a `multi_select` gate: comma-separated keys, every
/// key recorded, the first routes.
#[tokio::test]
async fn interactive_multi_select_takes_comma_separated_keys() {
    let case = Case::new("interactive-multi");
    let workflow = case.workflow(PICK, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some("A, R\n".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert!(
        finished
            .stderr
            .contains("(several keys, separated by commas)"),
        "{}",
        finished.stderr
    );
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("A,R"));
    let receipt = finished.receipt();
    assert_eq!(
        receipt["questions"][0]["reply"]["choices"],
        json!(["A", "R"])
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"apply".to_owned()) && !nodes.contains(&"review".to_owned()));
    finished.assert_no_leaked_processes().await;
}

/// `--interactive` on a `freeform` gate: one line of text is the answer, it
/// routes on the freeform edge and lands in the context.
#[tokio::test]
async fn interactive_freeform_takes_a_line_of_text() {
    let case = Case::new("interactive-freeform");
    let workflow = case.workflow(
        r#"digraph Free {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    ask [shape=hexagon, label="Any notes?", question_type="freeform"]
    note [shape=parallelogram, script="echo noted"]
    start -> ask
    ask -> note [freeform=true]
    note -> exit
}"#,
        None,
    );
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some("looks good to me\n".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(
        receipt["questions"][0]["reply"]["text"],
        json!("looks good to me"),
        "{receipt}"
    );
    assert_eq!(receipt["questions"][0]["kind"], json!("freeform"));
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("freeform"));
    assert_eq!(context["human.gate.ask.answer"], json!("looks good to me"));
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"note".to_owned()), "{nodes:?}");
    finished.assert_no_leaked_processes().await;
}

/// Typed input that names no choice is refused with a reason and the
/// question is asked again; the next line answers it.
#[tokio::test]
async fn interactive_invalid_input_is_refused_and_asked_again() {
    let case = Case::new("interactive-invalid");
    let workflow = case.workflow(GATE, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some("maybe\nno\n".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert!(
        finished
            .stderr
            .contains("`maybe` names no choice; the choices are [Y] Yes, [N] No"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        finished.stderr.matches("question [gate]: Ship it?").count(),
        2,
        "asked twice: {}",
        finished.stderr
    );
    assert_eq!(finished.receipt()["questions"][0]["reply"]["choice"], "N");
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"hold".to_owned()), "{nodes:?}");
    finished.assert_no_leaked_processes().await;
}

/// `--interactive` with no terminal input at all (stdin is `/dev/null`): the
/// gate fails closed with an explicit reason, the run fails, and the exit
/// code names the interview problem.
#[tokio::test]
async fn interactive_without_terminal_input_fails_closed_with_a_reason() {
    let case = Case::new("interactive-null");
    let workflow = case.workflow(GATE, None);
    let finished = case.run(&workflow, &["--interactive"]).await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("could not read the answer from standard input: standard input reached EOF"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("standard input is not a terminal; reading the answer from it anyway"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("interview verification failed: 1 problem(s)"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["questions"][0]["reply"]["kind"], "failed");
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(
        !nodes.contains(&"ship".to_owned()) && !nodes.contains(&"hold".to_owned()),
        "the gate fails closed: {nodes:?}"
    );
    finished.assert_no_leaked_processes().await;
}

// ── Container retention ────────────────────────────────────────────────────

const DOCKER_WORKFLOW: &str = r#"digraph Boxed {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'kept in the box\n' > notes.txt && echo prepared"]
    verify [shape=parallelogram, script="cat notes.txt"]
    start -> prepare -> verify -> exit
}"#;

/// The file a stopped container's workspace holds, through `docker cp`.
async fn file_in_container(container: &str, path: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["cp", &format!("{container}:{path}"), "-"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `docker cp ... -` writes a tar stream; the file body follows its
    // 512-byte header and is NUL padded.
    let body = output.stdout.get(512..)?;
    let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
    Some(String::from_utf8_lossy(&body[..end]).into_owned())
}

/// The retained sandbox a Docker run reported: its container name, checked
/// to exist and to be stopped; the prune command it printed; and the
/// reported provider.
async fn retained_container(finished: &support::fabro::launch::Finished) -> String {
    let sandboxes = finished.reported_sandboxes();
    assert_eq!(sandboxes.len(), 1, "{}", finished.stderr);
    let (_, provider, _) = &sandboxes[0];
    assert_eq!(provider, "docker", "{}", finished.stderr);
    assert!(
        finished.stderr.contains(&format!(
            "(delete with `petri sandbox prune --run-dir {}`)",
            finished.run_dir.display()
        )),
        "the retrieval command is reported: {}",
        finished.stderr
    );
    let name = testkit::sandbox_name(&finished.run_dir, 0);
    assert!(
        testkit::container_id(&name).await.is_some(),
        "the container {name} is retained"
    );
    assert!(
        !testkit::container_is_running(&name).await,
        "the retained container {name} is stopped"
    );
    name
}

/// Run the reported prune command through the binary and check the
/// container is gone.
async fn prune_removes(case: &Case, name: &str) {
    let (code, stderr) = case.prune().await;
    assert_eq!(code, Some(0), "prune failed:\n{stderr}");
    assert!(
        testkit::container_id(name).await.is_none(),
        "prune removed the container {name}: {stderr}"
    );
    assert!(
        stderr.contains("deleted") || stderr.contains("pruned"),
        "{stderr}"
    );
}

/// A Fabro run on `--backend docker` keeps its sandbox after success, the
/// file it wrote is still in the stopped container, and the reported prune
/// command deletes it.
#[tokio::test]
async fn a_docker_run_keeps_its_sandbox_after_success_and_prune_removes_it() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let case = Case::new("docker-success").docker();
    let workflow = case.workflow(DOCKER_WORKFLOW, None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "verify" && line == "kept in the box"),
        "{}",
        finished.stderr
    );
    let name = retained_container(&finished).await;
    assert_eq!(
        file_in_container(&name, "/workspace/notes.txt")
            .await
            .as_deref(),
        Some("kept in the box\n"),
        "the retained workspace is accessible after teardown"
    );
    assert_eq!(
        finished.final_context()["command.output"],
        json!("kept in the box\n")
    );
    finished.assert_no_leaked_processes().await;
    prune_removes(&case, &name).await;
}

/// A failed Docker run keeps its sandbox too, with the work done before the
/// failure, and prune removes it.
#[tokio::test]
async fn a_failed_docker_run_keeps_its_sandbox_and_prune_removes_it() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let case = Case::new("docker-failure").docker();
    let workflow = case.workflow(
        r#"digraph Boxed {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'before the failure\n' > notes.txt"]
    broken [shape=parallelogram, script="echo boom >&2; exit 3", on_failure="exit"]
    start -> prepare -> broken -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    let name = retained_container(&finished).await;
    assert_eq!(
        file_in_container(&name, "/workspace/notes.txt")
            .await
            .as_deref(),
        Some("before the failure\n")
    );
    finished.assert_no_leaked_processes().await;
    prune_removes(&case, &name).await;
}

/// A cancelled Docker run stops its work, keeps its sandbox with the work
/// done so far, and prune removes it.
#[tokio::test]
async fn a_cancelled_docker_run_keeps_its_sandbox_and_prune_removes_it() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let case = Case::new("docker-cancel").docker();
    let workflow = case.workflow(
        r#"digraph Boxed {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'started\n' > notes.txt"]
    slow [shape=parallelogram, script="echo working; sleep 60; echo finished > finished.txt"]
    start -> prepare -> slow -> exit
}"#,
        None,
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when_container_file: Some("/workspace/notes.txt".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    let name = retained_container(&finished).await;
    assert_eq!(
        file_in_container(&name, "/workspace/notes.txt")
            .await
            .as_deref(),
        Some("started\n")
    );
    assert!(
        file_in_container(&name, "/workspace/finished.txt")
            .await
            .is_none(),
        "the cancelled work never finished"
    );
    finished.assert_no_leaked_processes().await;
    prune_removes(&case, &name).await;
}

/// A Fabro run on `--backend daytona` keeps its VM after success, reports
/// it by provider and id with the prune command, leaves it stopped on the
/// account, and the reported prune command deletes it. Skips without a
/// Daytona credential and plugin, unless `PETRI_REQUIRE_DAYTONA` says the
/// tier must run; creates one billable VM.
#[tokio::test]
async fn a_daytona_run_keeps_its_sandbox_after_success_and_prune_removes_it() {
    if !support::fabro::require::daytona().await {
        return;
    }
    let observer = testkit::DaytonaObserver::from_env();
    let case = Case::new("daytona-success").daytona();
    let workflow = case.workflow(DOCKER_WORKFLOW, None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "verify" && line == "kept in the box"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        finished.final_context()["command.output"],
        json!("kept in the box\n")
    );
    let sandboxes = finished.reported_sandboxes();
    assert_eq!(sandboxes.len(), 1, "{}", finished.stderr);
    let (_, provider, id) = &sandboxes[0];
    assert_eq!(provider, "daytona", "{}", finished.stderr);
    assert!(
        finished.stderr.contains(&format!(
            "(delete with `petri sandbox prune --run-dir {}`)",
            finished.run_dir.display()
        )),
        "the retrieval command is reported: {}",
        finished.stderr
    );
    let run_id = testkit::recorded_run_id(&finished.run_dir);
    let status = observer
        .sandbox(&run_id, 0)
        .await
        .expect("the VM is retained on the account");
    assert_eq!(
        status.reference.id.as_str(),
        id,
        "the reported id is the provider's"
    );
    assert!(
        observer.eventually_stopped(&run_id, 0).await,
        "the retained VM is stopped: {status:?}"
    );
    finished.assert_no_leaked_processes().await;

    let (code, stderr) = case.prune().await;
    assert_eq!(code, Some(0), "prune failed:\n{stderr}");
    assert!(
        stderr.contains("deleted") || stderr.contains("pruned"),
        "{stderr}"
    );
    assert!(
        observer.eventually_gone(&run_id).await.is_empty(),
        "prune removed the VM: {stderr}"
    );
    observer.shutdown().await;
}
