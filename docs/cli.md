# Command-line guide

See the [README](https://github.com/lithoscomputer/petri/blob/main/README.md#install) for installation.
Use `petri --help` and `petri run --help` for the complete option list.

## Source installation

The README installs the CLI from Git. The Host, Docker, and Daytona providers
are sandbox-driver built-ins linked into the binary at the revision Petri's
lockfile pins; no provider executable is installed or launched. Docker needs a
running daemon. Daytona needs account credentials.

## Execution backends

Petri places every scope on a sandbox catalog through sandbox-driver's shared
sandbox API (`Sandboxes`): provider entries, and named environments on them.
Host, Docker, and Daytona are built-ins registered in-process with
`Registry::builtin` (`crates/core/executor-sandbox/src/catalog.rs`); no
provider executable is launched. With no catalog file, `--backend` describes
one entry and one environment per built-in: `host` (a directory of the run),
`docker` (the pinned slim runner image, `DOCKER_HOST` or the default socket),
and `daytona` (the pinned Docker-in-Docker runner, `DAYTONA_*`). Process scopes
go to the backend's environment and container scopes to `docker`, or to
`daytona` on the Daytona backend. `--sandbox-catalog PATH` reads
`[providers.*]` and `[environments.*]` from a TOML file instead; an embedder
passes its own entries, environment store and credential store to
`Runtime::sandbox_catalog` (Fabro does). Docker tests skip when no daemon is
reachable. A provider that lost command output on its own transport
(Daytona's encoded exec can tear a frame) counts the loss in its result;
Petri keeps the command's exit status and appends one line, `[sandbox] N
output frame(s), M bytes dropped by the provider`, to the command's stderr, so
the agent and the run log see it.

Every lease records its sandbox's reference (provider entry, namespace and
id) and the environment and resolved spec it was admitted with. A resume or
prune reaches the sandbox through that reference, and refuses an entry that
now reaches another namespace (a changed `DOCKER_HOST` or Daytona
organization). Leases recorded before this release name no reference; Petri
finds their sandboxes by the run's `petri.run` label and their recorded id.
The built-in Host provider owns each lease's process groups for the life of
the process that created it; a resumed run creates a new Host sandbox over the
same workspace directory, which Petri keeps under the run directory.
Retention keeps or deletes that workspace with its lease; `petri sandbox
prune` deletes retained workspaces.

Host jobs inherit `PATH`, `HOME`, `USER`, `SHELL`, `LANG`, `TERM`,
`TMPDIR`, `GOPATH`, `CARGO_HOME`, and `NVM_DIR`. Pass other values and secrets
explicitly through workflow environment settings.

`petri run --backend host|docker|daytona` selects the execution backend.
Host is the default, unless the workflow's own configuration asks for another
(a Fabro `workflow.toml` `[environments.<id>] provider`, read back from the
lowered graph through `Frontend::launch_settings`; `petri resume` finds the
format from the stored graph through `Frontend::claims_graph`); an explicit
`--backend` always wins. Docker runs process jobs in a pinned slim runner image.
Daytona runs them in a container with nested Docker; container jobs, services,
and Docker actions stay inside that sandbox. `--daytona-kind vm` is
refused until sandbox-driver's shared API carries Daytona's VM class. `--runner-image LABEL=IMAGE` overrides a
runner label. Docker defaults cover Ubuntu 22.04, 24.04, and 26.04. Daytona's
published Docker-in-Docker default covers Ubuntu 24.04 and `ubuntu-latest`;
other labels need an override. Unknown or conflicting labels fail at acquire.

Daytona reads its credentials from `DAYTONA_API_KEY` or `DAYTONA_JWT_TOKEN`
and the SDK's endpoint and organization variables. The CLI and embedded library
share a baseline of 2 CPUs and 4096 MiB of memory, the runner's minimums.
Disk is unspecified by default, so Daytona chooses the allocation. Existing
`--daytona-cpus`, `--daytona-memory-mb`, and `--daytona-disk-mb` flags override
each value independently; clients set `SandboxOptions` and `DaytonaResources`.
Explicit disk allocations must be positive; Daytona enforces image-fit and
account limits. No VM access or enlarged disk quota is needed for the defaults.
The executor uses the driver's whole-GiB allocation rule before checking the
memory minimum, naming a snapshot, and comparing its reported resources.
Equivalent requests therefore reuse a snapshot, including requests supplied
in MiB by an embedded client. Omitted disk remains unspecified.
Runner snapshots are named by image and effective resources, prepared once per run,
and reused across runs. Snapshot preparation has a 15-minute deadline.
Petri disables automatic sandbox stop, pause, deletion, and TTL timers; lease
release and prune control cleanup. Shared runner snapshots remain available.

Daytona has no inferred route to Petri's ObjectService. Ordinary JavaScript
and Docker actions still run, with the service variables omitted. Artifact
and cache actions need `PETRI_SANDBOX_DAYTONA_HOST_ADDRESS` set to a name or
address reachable from the sandbox. Local transport and adapter tests pass. The
live Daytona tier (`mise run test:daytona`, see
[the live Daytona tier](https://github.com/lithoscomputer/petri/blob/main/crates/core/executor-sandbox/DAYTONA.md)) needs
credentials. Its default process and nested-container smoke tests have passed
against the hosted service; the full tier remains separate validation.

sandbox-driver, Pebble, and lithos-llm are public repositories fetched over
HTTPS, so no job needs a deploy key. See
[git dependencies](https://github.com/lithoscomputer/petri/blob/main/DEVELOPING.md#git-dependencies).

Fabro agent nodes can use Pebble directly as a Rust library. Set
`backend="api"` and `model="provider/model"` on the node, or set graph
`backend` and `default_model`. The `petri` distribution reads provider
credentials from its environment. ACP remains the default. See
[native Pebble configuration](https://github.com/lithoscomputer/petri/blob/main/crates/attractor/FORMAT.md#native-pebble) for scope
requirements, client injection, project memory, skills, context compaction,
tool hooks, retained threads, sub-agents, events, and accounting. Fabro's `fidelity` modes, threads and
`[[run.hooks]]` are described under "Steps at run time" on the same page.

`petri inspect --run-dir <dir> [--json]` reconstructs a run from its run
directory alone: `run.json`, `coordinator.jsonl`, the registered graphs, and
each execution's `events.jsonl`, replayed through the engine. It never starts a
step, contacts a provider, takes the run lease, or writes under the run
directory, so it works after the process exits, after the source workflow is
gone, and while another process holds the run. `--json` prints a versioned
document (`inspect_format_version`) with the run status, the root invocation
and its final execution, every invocation with its parent call and children,
every execution in order with its own derived run context (`kv` plus the
node-instance records: status, output, generation, attempts), the final firing
history, every attempt including retries, every applied route, and the
interview receipt (`interviews`, from `<run-dir>/interviews.json`) when the run
had an interviewer. Secret values stay masked or as `$secret` references. An interrupted run is reported
as incomplete with the reasons (exit 1); a corrupt, truncated-then-terminated,
diverging, or unsupported-version log is an error (exit 2), never a final
snapshot. The field contract is
[`INSPECT.md`](https://github.com/lithoscomputer/petri/blob/main/crates/core/execution/INSPECT.md).

## The terminal path

`petri run <workflow>` is the standalone runner: one invocation loads,
validates and executes one root workflow with no server, database or UI. It
prints the run dir first, then every step's output on stderr as
`[<node>#<firing>] <line>`, or `[invocation-<N>/<node>#<firing>] <line>` for a
parallel branch (a child invocation), so interleaved output stays attributable
(each line is masked and cut at 4096 characters; a stage's echo stops at 64 KiB
with one marker naming its log file, which keeps every line; a scheduled retry
is announced as `retry: attempt N of M in Ss`; `--quiet` turns the echo off),
then one `<status> <node>` line per finished node, the retained workspace
paths, and `run: <status>`. The exit code is 0 for success, 1 for a failed or
cancelled run, 2 for a usage error, 3 for a host error, and 4 when the run
finished but its interview did not go as scripted.

**Answering questions.** A `attractor/human` gate, and a native agent's question
tool, ask through the core `Question` event; the host's `Interviewer` answers
(`execution::Interviewer`, an open trait a product host implements too). The
CLI ships three, one per option, and they exclude one another:

- `--interactive` prints each question on stderr and reads one line from
  stdin. Questions from parallel stages are serialized; a `yes_no` or
  `confirmation` gate takes Enter as its default; a `multi_select` gate takes
  comma-separated keys; a `freeform` gate takes text. EOF or three invalid
  answers fail the gate closed.
- `--auto-approve` takes the default choice, or empty text.
- `--interview-script <file>` answers from a versioned JSON script and fails
  the run when a question matches no entry, matches several, exhausts an
  entry's `count`, or a required entry goes unused. It never falls back to the
  terminal or to auto-approval.

An interview script:

```json
{
  "version": 1,
  "entries": [
    {
      "id": "ship-it",
      "match": { "node": "gate", "kind": "yes_no", "options": ["Y", "N"] },
      "count": 1,
      "action": { "kind": "choice", "value": "Y" }
    }
  ]
}
```

`match` fields (all optional, all must hold): `node`, `invocation_path` (`/`
for the root, `/<slot>` per nested call), `occurrence` (which distinct question
of that node, 1-based), `ask` (which time the same question was asked, after a
rejected answer), `kind`, `text`, `text_contains`, `options` (the offered keys
in order), `default`, `freeform`, `sensitive`, `reference_url_contains` (a
`review_target` gate's URL). Actions: `choice` (`value`), `choices`
(`values`, for `multi_select`; Fabro's `multi_selected` `option_keys`), `text`
(`value`), `negative` (the `N`/`no` option), `invalid` (`value`, sent as a
choice the step must reject; the re-ask matches `"ask": 2`), `cancel`,
`withhold` (no reply until the question expires or is cancelled, so a gate
with a `timeout` expires into its `human.default_choice` or Fabro's retry
outcome and the receipt records the question as `timed_out`).
`delay_ms` waits before acting; `required: false` lets an entry go unused.

The terminal shows a `review_target` gate's reference as `review: <label>
<url>` and a gate's answer deadline as `(answer within 90s)`. Steering is not
answering: a control line (below) never consumes a pending question's answer.

**Controls.** `--control <file>` tails a file for run controls while the run is
live, one per appended line: `pause` holds every attempt not yet admitted
(running work continues, and Ctrl-C still cancels), `unpause` releases them,
`steer <node> <text>` delivers guidance to the named stage's live firing (an
agent queues it for its session; a human gate ignores it), `interrupt <node>
[text]` stops the named agent stage's current model turn and keeps its
session (the text, else the next `steer`, is the stage's next input; a stage
with no turn in flight refuses it), and `cancel` cancels the run (a second
`cancel` reaches the kill tier). Each line's effect is reported as
`control: ...` on stderr; a line that is not a command, names a stage that is
not running, or interrupts a stage with no model turn is reported and
skipped. An embedded host drives the same
`execution::controls::ControlService`, installing its live-turn set as a
capability so `interrupt` can find a turn.

The pause is durable. Each `pause` and `unpause` is a coordinator record
(`RunPaused`, `RunUnpaused`), so `petri inspect` reports `paused` and a resume
of a run whose last control was a pause starts with admission held.

**Resuming.** `petri resume --run-dir <dir>` continues a run whose process
died. It needs only the run directory: not the workflow file, not its inputs.
The workflow's format is found from the stored root graph
(`Frontend::claims_graph`), and its launch settings and default retention
apply as they did to `petri run`. The session options are given again and
apply to the resumed session: `--interactive`, `--auto-approve`,
`--interview-script`, `--control`, `--retain`, `--quiet`, `--dry-run`, and the
provider and runner options. Finished work is not repeated; the node that was
in flight at the crash starts again. A gate that was waiting asks again, and
the resumed interviewer answers it; the interview receipt written beside the
run is the resumed session's. A paused run stays paused until an `unpause`
arrives through `--control`, which a resume tails from its current end, so
lines the earlier process applied are not applied again. Before anything
starts, the command reads the coordinator log without the lease and refuses
with exit code 2 and no work done: a run that already finished (`already
finished (<status>); nothing to resume`), a run another process holds (the
lease on `run.json`), a paused run given no `--control` file, and a run
directory that is missing or does not decode. What a resume does not restore:
an agent node's retained thread. As in Fabro, whose `AgentApiBackend` keeps
full-fidelity sessions in memory per worker, a node that was mid-turn starts a
fresh session from the `summary:high` preamble.

**Policies.** A Fabro graph's `stall_timeout` (default 30 minutes) cancels a
run that emits no execution event for that long; a pending question parks the
clock, and the terminal prints `stall watchdog: no execution activity for N
s`. Its `loop_restart_signature_limit` (default 3) fails a run whose node
repeats one deterministic failure that many times and blocks a `loop_restart`
edge taken by anything but a transient failure. See `crates/attractor/FORMAT.md`,
"Watchdog and circuit breaker".

**The receipt.** Every run with an interviewer writes
`<run-dir>/interviews.json` (`execution::InterviewReceipt`, version 2) each
time a question's outcome is recorded, so a crash loses only the questions
still waiting, which the resumed run asks again. A resumed run continues the
receipt it finds: the receipt's `lifetime` counts the processes that wrote
it, and each record carries the `lifetime` that asked it. One
record per question with its invocation, execution, firing, attempt, node,
occurrence, ask, question id, kind, text, offered option keys, the review
`reference` and `timeout_ms` when the question had them, the reply
(`answered` with `choice`/`choices`/`text`, `cancelled`, `failed`, or
`timed_out` with the `default` the gate took on its own when it had one: the
gate's own report of its expiry, never inferred from how the firing ended),
and how it left (`delivered`, `not_live`, `late`, `shutdown`, `withheld`,
`expired`); the
`errors` list; and under `script`, a scripted interviewer's per-entry
`consumed`/`remaining` counts. The records are in the receipt order: by
invocation path, then invocation, execution, firing, occurrence, and ask
(the root's questions first, then each nested invocation's in path order;
within an invocation, the order the run asked them), whatever order the
answers arrived in, so a re-asked question follows its original ask, and a
question asked again after a resume follows the earlier process's. A
sensitive answer appears only as its
`{"$secret": "answer:<id>"}` reference. A non-empty `errors` list is exit
code 4, whatever the engine status; the persisted run is not rewritten.

**Model selection.** `petri run --model <name>` and `--provider <id>` choose
the model for every prompt or agent node that names none of its own, over the
graph's `default_model`/`default_provider` and `[run.model]` in `workflow.toml`
(and the project and settings layers), as `fabro run --model` and `--provider`
do. Each applies on its own. A node that names its own model, by attribute or
stylesheet rule, keeps it, and keeps its provider to the node and the defaults.
`--provider` alone, where nothing names a model, runs the provider's default
model from the runner's catalog (`openai` is `gpt-5.6-sol`); the pinned
interview bundle relies on this. A host can also bind its own last-resort
default (`petri.default_model`, `petri.default_provider`), below every file
layer. The launch lands in the persisted root graph's `fabro.launch` parameter
(`model`, `provider`, `default_model`, `default_provider`, as given), so
`petri inspect` and a replay see it; `petri replay` takes the same options so
the graph lowers the same. `petri resume` needs nothing: the stored graph
already carries the choice.

**Environment selection.** `petri run --environment <id>` selects the
`[environments.<id>]` table the run executes in over what any settings layer's
`[run.environment]` names, as `fabro run --environment` does; the table may
come from `workflow.toml`, `.fabro/project.toml` or the user settings layer
(`$FABRO_HOME/settings.toml`), which is how a bundle whose project file selects
a Daytona environment still runs on the host. The resolved environment lands
in the persisted root graph's `fabro.environment` parameter.

**Retention.** `--retain always|on-failure|never` decides what happens to the
run's workspaces at teardown. The default is the workflow format's: Fabro
keeps every workspace (its result is the files), other formats keep a failed
run's and delete a successful run's. The run reports each retained workspace:
a host workspace as its path under the run dir, a container's as its sandbox
and the `petri sandbox prune --run-dir <dir>` command that deletes it.

**Provider configuration.** Native agents (`backend="api"`) use the
distribution's `lithos-llm` client (`petri::llm_client`). Credentials are the
provider library's conventional variables (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, and so on), read per request. `PETRI_LLM_CATALOG` names
one or more catalog TOML files (path-separator delimited) layered over the
built-in catalog, so a test or a gateway deployment redirects a provider:

```toml
schema_version = 1
[providers.openai]
base_url = "http://127.0.0.1:3000"
```

`PETRI_LLM_PROVIDERS` (comma-separated provider ids) narrows which providers
may be routed to at all; a model on any other provider is unavailable. An
unreadable or invalid layer leaves the client unbuilt and every native agent
node failing with `pebble_unconfigured`, rather than reaching a live
endpoint. Fabro prompt nodes (`tab`) use the same client for their one
tool-free call.

Three retry mechanisms exist and each has one owner. `PETRI_LLM_RETRY_ATTEMPTS`
(default 3; `1` disables) is the client's own budget for sending one request
again on the same route after a retryable failure, with exponential backoff and
the provider's `Retry-After` honored; `PETRI_LLM_TIMEOUT_MS` bounds one call,
retries included. Pebble replays a model turn whose response stream broke, on
its default policy. Both report on the agent's event stream as Pebble's
`LlmRetry`. Fabro's `[run.model.fallbacks]` chain is planned by Petri and run
by Pebble, and starts only once both are spent; see "Model fallback" in
`crates/attractor/FORMAT.md`. A workflow retry (`retries` on a node) is a new
attempt with a new plan and is none of these.

**Workflow secrets.** A Fabro `workflow.toml` environment value written as
`{{ secrets.NAME }}` is a secret reference. The standalone runner resolves it
from the environment variable `PETRI_SECRET_NAME` when the command that
carries it starts, masks the value in every log, and fails the command with
`secret_unavailable` when the variable is unset. An embedding host supplies
its own `SecretProvider` instead.

## GitHub Actions at run time

What a lowered workflow finds at run time, standing in for the world GitHub
provides around a job:

- **The ObjectService** (`crates/github/objects`): one process per run speaking
  the results backend the 2026 toolkit converged on — twirp `ArtifactService`
  and `CacheService` under one listener, one minted token, one signed-URL blob
  implementation. Artifacts are run-scoped under the run dir; cache entries
  live in the host's persistent store (`$PETRI_STORE`, default
  `~/.cache/petri/store`, 10 GiB LRU) and outlive the run. The token is also
  the auth: the exact bearer string, salted per run, so binding beyond
  loopback for container reachability (`host.docker.internal`, guaranteed via
  `--add-host`) never leaves an open write endpoint.
- **Local checkout**: a supportable `actions/checkout` call substitutes at
  lowering for the `github/checkout` step, which materializes the workspace
  from the run's own repository — committed HEAD plus the uncommitted tracked
  diff and untracked-but-not-ignored files, `.git` and mode bits included —
  offline and token-less. Run identity is honest too: `default_params` fills
  `github.sha`/`ref` from the checkout's HEAD (run parameters, so lowering and
  replay are untouched).
- **The tool cache**: host jobs point `RUNNER_TOOL_CACHE` at
  `<store>/toolcache/<os>`; a runner image that ships a populated
  `/opt/hostedtoolcache` keeps its own.

**The GitHub API stance (v1, decided 2026-08-28): petri neither proxies nor
blocks it.** `github-script` and `gh` steps hit the real API with the ambient
token when one exists (`$GITHUB_TOKEN`, else a logged-in `gh`); without one,
public reads work and authenticated calls fail routably. **A local run with a
real token can mutate — comment, tag, release — exactly as the workflow says.**
Prefer a fine-grained read-only token when running workflows you did not
write. The corpus sweep runs token-less by policy, so corpus code can never
act with your identity. A default-deny proxy stays future work if real demand
appears.
