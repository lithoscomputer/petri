# Petri

A Rust workflow engine for running CI and AI agent workflows, embedded in your application or from the command line.

[![CI](https://github.com/lithoscomputer/petri/actions/workflows/ci.yml/badge.svg)](https://github.com/lithoscomputer/petri/actions/workflows/ci.yml)

```rust,no_run
use petri::{RunOptions, frontend::native};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow = r#"
name: hello
nodes:
  greet:
    run: echo "Hello from Petri"
    next: done
  done:
    run: echo "Workflow complete"
"#;
    let lowered = native::load("hello.yml", workflow);
    let graph = lowered.graph.ok_or("invalid workflow")?;
    let runtime = petri::runtime().options(RunOptions::new("petri-run"));
    let report = runtime.run(graph).await?;
    println!("{}", report.status);
    Ok(())
}
```

Part of [Lithos](#part-of-lithos), a family of Rust building blocks for AI tools.
The example needs the host plugin from [Install](#install) and prints `success` when both commands finish.

## Why does this exist?

CI jobs and agent workflows need branching, parallel work, retries, and cancellation.
Petri runs these through one engine, with explicit routing and a replayable event log.
Applications can embed the runtime and supply their own storage, controls, and user interface.

## Who is it for?

**Use it if** you are building a workflow runner or an AI tool that combines commands, agents, and human decisions.
It also runs supported GitHub Actions workflows locally.

**Look elsewhere if** you need a hosted CI service with triggers, runner scheduling, and a web interface.
Use GitHub Actions for workflows that require GitHub's hosted environment or unsupported runner features.

## What does it do?

- Runs native YAML workflows with explicit branches, joins, loops, and parallel work.
- Runs supported GitHub Actions jobs, actions, matrices, and reusable workflows.
- Runs [Attractor](https://github.com/lithoscomputer/petri/blob/main/crates/attractor/FORMAT.md) agent workflows written as Graphviz DOT graphs.
- Reads [Fabro](https://github.com/lithoscomputer/petri/blob/main/crates/fabro/FORMAT.md) settings around those graphs, including models, environments, and hooks.
- Executes commands on the host, in Docker, or in [Daytona](https://www.daytona.io/) cloud sandboxes through provider plugins.
- Records runs for inspection, replay, and recovery after a process exits unexpectedly.
- Exposes events, human questions, secrets, and lifecycle hooks to an embedding application.

**Out of scope:** a hosted control plane, UI, trigger service, and cross-run scheduling.
The application supplies those, along with credentials and any custom storage or policy.
GitHub Actions compatibility is a supported subset; see the [support matrix](https://github.com/lithoscomputer/petri/blob/main/crates/github/SUPPORT.md).

| Crate | Use it when |
|---|---|
| `petri` | You want every shipped workflow format through one dependency. Core APIs are re-exported here. |
| `petri-runtime` | You want the native runtime and will register other formats yourself. |
| `petri-frontend-gha` | You are assembling a runtime with GitHub Actions support. |
| `petri-frontend-attractor` | You want to compile the Attractor language without Fabro settings. |
| `petri-frontend-fabro` | You want Attractor workflows with Fabro's settings and launch rules. |

The [architecture guide](https://github.com/lithoscomputer/petri/blob/main/docs/architecture.md) maps the full workspace.

## How do I use it?

### Install

Petri is not published to crates.io. Add it from Git:

```sh
cargo add petri --git https://github.com/lithoscomputer/petri --branch main
cargo add tokio --features macros,rt-multi-thread
```

Commands run through sandbox provider plugins. Install the host plugin at the revision in Petri's current lockfile:

```sh
cargo install --locked --git https://github.com/lithoscomputer/sandbox-driver --rev 691cb4e5b8425434c1fe24becc47962a4fae9ce4 sandbox-driver-host
```

Keep Cargo's binary directory on `PATH`. The opening example runs with `cargo run` in a debug build.
Source-built release binaries need explicit plugin trust; see the [CLI guide](https://github.com/lithoscomputer/petri/blob/main/docs/cli.md#source-installation).
Docker workflows also need Docker and its provider plugin. Agent workflows need an agent backend and its credentials.
Native model access reads variables such as `OPENAI_API_KEY` and `ANTHROPIC_API_KEY`.
See [agent configuration](https://github.com/lithoscomputer/petri/blob/main/crates/attractor/FORMAT.md#native-pebble) for setup.

### Use it as a library

Validate workflow text without starting a command or acquiring a sandbox:

```rust
use petri::{Runtime, frontend::{CompileInputs, NoFiles}};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow = "name: hello\nnodes:\n  greet:\n    run: echo hello\n";
    let checked = Runtime::standard().check_source(
        "hello.yml", workflow, &NoFiles, Some("native"), &CompileInputs::new(),
    )?;
    for diagnostic in checked.diagnostics.iter() {
        eprintln!("{diagnostic}");
    }
    checked.graph.ok_or("invalid workflow")?;
    Ok(())
}
```

`Runtime::standard()` includes the native format. `petri::runtime()` also registers GitHub Actions and Fabro support.
For durable coordination, nested workflows, and resume, use the `petri::host` helpers described in the [embedding guide](https://github.com/lithoscomputer/petri/blob/main/docs/embedding.md).

- [Native workflow format](https://github.com/lithoscomputer/petri/blob/main/crates/core/frontend-native/FORMAT.md): routing, joins, retries, and loops.
- [Embedding guide](https://github.com/lithoscomputer/petri/blob/main/docs/embedding.md): events, hooks, questions, and storage.
- [Fabro integration contract](https://github.com/lithoscomputer/petri/blob/main/crates/fabro/HANDOFF.md): host responsibilities and compatibility.

### Use it from the command line

Install the CLI alongside the host plugin above:

```sh
cargo install --locked --git https://github.com/lithoscomputer/petri --branch main petri-cli
```

Save this as `hello.yml`:

```yaml
name: hello
nodes:
  greet:
    run: echo "Hello from Petri"
```

```sh
petri check hello.yml
petri run hello.yml --run-dir petri-run --sandbox-plugin-dev
petri inspect --run-dir petri-run
```

`--sandbox-plugin-dev` lets this source-built binary launch your installed plugin without a bundled digest.
Use a fresh run directory for each run. See the [CLI guide](https://github.com/lithoscomputer/petri/blob/main/docs/cli.md) for backends, human questions, controls, resume, and retention.

## Status and compatibility

- **Version:** 0.1.0 in the workspace; packages are not published to crates.io.
- **Rust:** declared minimum 1.89; routine CI uses 1.97.1.
- **Platforms:** routine CI runs on Ubuntu 24.04 and macOS 15. Docker tests are required on Linux.
- **Workflow compatibility:** [GitHub Actions support matrix](https://github.com/lithoscomputer/petri/blob/main/crates/github/SUPPORT.md) and [Fabro compatibility contract](https://github.com/lithoscomputer/petri/blob/main/crates/fabro/acceptance/CONTRACT.md).
- **Persisted contracts:** versioned run logs, [public events](https://github.com/lithoscomputer/petri/blob/main/crates/core/execution/EVENTS.md), and [inspection output](https://github.com/lithoscomputer/petri/blob/main/crates/core/execution/INSPECT.md). Incompatible log versions are rejected.

## Part of Lithos

| Crate | Role |
|---|---|
| [lithos-llm](https://github.com/lithoscomputer/lithos-llm) | Model access: one client and catalog across LLM providers |
| [pebble](https://github.com/lithoscomputer/pebble) | Agent loop, plus a coding agent and CLI built on it |
| [sandbox-driver](https://github.com/lithoscomputer/sandbox-driver) | Sandboxes (host, Docker, cloud VMs) behind one interface |
| **[petri](https://github.com/lithoscomputer/petri)** | **Workflow engine that runs CI and agent workflows on the crates above** |

## Contributing

See [DEVELOPING.md](https://github.com/lithoscomputer/petri/blob/main/DEVELOPING.md) for setup and checks,
and the [test guide](https://github.com/lithoscomputer/petri/blob/main/docs/testing.md) for coverage and compatibility evidence.
The [engine specification](https://github.com/lithoscomputer/petri/blob/main/engine-spec.md) and
[frontend contract](https://github.com/lithoscomputer/petri/blob/main/frontend-handoff.md) define the design.

## License

See [LICENSE](https://github.com/lithoscomputer/petri/blob/main/LICENSE).
