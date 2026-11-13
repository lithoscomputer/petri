//! The same action contract on a sandbox without a callback route. Docker
//! covers it locally; the live Daytona tier (`PETRI_REQUIRE_DAYTONA`, see
//! `crates/core/executor-sandbox/DAYTONA.md`) adds Daytona process and job
//! targets.

mod support;

use std::env;

use acceptance::runs::RUNNER_IMAGE_2404;
use executor_sandbox::{RoutingExecutor, SandboxCatalog};
use runtime::executor::Retention;
use runtime::ir::RunStatus;
use runtime::{DaytonaResources, DaytonaSandboxKind, SandboxBackend, SandboxOptions};
use support::*;
use testkit::{RunDir, is_daytona_ready, is_docker_ready};

const WORKFLOW: &str = r#"
on: push
jobs:
  job:
    runs-on: ubuntu-24.04
    CONTAINER
    steps:
      - run: |
          mkdir -p .github/actions/probe
          cat > .github/actions/probe/action.yml <<'ACTION'
          name: probe
          inputs:
            require_service:
              default: 'false'
          runs:
            using: node20
            main: index.js
          ACTION
          cat > .github/actions/probe/index.js <<'JS'
          const fs = require('fs');
          if (process.env.ACTIONS_RUNTIME_TOKEN || process.env.ACTIONS_RESULTS_URL) {
            throw new Error('an unreachable callback was advertised');
          }
          if (process.env.INPUT_REQUIRE_SERVICE === 'true') {
            throw new Error('ObjectService is unavailable on this backend');
          }
          fs.appendFileSync(process.env.GITHUB_OUTPUT, 'message=ordinary-action-ran\n');
          console.log('ordinary-action-ran');
          JS
      - id: regular
        uses: ./.github/actions/probe
      - id: service
        continue-on-error: true
        uses: ./.github/actions/probe
        with:
          require_service: 'true'
      - uses: docker://alpine:3.20
        with:
          args: sh -c 'test -z "$ACTIONS_RUNTIME_TOKEN"; echo shared > "$GITHUB_WORKSPACE/one-shot"; echo docker-action-ran'
      - run: |
          test "${{ steps.regular.outputs.message }}" = ordinary-action-ran
          test "${{ steps.service.outcome }}" = failure
          test "$(cat one-shot)" = shared
          echo workflow-continued
"#;

fn workflow(container: bool) -> String {
    WORKFLOW.replace(
        "CONTAINER",
        &if container {
            format!("container: {RUNNER_IMAGE_2404}")
        } else {
            String::new()
        },
    )
}

fn check_report(report: &RunReportPlus) {
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.history()
    );
    let lines = log_lines(report);
    for expected in [
        "ordinary-action-ran",
        "docker-action-ran",
        "workflow-continued",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing {expected}: {lines:?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|line| line.contains("ObjectService is unavailable on this backend")),
        "{lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_actions_run_without_advertising_an_unreachable_results_service() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("actions-without-callback");
    let options = SandboxOptions {
        backend: SandboxBackend::Docker,
        ..Default::default()
    };
    // No route back to this machine: the sandbox cannot reach a results
    // service, so none may be advertised.
    let catalog = SandboxCatalog::from_options(&options)
        .expect("catalog")
        .with_host_address("docker", None);
    let executor = RoutingExecutor::with_catalog(catalog, dir.path(), Retention::Never, options);
    let rt = with_object_service(runtime(dir.path()).executor(executor), None);
    let report = rt
        .run(with_params(lower_ok(&workflow(true))))
        .await
        .expect("replay matches");
    check_report(&report.into());
}

/// Skips without a Daytona credential and plugin, unless
/// `PETRI_REQUIRE_DAYTONA` says the tier must run. Creates billable
/// sandboxes: one runner and one runner with a nested job container.
#[tokio::test(flavor = "multi_thread")]
async fn daytona_runs_process_and_container_jobs_with_javascript_and_docker_actions() {
    if !is_daytona_ready().await {
        return;
    }
    let kind = env::var("PETRI_TEST_DAYTONA_KIND")
        .unwrap_or_else(|_| "container".to_owned())
        .parse::<DaytonaSandboxKind>()
        .expect("PETRI_TEST_DAYTONA_KIND must be vm or container");
    let mut resources = DaytonaResources::default();
    if let Ok(value) = env::var("PETRI_TEST_DAYTONA_CPUS") {
        resources.cpu_cores = value
            .parse()
            .expect("PETRI_TEST_DAYTONA_CPUS is an integer");
    }
    if let Ok(value) = env::var("PETRI_TEST_DAYTONA_MEMORY_MB") {
        resources.memory_mb = value
            .parse()
            .expect("PETRI_TEST_DAYTONA_MEMORY_MB is an integer");
    }
    if let Ok(value) = env::var("PETRI_TEST_DAYTONA_DISK_MB") {
        resources.disk_mb = Some(
            value
                .parse()
                .expect("PETRI_TEST_DAYTONA_DISK_MB is an integer"),
        );
    }
    for container in [false, true] {
        let dir = RunDir::new(if container {
            "daytona-container-actions"
        } else {
            "daytona-process-actions"
        });
        let rt = runtime(dir.path());
        let mut options = rt.run_options().clone();
        options.sandbox.backend = SandboxBackend::Daytona;
        options.sandbox.daytona_kind = kind;
        options.sandbox.daytona_resources = resources;
        let rt = with_object_service(rt.options(options), None);
        let report = rt
            .run(with_params(lower_ok(&workflow(container))))
            .await
            .expect("replay matches");
        check_report(&report.into());
    }
}
