# Black box scenario format, version 1

A scenario is one JSON file, `<family>/<name>.scenario.json`, under this
directory. It names the graph it runs, the inputs and fixture state the run
starts from, the execution modes it runs in, the scripts every external
service answers with, the interview answers, the controls and faults the
harness applies, the time bounds, and the observations the run must produce.
The expected observations are written before the run; the harness compares,
it never records.

The loader is `crates/petri/cli/tests/support/fabro/scenario.rs`. Unknown
keys are errors everywhere, so a typo in a field name fails the load instead
of silently disabling an assertion. Task 18's differential adapter and task
19's coverage report read the same files.

## Top level

| Key | Required | Meaning |
| --- | --- | --- |
| `schema_version` | yes | `1` |
| `id` | yes | `<family>/<name>`, equal to the file's path without `.scenario.json` |
| `family` | yes | one of `implement`, `interview`, `provider-faults`, `routing`, `backend` (the backend-matrix cases: what a container scope must do that a host scope already does), `acp` (the ACP agent backend through a scripted agent the fixture commits) |
| `title` | yes | one sentence |
| `obligation` | no | the obligation in `../CONTRACT.md` this scenario satisfies |
| `bundle` | yes | see "Bundle" |
| `inputs` | no | `--input` values, `{"key": value}`; a string value may use placeholders |
| `fixture` | no | see "Fixture" |
| `modes` | yes | see "Modes" |
| `services` | no | see "Services" |
| `interviews` | no | the `--interview-script` document's `entries` list, in the format of `cli::answer` |
| `controls` | no | see "Controls and faults" |
| `bounds` | no | `{"deadline_ms": n}`; the harness kills the run after `n` ms (default 120000) and fails the case |
| `expect` | yes | see "Expected observations" |

## Bundle

```json
"bundle": { "inline": "graphs/workflow.fabro" }
```

`inline` names a graph file in the scenario's directory. Every graph file
(`.fabro` or `.dot`) and `workflow.toml` beside it is copied flat to the root
of the fixture repository and committed in the first commit, so a parent's
`stack.child_workflow` can name its child by file name.

## Fixture

```json
"fixture": {
  "commits": [
    { "message": "base", "write": { "src/app.py": { "text": "…" } }, "remove": [] },
    { "message": "change", "write": { "src/app.py": { "file": "fixtures/app-after.py" } } }
  ],
  "remotes": { "origin": { "bare": true, "branches": ["main"] } },
  "bin": { "gh": { "file": "fixtures/bin/gh", "mode": "0755" } },
  "env": { "PETRI_SECRET_RXDB_PREMIUM": "fixture" },
  "seed": true
}
```

- `commits` build the fixture repository in order; the first commit also
  holds the bundle files. `write` maps a repository path to `{"text": …}` or
  `{"file": …}` (relative to the scenario directory) with an optional `mode`.
  Placeholders `{commit:N}` (the SHA of commit N, zero based) and
  `{fixture}` (the repository's host path) are available afterwards.
- `remotes` are real local Git remotes: a bare clone of the repository at the
  last commit, added under that name. `branches` lists the branches the
  remote carries (default `main`). `{remote:origin}` is its host path.
- `bin` are executables written under `fixtures/bin` of the staged scenario,
  outside the repository, and put first on the launcher's `PATH`. Host steps
  inherit that `PATH`; a Docker scope does not, so a scenario with `bin` is
  host-only unless the matrix marks it otherwise.
- `env` are extra variables for the `petri` process, such as
  `PETRI_SECRET_<NAME>` stand-ins.
- `settings` is the text of the host's user settings layer, written to
  `$FABRO_HOME/settings.toml` of the isolated home. Fabro merges that layer
  under `.fabro/project.toml` and `workflow.toml`; a scenario uses it for
  what a Fabro operator sets outside the repository, such as the
  `[run.model]` default a bundle that declares no model needs.
- `python_modules` names Python modules the bundle's helpers import (the
  bundle's own image installs them). On the host the harness puts the first
  `python3` on its PATH that imports them all first on the run's PATH; when
  none does, the cell is skipped with that reason, which the coverage report
  shows as a missing required cell until the modules are installed. A Docker
  scope uses the pinned runner image's `python3`, which carries the modules
  the pinned bundles need (PyYAML since sandbox-images `df708f91`).
- `seed` (default `true`) is whether the run checks the fixture repository
  out into its workspace, as Fabro's `[run.clone]` does. The frontend's
  `[run.clone]` handling decides the depth.

## Modes

```json
"modes": { "backend": "host", "agent": "api:openai" }
```

`backend` is `host` or `docker`. `agent` is `api:openai`, `api:anthropic`,
`api:openrouter`, `acp`, or `none` (a run with no model call). The matrix
file names every required cell of `(scenario, backend, agent)`; a scenario
file declares the mode it is written for, and a second cell reuses the file
with `modes` overridden by the matrix entry. `api:openrouter` is answered by
the OpenAI twin on its chat completions endpoint, as the pinned catalog
routes it. `acp` starts no twin and binds no `{model}`: the fixture commits
a scripted Agent Client Protocol agent (`acp/fixtures/scripted_acp_agent.py`,
driven by a JSON script committed beside it, its header says how) and the
graph names it through `acp.command`, which resolves in the run workspace
on the host and in the container alike.

## Services

```json
"services": {
  "openai": [ { "scenario_id": "finder", "matcher": { "input_contains": "finder:low-pass" }, "script": { "kind": "success", "response_text": "…" } } ],
  "anthropic": [],
  "http": [ { "name": "github", "routes": [ { "method": "GET", "path": "/repos/o/r/pulls/1", "status": 200, "json": {} } ] } ]
}
```

`openai`, `anthropic`, and `openrouter` are twin scenario lists in the twins'
own format (`scenario_id`, `matcher`, `script`, `repeat`, `sticky`). The
harness fills `namespace` with the case credential and `matcher.endpoint`
and `matcher.model` with the mode's provider defaults when absent, so a
scenario names only what distinguishes a request. The placeholder `{model}`
is the mode's model id.

`http` declares scripted HTTP fixture services the harness serves on
loopback; the run reaches one through the environment variable named by
`env_var`, and `expect.side_effects.http` asserts its request log. No bundle
in this version needs one (fix-ci's branch-target path makes no HTTP call),
so the loader accepts the field and the harness refuses to run a scenario
that uses it until a service is implemented.

## Controls and faults

```json
"controls": {
  "interrupt_when": { "file": "waiting.txt" },
  "control_lines": [ { "after_file": "gate-open.txt", "line": "pause", "delay_ms": 0 } ],
  "stdin": { "text": "Y\n", "close": true },
  "path_without_fabro": true
}
```

- `interrupt_when.file` sends SIGINT once the named workspace file exists:
  on the host the harness reads the workspace, in a container it reads
  `/workspace/<file>` through `docker cp`, so one scenario serves both
  backends (`container_file` names a container path outright). This is how
  a case cancels a run the way a person does, tied to an observed event.
- `interrupt_when.request` sends SIGINT once the named twin scenario has been
  consumed: a cancellation timed to a provider request the run made.
- `interrupt_when.stderr` sends SIGINT once a stderr line of the run
  contains the text: the cancel a person sends when they see a gate
  waiting (`waiting for an answer: <question>`).
- `control_lines` append lines to a `--control` file, each after an observed
  workspace file, for pause, unpause, steer and cancel.
- `stdin` runs `--interactive` with the given text; `close` sends EOF after.
- `path_without_fabro` runs with a `PATH` that resolves no `fabro`.

Provider faults are twin scripts (`error`, `hang`, `close_after_chunks`,
`malformed_sse`, `repeat`), declared under `services`; nothing in the run is
patched.

## Expected observations

Every scenario declares every row of the phase 3 table. A row a scenario
has nothing to say about is declared empty on purpose (`[]`, `{}`), never
omitted: a missing row is a load error.

```json
"expect": {
  "process":   { "exit_code": 0, "status": "success", "required_nodes": ["prepare"], "forbidden_nodes": ["publish_pr"], "visits": { "verify": 2 } },
  "context":   { "exact": { "empty_target": true }, "absent": ["finder_jobs"], "extra_allowed": ["last_stage"], "complete": false },
  "files":     [ { "path": "CODE-REVIEW-*/evidence/findings.json", "json": [] } ],
  "side_effects": { "git": [ { "repo": "origin", "ref": "refs/heads/main", "is": "{commit:1}" } ], "http": [] },
  "providers": { "openai": { "consumed": ["finder"], "unmatched": 0, "requests": 1, "model": "{model}", "effort": "low" } },
  "interviews": { "questions": [], "errors": [], "consumed": {}, "no_plaintext": [] },
  "lifecycle": { "no_leaked_processes": true, "retained_workspace": true }
}
```

Values compare exactly unless the value is a matcher object:

| Matcher | Meaning |
| --- | --- |
| `{"$any": true}` | present, any value |
| `{"$regex": "…"}` | a string matching the pattern (anchored by the pattern itself) |
| `{"$contains": "…"}` | a string containing the text, or an array containing the value |
| `{"$len": n}` | an array or string of length `n` |
| `{"$set": [...]}` | an array with exactly these elements in any order |
| `{"$subset": {...}}` | an object whose listed keys match; other keys are not checked |
| `{"$type": "number"}` | `null`, `bool`, `number`, `string`, `array`, `object` |

`process.status` is `success`, `failed`, or `cancelled`, the run's own
status as `petri inspect` reports it. `visits` are per node, from the node
records. `required_nodes` and `forbidden_nodes` are node names as the run
finished them (branch instances are `job#0`, ...); a node a cancel reached
before it ran has a `cancelled` record and counts for neither list. Two
optional keys of `process` default to empty: `attempts` is per node, how
many attempts the node's final firing took (`engine.context.nodes.<node>.attempts`
of the root's final execution), which is how a retry inside one firing is
observed while `visits` still counts one; `stderr_contains` lists texts
the run's terminal output must contain, the echoed step lines
(`[node#firing] …`) and the run's own notices, which is where a hook
warning or a block reason is public.

`context` compares the root invocation's final context: `exact` per key,
`absent` per key, and with `complete: true` every other key must be listed
in `extra_allowed`. Missing and `null` differ, strings and numbers differ,
array order matters. Branch results are asserted as the value of the fan-in
node's key or through `files` when the workflow writes them out.

`files` are read from the retained workspace. `path` may be a glob that must
match exactly one file. One of `text`, `file`, `contains` (a list), `json`,
`absent`, `lines` applies, plus `mode`.

`side_effects.git` reads refs of the workspace repository (`repo: "workspace"`)
or a fixture remote: `is` the exact SHA, `advanced_from` a SHA the ref must
now be ahead of by `commits` (default 1), `message` the tip's subject.

`providers.<provider>` asserts the twin: `consumed` in order (or a `$set`),
`unmatched`, `requests` (bodies with the case's credential), `model` and
`effort` on every request, and `contains`, a list of `{"request": n, "text": …}`.

`interviews` asserts the receipt: `questions` in receipt order with `node`,
`kind`, `reply`, `delivery`, `invocation_path`, and `ask`, every field a
matcher; `errors`; `consumed` per script entry id; `no_plaintext` lists
values that may appear nowhere in the receipt, stderr, or final context.
An `invocation_path` is `/` for the root and `/<slot>` per nested call; a
parallel branch's slot is `branch:<fork>@<firing>:<index>:<target>`, the
firing being the fork occurrence that opened the group (`crates/attractor/FORMAT.md`,
"Parallel"), and the same form matches a question in an interview script entry.

`lifecycle`: `no_leaked_processes`, `retained_workspace`,
`no_requests_after_cancel` (no twin request logged after the interrupt),
`cancel_reason`, and `prune_removes_sandbox` for a Docker run.

## Placeholders

`{commit:N}`, `{fixture}`, `{remote:NAME}`, `{model}`, `{credential}` are
substituted in `inputs`, `services`, `fixture.env`, and every string inside
`expect` before comparison.
