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

## Dependencies and bundle sources

The workspace pins sandbox-driver, Pebble, lithos-llm, and the twins by Git
revision. All four repositories are public, so Cargo fetches them over HTTPS
with no credentials, locally and in CI. Use a local, untracked Cargo `[patch]`
config when working across sibling checkouts; commit a pushed revision in
`Cargo.toml` and regenerate `Cargo.lock` before sharing the integration.

Two Fabro black box bundle sources are not public. CI and nightly workflows
use `.github/actions/private-dependencies` to reach them. The
`sandbox-driver-read` GitHub environment holds these secrets:

- `CODE_REVIEW_DEPLOY_KEY`: read-only deploy key on `lithoscomputer/code-review`.
- `FACTORY_DEPLOY_KEY`: read-only deploy key on `veniceai/factory`, the
  `fix-ci` bundle source.

Neither exists yet. Owner action: create one read-only deploy key pair per
repository (`ssh-keygen -t ed25519 -N '' -f code-review` and the same for
`factory`), add each public key as a read-only deploy key on its repository,
and add each private key as the named secret in the `sandbox-driver-read`
environment. Until then the "Fetch and verify the Fabro bundles" step fails
with a message naming the key; it never skips a bundle. The environment keeps
its historical name; the `SANDBOX_DRIVER_DEPLOY_KEY` secret it still holds is
no longer read and can be deleted.

Use a different key pair for each repository. The action selects each key with
an SSH host alias and checks GitHub's pinned host key. Each workflow removes
the temporary credentials when its job finishes. The release workflow fetches
no bundles and uses neither the action nor the environment.

Native Pebble tests use a scripted model and real execution scopes. They need
no provider credentials. `crates/fabro/steps/tests/pebble.rs` covers tool
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
| `mise run check` | Run the complete routine verification gate |
| `mise run check:pins` | Check that the manifests, `CONTRACT.md`, and the latest evidence records cite the same revisions |
| `mise run test:fabro:blackbox` | Run the required Fabro black box scenarios and write their evidence records and coverage report |
| `mise run test:fabro:blackbox:repeat` | The same set three times, each in fresh processes under a different schedule |
| `mise run test:fabro:differential` | Compare the shipped binary with the pinned `fabro` binary (built from the corpus on first use, about three minutes) |
| `mise run check:nightly` | Run the extended verification gate |
| `mise run release <target> <version>` | Build a native release archive |

Run `mise run check` before opening a pull request.

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

Pebble, `lithos-llm`, and any MCP client library are pinned by revision. A
change to one of them runs that repository's required checks first; only then
does Petri move the pin, update the "Pinned revisions" table in
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
plugins:build` installs it from the pinned sandbox-driver revision under
`target/plugins/bin`; the test tasks depend on it and set
`PETRI_SANDBOX_DOCKER_PLUGIN` to that path. To test against another build of the
plugin, set the variable yourself. A plugin this build does not pin runs only in
dev mode, which debug builds turn on; a release build needs
`PETRI_SANDBOX_PLUGIN_DEV=1` or `--sandbox-plugin-dev`.

To build plugins from the sibling sandbox-driver checkout during coordinated
development, use:

```sh
SANDBOX_DRIVER_SOURCE=../sandbox-driver mise run plugins:build
```

Both local and pinned Git builds install the provider packages directly.
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
