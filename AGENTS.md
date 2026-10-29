# Repository Instructions

## Project purpose

Petri is a Rust workflow engine. It runs a token-flow graph with explicit
routing and supports native workflows, GitHub Actions workflows, and
Attractor workflows, with Fabro's settings layer as a frontend over the
Attractor one.

Use these documents as the authoritative design sources:

- `engine-spec.md` defines the engine and intermediate representation.
- `frontend-handoff.md` defines the frontend contract.
- `crates/core/frontend-native/FORMAT.md` defines the native workflow format.
- `crates/attractor/FORMAT.md` defines the Attractor language as lowered.
- `crates/fabro/FORMAT.md` defines the Fabro layer around it: the settings files and the launch.
- `README.md` introduces Petri; `docs/architecture.md` maps the workspace and
  `docs/testing.md` maps the test suites.

## Rust style

Before changing Rust code, configuration, project structure, or tests:

1. Run `bin/style-guides prepare` when that command is available.
2. Read `.ai/style-guides/rust-style-guide/SKILL.md` completely.
3. Read each workflow and policy page that the skill routes for the task.

Project requirements and accepted architecture decisions override general
style-guide defaults.

## Repository tasks

- Use `mise run dev` to build and run the Petri CLI.
- Use `mise run test` for the routine test suite.
- Use `mise run check` for the complete routine verification gate.
- Use `mise run check:nightly` for the extended verification gate.
- Use `mise run fmt` to format Rust with the pinned nightly formatter.
- Name internal Git dependencies as "Git dependencies" in `DEVELOPING.md`
  describes.
- Use `scripts/corpus-fetch.sh` and `scripts/corpus-fetch-actions.sh` to prepare
  the GitHub Actions compatibility corpus.
- Use `scripts/corpus-fetch-fabro.sh` to prepare the Fabro compatibility corpus,
  and `scripts/oracle-regenerate.sh` to refresh the Fabro oracle fixtures when
  the Fabro pin changes.

Container scopes run through the `sandbox-driver-docker` plugin, which
`mise run plugins:build` installs from the sandbox-driver commit that
`Cargo.lock` locks. Docker tests skip when the plugin is missing or no
Docker daemon is available. CI requires Docker tests on Linux and requires the
compatibility corpus on all runners.

## Safety

- Never install packages less than 24 hours old.
- Never force push, including with `--force-with-lease`.
- Never amend commits. Create a new commit instead.

## Working documents

Save plans under `.ai/plans/` and reviews under `.ai/reviews/`.

Use plain language. Prefer short, direct sentences and established project
terms.

## README.md

Keep the README focused on what Petri does, who it serves, installation,
complete examples, scope, compatibility, and the Lithos family.

- Put API detail in rustdoc and longer guides in `docs/`.
- Keep repository file links absolute so they work outside GitHub.
- README Rust examples are doctests. Run `cargo test --locked -p petri --doc`
  after changing them.
- Keep the plugin install revision aligned with `Cargo.lock`.
