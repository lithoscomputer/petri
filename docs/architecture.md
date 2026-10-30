# Architecture

A Rust implementation of [`engine-spec.md`](https://github.com/lithoscomputer/petri/blob/main/engine-spec.md): a token-flow graph with
explicit routing, plus the pure state machine that executes it.

```
crates/core/ir         the vocabulary: graph, ids, expressions, values, validation
crates/core/store      the run store seam: logs and blobs by key; the run directory and in-memory backends
crates/core/engine     the sans-IO state machine: apply(state, event) -> (state, commands)
crates/core/executor   the environment interface; executor-sandbox implements it
crates/core/executor-sandbox  scopes and leases over Host, Docker, and Daytona plugins
crates/core/steps      step kinds and the one registry; frontends depend on names, not on this
crates/core/driver     the IO loop between the pure core and real processes
crates/core/execution  run, invocation, and execution coordination over the store seam
crates/core/frontend   what every format shares; frontend-native is core's own format
crates/core/runtime    core assembled: the `Runtime` builder components register onto
crates/core/cli        the command line, format-agnostic; the shipped binary hands it a runtime
crates/core/testkit    the scaffolding the end-to-end batteries share
crates/github/frontend   the GitHub Actions frontend: lowers workflow files to the IR
crates/github/actions    the GitHub Actions step kinds: run, action, checkout
crates/github/objects    the ObjectService: cache, artifacts, and tool-cache storage
crates/github/acceptance the acceptance battery: corpus harness and end-to-end runs
crates/attractor/frontend    the Attractor frontend: lowers Graphviz DOT workflows to the IR; reads no settings file
crates/attractor/steps       the Attractor step kinds: command, wait, human, agent over ACP or native Pebble, prompt, nested workflow, the stage step, the local hook service, MCP servers as Pebble tool sources (`pebble::mcp`), fidelity preambles, retained threads, project memory, Fabro's skill directories, context compaction, the output store, and the stub registry
crates/fabro/frontend    the Fabro frontend: Fabro's settings layers (`workflow.toml`, `.fabro/project.toml`, the user settings) and launch precedence around the Attractor frontend
crates/fabro/acceptance  the Fabro battery: corpus harness, runs under stubs, the Fabro oracle, real steps, the compatibility contract and bundle manifest
crates/petri/lib       the distribution: core plus every component
crates/petri/cli       the shipped `petri` binary: the distribution handed to core's CLI
```

Packages are named `petri-*` and arrows point down. Core never depends on a component; components
depend on core and never on each other, with one declared layer: `fabro` sits on `attractor`, since
the Fabro frontend wraps the Attractor frontend; only the distribution names them all.
`crates/petri/lib/tests/layering.rs` enforces this from `cargo metadata`. The next
format (CircleCI, RWX) is a `crates/<component>/` directory and one
registration in the distribution, as `crates/fabro/` is.

A node fires when its **join policy** is satisfied by incoming tokens. On completion
its **routing policy** emits tokens on outgoing edges. Routing is an AND of XORs:
each select group emits at most one token, and groups emit concurrently. The default
is selection, so fan-out is never implicit — it takes writing more than one group.

## Where the design lands in the code

| Design section | Code |
|---|---|
| §2 routing, AND-of-XOR | `ir::graph::{Routing, RoutingGroup, SelectionPolicy, Guard, Fallthrough}` |
| §3 core types | `ir::graph`, `ir::ids` |
| §3 expressions | `ir::expr` — `Expr`, `ExprTable`, `Context`, `eval` |
| §4 runtime types | `ir::flow` — `Token`, `Outcome`, `Status`, `Metrics` |
| §4 firing rule | `engine::apply::try_fire` |
| §4 quiescence | `EngineState::is_quiescent`, `apply::finish_if_quiescent` |
| §5 engine interface | `engine::event` — `Event`, `Command`; `engine::apply::apply` |
| §5 event log | `engine::log` — v9, with durable admission and routing decisions and a recording time beside every persisted record |
| §5 replay | `engine::replay` — `verify_replay` is the determinism canary |
| §4 retries | `ir::RetryPolicy`, `engine::apply::on_retry_elapsed` |
| §4 run context | `ir::RunContext`, `engine::state::EngineState::record_outcome` |
| §4 expression environment | `ir::EvalEnv` — one way to see upstream state |
| exec §1 layering | `executor::Executor` / `ExecEnv`, `steps::StepRunner`, `driver::Driver` |
| exec §2 driver loop | `driver::run` — one channel, so arrival order is the total order |
| exec §3 process step | `steps::process`, `steps::outputs` |
| exec §4 cancellation | `steps::process::ladder`, `driver::Driver::on_hard_deadline` |
| exec §5 environments | `executor::scope` (the interface), `executor_sandbox::{HostExecutor, SandboxExecutor, RoutingExecutor}` |
| exec §6 secrets | `executor::secrets`, `driver::LogSink` |
| execution hierarchy | `execution::{Coordinator, InvocationId, ExecutionId, InvocationClient}` |
| standalone persistence | `store::{RunStore, RunLogs}` behind `execution::{CoordinatorStore, ResourceStore, StoreWriter}`; `store::RunDirStore` is the run directory, one engine log per execution |
| §5a cancel scopes | `engine::state::CancelScope`, `apply::on_cancel`, `apply::on_kill` |
| §6 HIR → plan lowering | `engine::context::resolve_config`, `apply::expand` |
| §6 splice semantics | `engine::event::SubgraphSplice`, `apply::on_node_expanded` |
| §6a sequential `for_each` | `ir::desugar::sequential_for_each` — no new IR, just a cycle |
| GHA frontend mapping | `crates/core/engine/tests/gha.rs` |
| §7 validation invariants | `ir::validate` — `check` for errors and warnings, `validate` for errors alone |
| §8 reserved seams | `StepKind::fingerprint`, `Control`, `Command::{Acquire,Release}Scope`, `EventLog::version` |

## The Attractor language and the Fabro layer

`crates/attractor/FORMAT.md` is the language as lowered: every construct in a
DOT file, what it becomes in the IR, what is refused, and the step kinds a
graph runs on. `crates/fabro/FORMAT.md` is the Fabro layer around it: how
`workflow.toml`, `.fabro/project.toml` and the user settings resolve into the
run settings the lowering applies, and the launch record Fabro adds to the
graph. Fabro is one implementation of the Attractor specification, and the
next revision of that specification follows Fabro's dialect, so a Fabro
workflow is an Attractor workflow with Fabro's files beside it. The `attractor` prefix identifies the language's step kinds, diagnostics, and events.

## What the core may know

An audit after the first frontends asked whether GitHub-specific code had leaked into the
core, since every frontend added later — CircleCI, Buildkite, RWX, Attractor — would
otherwise leave its own residue there. The rule that came out of it:

> A primitive may enter `ir`/`engine` when it is generic — a thing several formats
> would plausibly want, described on its own terms. Nothing uniquely one format's may.
> Format semantics are *compositions* of primitives, and they live in that format's
> crate.

What the audit found and changed:

- **One real leak:** the `matrix_combinations` builtin carried GitHub's
  `include`/`exclude` merging rule. Replaced by generic combinators (above); the
  GitHub rule is spelled out in `frontend_gha::expr_lower::matrix_legs`, and GitHub's
  documented examples are tested there, through the engine's evaluator.
- **Wrong layer:** the GHA expression lowering (`gha`, `gha_function`,
  `GHA_FUNCTIONS`) lived in the *shared* `frontend` crate. Moved to
  `frontend-gha`. The shared crate now owns syntax and the strict lowering only.
- **Framing:** `loose.rs` and the `loose_*` summaries described themselves as
  "GitHub's rules". The semantics are the JavaScript-family ones; they are now
  described that way. `filter_field` became `pluck_present`; `gha_format` became
  `positional_format`.
- **Kept, by judgment:** `loose_string` renders a container as its type name
  (`Array`, `Object`), which is exactly GitHub's output. A loose string coercion needs
  *some* rule for containers, and type-name makes a value that should have been a
  scalar visible instead of a JSON blob silently reaching a command line. Documented
  on those terms.
- **Runtime layer, GitHub-influenced by design:** the process step's outputs file
  accepts the `key<<DELIM` heredoc form, a superset of `$GITHUB_OUTPUT`'s format so
  that variable can be a plain alias. A compatibility choice inside one step kind,
  documented as such; the engine does not know it exists.
- **Clean:** `Status`, `RetryPolicy`, `Expansion`, `RunContext`, `Graph.params`,
  `NoopStep`, `soft_fail`, the driver, the executor. Comments that cited GitHub as
  the *reason* for a rule were reworded to state the rule.

## What the executor interface may know

The executor boundary follows the same rule:

> The `executor` crate is the interface. It names no executor, and neither does
> anything above it: the IR describes the *kind* of environment a scope needs, step
> kinds reach the workspace only through the interface, and the driver translates.
> Each executor is its own crate, and adding one is adding a crate, not editing the
> interface.

What the audit found and changed:

- **`RuntimeTarget::Docker { image, args }` named an executor in the IR**, and
  `args` were Docker CLI flags. It is `RuntimeTarget::Container { image }` now: a
  *kind* of environment several executors can provide. Engine flags, mounts and
  limits are executor configuration, not something the graph carries.
- **The step kind assumed the workspace was on this machine.** The process step
  read and wrote its outputs file with `std::fs` on `ExecEnv::workspace()`, a
  host-visible path — true for a host workspace, false for anything else.
  `workspace()` is gone from the interface; `read_file` and `write_file` take its
  place. The host executor answers from the filesystem; the container executor
  answers through the sandbox's filesystem facet over the plugin wire, because a
  sandbox owns its workspace and nothing on this machine mirrors it. The step
  kind is none the wiser.
- **The shared error and report types had Docker fields.** `EnvError::Docker` is
  `EnvError::Backend { backend, operation, message }`; `ReleaseReport` describes
  what was released and kept as text rather than as `container_removed` /
  `workspace_removed` booleans that would grow a field per executor.
- **Already right when the audit ran:** the executors live outside the interface
  crate (`executor-sandbox` over the provider plugins), with a private teardown record the interface carries
  as an opaque `Teardown` trait object and hands back untouched; the output pump
  lives in the interface crate because every executor needs it and the line cap
  must be decided once.
- **Kept:** `Retention` (keep-on-failure) is generic — anything with a workspace has
  the question. `Sig` is POSIX; a remote executor maps it. Placement labels stay
  opaque in `RuntimeSpec.requirements`, and the set the GHA frontend accepts is
  GitHub's hosted-runner vocabulary, which is GitHub knowledge in the GitHub crate;
  which of those any executor honours is that executor's, and not yet plumbed.

See [design notes](https://github.com/lithoscomputer/petri/blob/main/docs/design-notes.md) for the implementation history and differences from earlier handoffs.
