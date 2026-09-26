# Developing Petri

## Setup

Install [Mise](https://mise.jdx.dev/), then install the locked tools and prepare
the pinned Rust Style Guide:

```sh
mise trust
mise install --locked --jobs=1
mise run setup
```

Install Docker to run the container-backed tests. Tests that need Docker skip
when no daemon is reachable.

The GitHub Actions compatibility corpus is optional for normal local work. To
prepare it, install and authenticate the GitHub CLI, then run:

```sh
scripts/corpus-fetch.sh
scripts/corpus-fetch-actions.sh
```

The corpus uses the commits in `crates/github/corpus-pins.txt`. Do not use
`--repin` during a routine fetch.

## Git dependencies

Internal Git dependencies (`lithoscomputer/*`, the twins included) name
exactly `branch = "main"`: not `rev`, and not an omitted ref, which Cargo
treats as a different source. Pick up a newer commit with
`cargo update -p <crate>` and let CI decide; whoever breaks an API another
Lithos repository uses fixes that repository promptly. Fabro's `Cargo.lock`
decides what ships. `mise run check:pins` enforces the form.

All of these repositories are public. `Cargo.toml` names them over HTTPS, so
a build needs no SSH key and no credential, locally or in CI.
`GIT_SSH_COMMAND=false cargo fetch --locked` must pass; it proves no
dependency still needs SSH.

Use a local, untracked Cargo `[patch]` config when working across sibling
checkouts; push the library change to its `main` and update `Cargo.lock`
before sharing the integration.

## Attractor and Fabro

The Attractor crates (`crates/attractor/`) lower and run the language; the
Fabro crates (`crates/fabro/`) read Fabro's settings files around it and prove
compatibility with the pinned Fabro. `fabro` may depend on `attractor`, never
the other way; `crates/petri/lib/tests/layering.rs` enforces the direction.

## Fabro bundles

The Fabro workflow bundles the black box battery runs are vendored under
`crates/fabro/acceptance/bundles/<id>/`, one directory per bundle, with each
file at its path in the source repository. `bundles.lock.json` beside them
records the source repository, revision, and every file's digest;
`bundles/PROVENANCE.md` records the copy and the sources' licenses. Two of
the sources are not public, so nothing fetches them: `mise run check:bundles`
(part of `mise run check` and of every CI job) verifies the tracked files
against the lock and fails on a missing, changed, or extra file. To update a
bundle, follow "Updating a bundle" in `PROVENANCE.md`.

Native Pebble tests use a scripted model and real execution scopes. They need
no provider credentials. `crates/attractor/steps/tests/pebble.rs` covers tool
execution, output repair, model selection, accounting, steering, cancellation,
bounded output capture, and the Pebble environment contract on Host and Docker.

## Common tasks

| Command | Purpose |
| --- | --- |
| `mise run dev` | Build and run the Petri CLI |
| `mise run fmt` | Format Rust code |
| `mise run fmt:check` | Check formatting without changing files |
| `mise run lint` | Run Clippy with warnings denied |
| `mise run test` | Run the routine suite with Nextest, then run doctests |
| `mise run check:msrv` | Check all targets with Rust 1.89 |
| `mise run lean:build` | Build the Lean model in `lean/` and check every proof |
| `mise run test:lean` | Check the engine core against the Lean model, with the model required |
| `mise run check` | Run the complete routine verification gate |
| `mise run check:bundles` | Verify the vendored Fabro bundles against `bundles.lock.json`, digest by digest |
| `mise run check:pins` | Check that internal Git dependencies track `main`, and that `Cargo.lock`, `CONTRACT.md`, and the latest evidence records cite the same revisions |
| `mise run test:fabro:blackbox` | Run the required Fabro black box scenarios and write their evidence records and coverage report |
| `mise run test:fabro:blackbox:repeat` | The same set three times, each in fresh processes under a different schedule |
| `mise run test:fabro:differential` | Compare the shipped binary with the pinned `fabro` binary (built from the corpus on first use, about three minutes) |
| `mise run check:fabro:readiness` | The final readiness gate: the strict black box run, then a coverage report that fails unless every required cell passed, blocked cells included |
| `mise run check:nightly` | Run the extended verification gate |
| `mise run release <target> <version>` | Build a native release archive |

Run `mise run check` before opening a pull request.

## Lean model

`lean/` holds a Lean 4 model of parts of the engine core, the theorems proved
about it, and an executable the engine's `lean_model` tests compare the real
core with; `lean/README.md` explains it. Install
[elan](https://github.com/leanprover/elan) to work on it: elan reads the Lean
version from `lean/lean-toolchain`. Without a built model, `mise run test`
skips the comparison; `mise run test:lean` builds the model and requires it,
as the `lean model` CI job does. A new Lean or elan version must be at least
a day old, like every other tool here.

## Diagnostics

Set `PETRI_LOG` to see tracing output on stderr. The default is `warn`. Use
`PETRI_LOG=debug` for per-step and per-external-call detail:

```sh
PETRI_LOG=debug mise run dev -- run workflow.yml
```

`PETRI_LOG` takes any `tracing_subscriber` filter directive. Diagnostics never
mix with command output on stdout. Tracing fields carry only structural data;
secrets, step output, and environment values are never captured.

## Library and repository gates

The MCP client is Pebble's dependency. A change to Pebble or `lithos-llm`
runs that repository's required checks first; only then does Petri move its lock,
update the "Pinned revisions" table in
`crates/fabro/acceptance/CONTRACT.md`, and rerun the affected black box
scenarios. `mise run check:pins` fails while the citations disagree. A library
test pass never replaces a required Petri scenario. The pending library batch
is listed in `README.md` under "Library and repository gates".

## Rust policy

Petri follows the pinned Brynary Rust Style Guide. Run `mise run setup`, then read
`.ai/style-guides/rust-style-guide/SKILL.md` before changing Rust code,
configuration, project structure, or tests.

Petri uses Rust 2024 and supports Rust 1.89 or newer. Mise pins the minimum
compiler, the development compiler, and the nightly formatter. Tokio owns
asynchronous subprocesses, timers, networking, and orchestration. Keep parsing,
validation, graph transformations, and the engine state machine synchronous
unless a real I/O boundary requires async.

The `petri-cli` crate owns Tokio runtime creation. Library crates can expose
Tokio-based async APIs, but they must not create a process-wide runtime.

## Lints and unsafe code

The root `Cargo.toml` holds the workspace lint tables, and every member crate
opts in with `[lints] workspace = true`. Workspace lints are not inherited
automatically, so a new crate must add that table or the policy does nothing
for it. The root `clippy.toml` allows `unwrap` in tests and on
`std::sync::LockResult`, and nowhere else.

`mise run lint` runs the whole policy with warnings denied. It is the source of
truth. Repair a diagnostic with a small, behavior-preserving code change first.
When the code is intentionally different from the policy, put
`#[expect(LINT, reason = "...")]` on the narrowest item or expression and state
the real constraint in the reason. Do not add a workspace-wide exemption to
silence one site.

Project-written unsafe code is denied by default: `unsafe_code = "deny"` in the
workspace lint table. The host executor module of `petri-executor-sandbox`
(`src/host.rs`) is the only exception. It is limited to POSIX process control
(`killpg`) and the macOS `libproc` queries the host executor uses to observe a
process group without signalling it. The module takes the exception with a
reasoned `#![allow(unsafe_code, ...)]` at its top.

Every unsafe operation carries an adjacent `SAFETY:` comment. The comment must
prove the preconditions that operation relies on: pointer validity, initialized
storage, buffer size, exclusive borrow, and the operating system's own
contract. Keep each unsafe block around only the operation that requires it.
Ordinary filtering and iteration stay outside. Adding unsafe code to any other
crate needs a project decision, not a local attribute.

## Crate policy

The external distribution surface is:

- `crates/petri/lib`, the `petri` library that re-exports the supported API;
- `crates/petri/cli`, the shipped `petri` executable.

All crates under `crates/core/` and `crates/github/` are internal components or
test support. Their manifests set `publish = false`. Treat their public items as
in-repository APIs unless a later project decision gives independent consumers
a direct contract.

Do not publish either distribution crate to crates.io. The binary release
workflow packages the `petri` executable. It does not publish a crate.

## Project structure

`README.md` maps Petri's design documents to the workspace and its tests. Keep
crate dependencies consistent with the layering rules described there.
`crates/petri/lib/tests/layering.rs` enforces the dependency graph.

## Tests and the compatibility corpus

The routine suite runs without Docker or the corpus. It reports skips when those
resources are absent.

Container scopes run through the `sandbox-driver-docker` plugin. `mise run
plugins:build` installs it from the sandbox-driver commit `Cargo.lock` locks, under
`target/plugins/bin`; the test tasks depend on it and set
`PETRI_SANDBOX_DOCKER_PLUGIN` to that path. `mise run plugins:build:daytona`
adds the `sandbox-driver-daytona` plugin for the live Daytona tier
(`SANDBOX_DRIVER_PLUGINS` names the kinds the script installs). To test against
another build of a plugin, set the variable yourself. A plugin this build does
not embed runs only in dev mode, which debug builds turn on; a release build needs
`PETRI_SANDBOX_PLUGIN_DEV=1` or `--sandbox-plugin-dev`.

To build plugins from the sibling sandbox-driver checkout during coordinated
development, use:

```sh
SANDBOX_DRIVER_SOURCE=../sandbox-driver mise run plugins:build
```

Both local and locked Git builds install the provider packages directly.
Each package builds its same-named plugin executable.

Nextest is the normal test runner. `mise run test` also uses Cargo to run
doctests, which Nextest does not run.

The Fabro side has three more fetched assets. `scripts/corpus-fetch-fabro.sh`
fetches the Fabro corpus at the pin in `crates/fabro/corpus-pin.txt`.
`scripts/corpus-fetch-fabro-bundles.sh` materializes the bundle set in
`crates/fabro/acceptance/bundles.lock.json` and verifies every digest; the two
private sources need SSH read access, or `FABRO_BUNDLE_SOURCE_<OWNER>_<REPO>`
pointing at a local checkout (for example
`FABRO_BUNDLE_SOURCE_LITHOSCOMPUTER_CODE_REVIEW=../code-review`).
`scripts/fabro-provision.sh` builds the pinned `fabro` binary from the fetched
corpus into `crates/fabro/corpus/fabro-target/` and checks it reports the pin;
a `fabro` on `PATH` is never used.

CI sets `PETRI_REQUIRE_DOCKER=1` on Linux and `PETRI_REQUIRE_CORPUS=1`,
`PETRI_REQUIRE_FABRO_CORPUS=1`, and `PETRI_REQUIRE_FABRO_BUNDLES=1` on all
runners; the compatibility job also sets `PETRI_REQUIRE_FABRO_BINARY=1`. These
variables turn an unexpected skip into a failure. Do not set them for ordinary
local work unless the resources are available. Without them an absent asset
prints a `skipping:` notice, and the coverage report shows the scenario as
skipped, never as passed.

The live Daytona tier is the one battery CI does not run: no runner has a
Daytona credential. Run it by hand before a change to the executor's Daytona
path or to the locked sandbox-driver commit lands:

```sh
DAYTONA_API_KEY=... mise run test:daytona
```

The task installs the Daytona plugin beside the others (`mise run
plugins:build:daytona`, which sets `PETRI_SANDBOX_DAYTONA_PLUGIN`), sets
`PETRI_REQUIRE_DAYTONA=1`, and runs every test whose binary or name says
`daytona`. `DAYTONA_TARGET`, `DAYTONA_ORGANIZATION_ID` and `DAYTONA_API_URL`
are forwarded to the plugin when set. The tier creates billable sandboxes,
about fourteen VMs, and the first run on an account builds the shared runner
snapshot, which can take fifteen minutes. Without the variable the same tests
skip with a notice, which is what `mise run test` does.
`crates/core/executor-sandbox/DAYTONA.md` describes the tier and maps Fabro's
former live Daytona tests onto it.

Every black box scenario writes an evidence record into `PETRI_EVIDENCE_DIR`
(`mise run test:fabro:blackbox` picks a directory under
`target/fabro-evidence/` and links `latest` to it). Read
`target/fabro-evidence/latest/coverage.md` after a run; a failed scenario's
process output, inspect document, twin logs, and case directory are under
`bundles/<record id>/`. The record format is documented in
`crates/petri/cli/tests/support/fabro/record.rs`.

The corpus contains third-party repositories and is not committed. The corpus
run battery does not receive a GitHub token, so workflows from the corpus cannot
mutate repositories through the GitHub API.

## Continuous integration

Pull requests and pushes to `main` run the routine verification gate on fixed
Linux and macOS runners. The gate checks formatting, Clippy, GitHub Actions,
whitespace, the full test suite, and a release build. Linux also runs the
Docker-backed acceptance tests and checks that Petri leaves no containers
behind.

The `check` jobs also fetch and verify the Fabro bundle set, write the Fabro
coverage report into the job summary, and keep the evidence as artifacts
(`fabro-evidence-<runner>` on success with the records and the report;
`fabro-evidence-<runner>-failed` on failure with every bundle). A second job,
`fabro compatibility (ubuntu-24.04)`, builds the pinned `fabro` binary (cached
by pin and toolchain) and runs the comparison matrix. Both `check (ubuntu-24.04)`,
`check (macos-15)`, and `fabro compatibility (ubuntu-24.04)` are the required
checks for `main`; mark them required in the branch protection rule. Run
`mise run check:actions` (zizmor) after any workflow change.

The scheduled Nightly workflow also checks the Rust 1.89 compiler floor, runs
the long tests, repeats the black box set three times under different
schedules, reruns the comparison matrix (required on Linux x86_64, best effort
on the other runners), and runs the full test suite in release mode. You can start it manually from GitHub
Actions. Nightly includes Linux arm64 coverage. Keep arm64 in Nightly until its
Docker and corpus runs are reliable enough for the routine gate.

The code-review bundle's host scenario cells resolve `python3` from `PATH` and
need PyYAML, which its rule loader imports. Locally, any `python3` on `PATH`
that imports `yaml` satisfies the scenario runner; without one those cells
fail with a message naming the module. CI installs the bundle's own pinned
requirements (`requirements-rules.txt`, with hash checking) into a virtual
environment placed first on `PATH`. A Docker scope needs nothing: the pinned
runner image carries the same modules.

A test that starts a Python `http.server.HTTPServer` must override
`server_bind` so it does not call `socket.getfqdn`. On the macOS runner that
reverse DNS lookup of the bound address can take longer than a test's startup
window, so the server never answers and the failure looks like a slow server
rather than a stalled bind. `crates/fabro/acceptance/testdata/mcp_server.py`
shows the override; a `ThreadingHTTPServer` inherits the same `server_bind`
and needs the same override. The security-review fixture servers are written
into a workspace to be scanned and are never run, so they are exempt.

## Releases

A `v*` tag builds native `petri` archives for macOS arm64, Linux x86_64, and
Linux arm64. The workflow adds a SHA-256 file for each archive and creates a
draft GitHub release. Review and publish the draft manually.

To test packaging locally, run the release task with your native Rust target
and a version that starts with `v`, for example:

```sh
mise run release aarch64-apple-darwin v0.1.0-test
```

The task writes ignored artifacts under `dist-release/`. It refuses to build a
target that does not match the host runner.
