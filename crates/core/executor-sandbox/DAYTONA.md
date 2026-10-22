# The live Daytona tier

Petri's Daytona tests run against a real Daytona account. They are the port of
the 22 live tests Fabro had before it moved onto Petri (Fabro's
`lib/components/fabro-workflow/tests/it/daytona_integration.rs`, gated by
`#[e2e_test(live("DAYTONA_API_KEY"))]`). This page says how the tier runs,
maps every Fabro test onto its Petri test or the reason it has none, and lists
what the port left open.

## How the tier runs

Every Daytona test starts with `testkit::is_daytona_ready()`. The check
mirrors the Docker battery's `is_docker_ready()` and `PETRI_REQUIRE_DOCKER`:

- Without a credential (`DAYTONA_API_KEY` or `DAYTONA_JWT_TOKEN`) or without
  a Daytona plugin whose backend accepts it, the test prints a `skipping:`
  notice with the reason and returns. The routine suite (`mise run test`,
  `mise run check`) therefore passes on a machine with no account.
- With `PETRI_REQUIRE_DAYTONA=1`, the same absence fails the test with the
  reason in the message: no credential, a plugin that is not where
  `PETRI_SANDBOX_DAYTONA_PLUGIN` says, or a backend that rejected the key.
  A silently skipped live battery would be indistinguishable from a passing
  one.

The plugin comes from `PETRI_SANDBOX_DAYTONA_PLUGIN`. `mise run
plugins:build:daytona` installs it under `target/plugins/bin` beside the host
and Docker plugins, from the sandbox-driver revision the workspace pins. The
plugin receives `DAYTONA_API_KEY`, `DAYTONA_JWT_TOKEN`,
`DAYTONA_ORGANIZATION_ID`, `DAYTONA_API_URL` and `DAYTONA_TARGET` from the
environment and nothing else.

`mise run test:daytona` is the tier's entry point. It builds the Daytona
plugin, sets `PETRI_REQUIRE_DAYTONA=1`, and runs every test whose binary or
name says `daytona`:

```sh
DAYTONA_API_KEY=... mise run test:daytona
```

The tests run in the `daytona` Nextest group, three at a time. Each live test
creates one container sandbox with 2 CPUs, 4096 MiB of memory and provider-selected
disk from the shared runner snapshot (`petri-runner-<digest>`,
named by image, resources, kind and region). The first run on an account
builds that snapshot, which can take up to fifteen minutes; later runs reuse
it. Every test releases what it created and then lists the account by the
run's labels to check nothing was left behind. Sandboxes are billable; the
tier creates about fourteen sandboxes per run.

CI does not run the tier: no runner has the credential. The tier is run by a
developer with an account, before a change to the executor's Daytona path or
to the sandbox-driver pin lands. The unit tests of the Daytona spec
(`src/daytona_tests.rs`, `src/snapshots.rs`, `src/backend.rs`) run
everywhere, as does `a_missing_daytona_plugin_fails_the_scope_routably`.

The observer the tests compare against is `testkit::DaytonaObserver`. It
launches its own plugin from the same environment and lists the run's
sandboxes by their labels, reads and writes files through a second
attachment, and reports their state. It plays the part `docker inspect` plays
for the Docker battery.

## Where the tests are

| Suite | Tests |
| --- | --- |
| `crates/core/executor-sandbox/tests/daytona_backend.rs` | the executor over the Daytona plugin: a process step in the runner sandbox, signals and timeouts, workspace files over the wire, a nested container job with a one-shot action, retention and reattachment, crash recovery and the fence, output after idle and under a burst, a preview URL to a port, a bad image, a missing plugin |
| `crates/petri/lib/tests/daytona.rs` | the standalone host on `--backend daytona`: the run's records and `prune`, and `resume` fencing the crashed sandbox |
| `crates/petri/cli/tests/fabro_terminal_blackbox.rs` | `a_daytona_run_keeps_its_sandbox_after_success_and_prune_removes_it`: the binary on `--backend daytona`, the reported sandbox and the reported prune command |
| `crates/github/acceptance/tests/backend_e2e.rs` | `daytona_runs_process_and_container_jobs_with_javascript_and_docker_actions`: a GitHub Actions workflow with JavaScript and Docker actions on a sandbox job and on a nested container job |

## The mapping

Fabro's tests in file order. "Petri test" names the test in
`daytona_backend.rs` unless a path says otherwise.

| # | Fabro test | What it proved | Petri test, or why there is none |
| --- | --- | --- | --- |
| 1 | `daytona_exec_command` | create a sandbox, run `echo hello`, exit code 0 and the output | `a_process_step_runs_in_the_runner_sandbox` |
| 2 | `daytona_exec_command_with_pipe` | a shell pipeline runs (`echo hello world \| wc -w` is 2) | `a_process_step_runs_in_the_runner_sandbox` |
| 3 | `daytona_exec_command_cancelled` | a cancel token ends `sleep 10`: no exit code, a cancelled or killed termination | `term_and_a_timeout_end_a_step_in_the_runner_sandbox`: `SIGTERM` ends the step, the signal is reported, and it reaches the step's process group |
| 4 | `daytona_exec_command_local_timeout` | a 100 ms timeout ends `sleep 10` in under three seconds as a timeout | `term_and_a_timeout_end_a_step_in_the_runner_sandbox`: the step's own deadline ends it as `timed_out`, promptly |
| 5 | `daytona_file_round_trip` | write, exists, read, delete a file | `workspace_files_go_over_the_wire`: write with missing parents, read, a missing file, the read limit, a directory listing. Delete is not on `ExecEnv`; a step removes files with a process |
| 6 | `daytona_full_lifecycle` | initialize, the platform is Linux, `pwd` works, the directory lists, delete | `a_process_step_runs_in_the_runner_sandbox` (`pwd`, the release deletes the sandbox) and `workspace_files_go_over_the_wire` (the listing). `ExecEnv` has no platform query; the executor reads the image's `PATH` at acquire instead |
| 7 | `daytona_snapshot_sandbox` | a Dockerfile source with 2 CPUs, 4 GiB and 10 GiB, an idle auto-stop timer, and the tool the build installed | `a_process_step_runs_in_the_runner_sandbox`: the sandbox comes from the `petri-runner-*` snapshot with the requested CPUs, memory and disk, as a container. `a_container_job_runs_nested_inside_the_runner_sandbox`: a job's image is selected. Timers: Petri disables auto-stop, pause, delete and TTL (`daytona_tests::a_process_target_uses_the_vm_and_disables_automatic_stops_and_deletion`). A Dockerfile source is not exposed (gap 3) |
| 8 | `daytona_artifact_sync_uploads_and_rewrites_pointer` | a 150 KiB local artifact is uploaded into the sandbox and its pointer rewritten | the transfer: `workspace_files_go_over_the_wire` writes a 150 KiB file and reads it back. Pointer rewriting is Fabro's artifact scheme |
| 9 | `daytona_pipeline_artifact_offload_and_sync` | a pipeline over Daytona whose output above 100 KiB is offloaded to a blob and resolved | a run over Daytona: `crates/petri/lib/tests/daytona.rs::a_daytona_run_keeps_its_sandbox_stopped_and_prune_deletes_it`. The offload above 100 KiB is the Attractor steps' behaviour, proved on the host in `crates/attractor/steps/tests/steps.rs` and independent of the backend |
| 10 | `daytona_git_checkpoint_remote_emits_events` | one checkpoint commit per stage inside the sandbox, a 40-character SHA on the event and in the checkpoint | Fabro's: checkpoints are Fabro's `ExecutionHooks` over Petri. Fabro's `a_daytona_run_commits_inside_the_sandbox_and_publishes_every_checkpoint` covers it |
| 11 | `daytona_git_checkpoint_without_metadata_branch` | no `fabro/meta` refs; the `Fabro-Run:` trailer without `Fabro-Checkpoint:` | Fabro's, as row 10 |
| 12 | `daytona_asset_collection` | `run.artifacts.include` globs are collected from the remote sandbox into the artifact store, with no scratch cache | Fabro's collection over Petri's `list_directory` and `read_file`, both proved on Daytona in `workspace_files_go_over_the_wire` |
| 13 | `daytona_ssh_access` | an SSH command is offered after initialize | none: gap 1 |
| 14 | `daytona_ssh_access_before_init_fails` | the SSH command is refused before initialize | none: gap 1. Petri has no uninitialized handle; acquire returns a running sandbox |
| 15 | `daytona_clone_private_repo_with_github_app_iat` | a private repository clones with a GitHub App installation token; `CLAUDE.md` and the origin URL are there | Fabro's GitHub App credential broker; Petri has none. A clone into a sandbox is a step (the checkout action or an explicit workflow command), proved on Docker in `crates/github/acceptance/tests/checkout_e2e.rs` |
| 16 | `daytona_clone_public_repo_gets_credentials` | an installed organization's repository gets a token for pushing | Fabro's (`fabro_github`); no sandbox is involved |
| 17 | `daytona_iat_not_installed_gives_clear_error` | a repository the app is not installed on gives a clear error | Fabro's, as row 16 |
| 18 | `daytona_git_push_run_branch_to_origin` | the run branch is pushed to origin after each checkpoint | Fabro's, as row 10 |
| 19 | `daytona_toolbox_idle_diagnostic` | exec keeps working after one, two and three seconds idle | `output_survives_idle_and_a_burst_is_delivered_or_its_loss_is_counted` |
| 20 | `daytona_cp_upload_download_round_trip` | reconnect to a sandbox by its id, upload text and binary files, download them back exactly | `a_kept_sandbox_is_stopped_and_reattached_later` (a later executor reattaches by the run's records) and `workspace_files_go_over_the_wire` (every byte value round-trips). Petri has no `cp` command; the reattachment is the executor's |
| 21 | `daytona_computer_use_browser_screenshot` | the VNC viewer URL, a browser launched on the desktop, noVNC listening | none: gap 2 |
| 22 | `daytona_playwright_mcp_sandbox_transport` | an MCP server launched in the sandbox is reached over its preview URL with the token header, through an agent | `a_preview_url_reaches_a_port_inside_the_sandbox`: the route and its headers reach a server in the sandbox. The MCP transport over such a route is proved on Docker in `crates/petri/cli/tests/fabro_mcp_blackbox.rs` (`a_sandbox_server_in_a_docker_scope_is_reached_through_the_plugins_forward`) and not repeated on Daytona |

Behaviours the task named that no single Fabro test carried:

| Behaviour | Petri test |
| --- | --- |
| reconnect after a restart | `a_reacquire_attaches_and_fences_the_crashed_predecessor`; `crates/petri/lib/tests/daytona.rs::resume_fences_the_crashed_daytona_sandbox` |
| retention and prune | `a_kept_sandbox_is_stopped_and_reattached_later`; `crates/petri/lib/tests/daytona.rs::a_daytona_run_keeps_its_sandbox_stopped_and_prune_deletes_it`; the CLI cell in `fabro_terminal_blackbox.rs` |
| output resync | `output_survives_idle_and_a_burst_is_delivered_or_its_loss_is_counted`: a 5000-line burst arrives whole, or the loss the provider counted is one stderr line and the exit status stands |
| failure modes | `a_bad_image_fails_a_nested_job_routably`; `a_missing_daytona_plugin_fails_the_scope_routably` (runs everywhere); `daytona_tests::a_vm_process_target_with_sidecars_fails_before_snapshot_preparation` |
| regional environments | `a_process_step_runs_in_the_runner_sandbox` checks the sandbox's region when `DAYTONA_TARGET` is set; `snapshots::tests::runner_snapshots_and_sandboxes_use_the_selected_region` covers the snapshot identity. Fabro's environment catalog and its regional settings stay Fabro's |
| GitHub Actions on Daytona | `crates/github/acceptance/tests/backend_e2e.rs`: JavaScript and Docker actions on a sandbox job and a nested container job |

## Gaps

1. **SSH access.** The Daytona provider implements sandbox-driver's
   `SshAccess` facet (sandbox-driver's own live test
   `labels_timers_access_round_trip` covers it). Petri's executor does not
   expose it: `ExecEnv` has no SSH method and the CLI has no command. Fabro's
   two SSH tests have no Petri equivalent until a host asks for one.
2. **VNC.** The provider implements the `Vnc` facet (sandbox-driver's
   `extended_daytona_streams_and_browser_access_cross_the_wire`). The
   executor does not expose it. Fabro's computer-use test has no Petri
   equivalent.
3. **Dockerfile sources.** The provider builds a snapshot from a Dockerfile.
   Petri builds its runner snapshots from an image reference only, selected
   by placement label (`--runner-image LABEL=IMAGE`). A workflow that needs
   tools baked in points at an image that has them.
4. **Partial live validation.** The default container runner has passed
   `a_process_step_runs_in_the_runner_sandbox` and
   `a_container_job_runs_nested_inside_the_runner_sandbox` against the hosted
   service with 2 CPUs, 4096 MiB memory and provider-selected disk. These
   cover process output and exit codes, workspace reads/writes, nested
   Docker, a one-shot container sharing files, and cleanup. A standalone
   `petri run --backend daytona` also passed without resource or class
   overrides. Deletion is asynchronous: the leak check waits up to 30 seconds
   for the list endpoint to reflect it. The full tier, including the signal,
   crash-recovery, streaming and preview cases, still needs live validation.
