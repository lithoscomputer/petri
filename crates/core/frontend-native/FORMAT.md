# The native workflow format

A YAML surface over the engine's own model. Where GitHub Actions reaches only the
degenerate subset — no cycles, no `any` joins, no guarded selection — this format
reaches all of it. Every construct below is one engine feature, named for what it is.

Expressions use the `${{ }}` grammar with the **engine's** semantics: `==` is strict
JSON equality, `&&` yields a boolean, and function names are the engine's builtin
table. Bindings an expression may name: `token` / `input` (the payload that arrived),
`inputs`, `nodes.<name>.*`, `kv.*`, `env.*`, `status`, `output`, `outcome`, `item`,
`index`, `generation`, `attempt`, `node`, `run`, `params.*`. Anything else is an
error, with that list in the hint.

```yaml
name: deploy-regions
params:                      # defaults; a host overrides them per run
  regions: [us-east, us-west, eu]

scopes:
  main:
    runtime: host            # or  runtime: { container: { image: alpine:3.20 } }
    env:
      DEPLOY_ENV: staging
      REGION_COUNT: ${{ len(params.regions) }}     # scope env may read params only

nodes:
  plan:
    run: echo "planning"
    next: deploy             # one group, one unconditional arm
  deploy:
    run: echo "deploying"
```

A scope's `runtime` is `host` or `{ container: { image: … } }`, and the
container form takes only the image. What runs the container, and with which
flags, is the executor's configuration, not the workflow's; the native format
carries no container options and no service containers. (The graph has typed
fields for both — `ir::ContainerOptions`, `ir::ServiceOptions` — which the
GitHub frontend fills from `container.options` and `services:`. A native key
for them would lower the same way.)

## Routing — the explicit part

Fan-out is never implicit. A node routes in exactly one of three ways.

**`next:`** — one select group with one unconditional arm. The default case.

```yaml
build:
  run: make
  next: test
```

**`select:`** — one group, arms in order, first passing guard wins. Exactly one
successor. An arm without `when:` matches always and must be last.

```yaml
classify:
  run: ./size.sh
  select:
    - when: ${{ output.size < 3 }}
      to: small
    - when: ${{ output.size < 10 }}
      to: medium
    - to: large
```

A group may require a match: `select: { arms: [...], fallthrough: error }`. With the
default `no_emit`, a group where nothing matches emits nothing — that is how a loop
exits, and how an OR-split drops a branch.

**`parallel:`** — several groups, emitting concurrently. Each entry is one group: a
bare node id (an unconditional arm), an arm mapping (`{ to, when }`), or a list of
arms (a full guarded select).

```yaml
start:
  run: echo go
  parallel:
    - lint                                   # always
    - { to: docs, when: "${{ input.docs }}" }  # conditional: an OR-split
                                               # (quote expressions inside `{ }`)
    - - { to: unit, when: "${{ input.fast }}" }
      - { to: integration }                  # a guarded choice inside the fan-out
```

Edges carry payloads. `map:` on an arm sets the token's payload; without it, the
source's `output` flows on.

The core IR also carries stable `weight`, `label`, and `transition` metadata and a
`SelectionPolicy::Tiered` policy. These fields support compiled adapters such as
Fabro. The current native YAML surface continues to lower to `FirstMatch` and
`EdgeTransition::Continue`.

An invocable compiled graph can set `Graph.result` to `NodeOutput(node)`. The
native YAML surface does not declare this contract yet. Its default is
`ResultProjection::None`, which returns JSON null to an invocation caller.

## Joins

```yaml
gather:
  join: all            # a token on every incoming edge (the default)
  join: any            # the first token fires it; later same-generation tokens drop
  join: { quorum: 2 }  # tokens on two distinct incoming edges
```

Only one arm of a `select:` ever emits, so `join: all` cannot wait on two arms
of one select: the node would never run, and loading rejects it
(`validate.all_join_exclusive_arms`). Use `join: any` on the node a select's
arms meet at.

For the same reason, `{ quorum: n }` needs `n` incoming routes that can each
emit: the arms of one `select:` count once, and an entry node's seed counts
once (`validate.quorum_exceeds_fan_in`). The node a parallel `for_each` exits
to is the exception: each clone adds a route at run time. On a `for_each`
node itself, the join decides when the expansion starts; the clones then start
without waiting on it again.

## Preconditions

`if:` is evaluated in the node's own context before it runs. False means the node
completes `skipped` without executing — and **routing still runs**, so downstream
guards like `always()` see it.

```yaml
publish:
  if: ${{ nodes.test.status == 'success' && input.tag != null }}
  run: ./publish.sh
```

## Loops

A cycle is legal when the arm that closes it is marked `back: true`. Crossing a back
edge increments the token's generation, and joins match tokens per generation. Every
node in a loop needs a finite `budget.max_firings`; that is what makes a runaway loop
terminate.

**Invariant 8.** A node with an incoming back edge is a loop head and must `join:
any` — forward edges into it carry only generation 0, and back edges only later
generations, so `all` over both is unsatisfiable forever. The corollary you will meet
first: a node cannot be both a multi-branch `all` join and a loop head. Put a
dedicated join node in front of the loop head. `join: { quorum: 1 }` on a loop head
is normalized to `any` in lowering rather than the rule being relaxed.

```yaml
start:
  run: echo 0
  next: poll
poll:
  join: any
  budget: { max_firings: 30 }
  run: ./check.sh                  # writes ready=true|false to $CI_OUTPUT
  select:
    - when: ${{ output.ready != 'true' }}
      to: poll
      back: true
    - to: done
done:
  run: echo ready
```

## `for_each`

Declared on the first node of the body. `until:` names the last (default: the same
node). `items:` is an expression yielding an array.

**Parallel** (`parallel: true`, the default) clones the body per element, with `item`
and `index` bound in every clone, and splices one edge per clone into the node the
body exits to. That node's `all` join waits for every clone; `fail_fast` cancels the
siblings when one fails; `max_parallel` bounds how many run at once.

```yaml
plan:
  run: printf 'regions<<EOF\nus-east\nus-west\neu\nEOF\n' > "$CI_OUTPUT"
  next: deploy
deploy:
  for_each:
    items: ${{ split(input.regions, '\n') }}
    parallel: true
    max_parallel: 2
    fail_fast: true
  run: echo "deploying to ${{ item }} (#${{ index }})"
  next: report
report:
  join: all
  run: echo "all regions deployed"
```

**Sequential** (`parallel: false`) uses no engine feature at all. It desugars onto a
back edge and generations, exactly as the spec describes (§5):

- the edge from the feeding node into the head gets `map: { items, idx: 0, acc: [] }`;
- the head is set to `join: any`;
- the tail's `next:` is replaced by one select group with two arms — a back arm to the
  head, guarded `idx + 1 < len(items)`, with `map` advancing `idx` and appending the
  tail's `output` to `acc`; and an exit arm to the original `next:` target carrying
  `acc ++ [output]`, every result in order;
- every node in the body gets `budget: { max_firings: max_iterations }`.

Inside the body, `${{ item }}` and `${{ index }}` read `input.items[input.idx]` and
`input.idx`, so a body reads the same either way.

```yaml
plan:
  run: printf 'regions<<EOF\nus-east\nus-west\neu\nEOF\n' > "$CI_OUTPUT"
  next: deploy
deploy:
  for_each:
    items: ${{ split(output.regions, '\n') }}   # evaluated on `plan`'s outcome
    parallel: false
    max_iterations: 10
  run: echo "deploying to ${{ item }}"
  next: report
report:
  run: echo "regions deployed in order"        # receives [output, output, output]
```

`petri check --print-graph` shows exactly what a loop became.

## Budgets, retries, steps

```yaml
flaky:
  run: ./deploy.sh
  budget: { max_firings: 1, timeout: 10m }     # timeout is per attempt
  retry:
    max_attempts: 3
    backoff: { initial: 2s, factor: 2, max: 1m, jitter: true }
    retry_on: { statuses: [failure, timed_out], classes: [exit_status:75] }
    on_exhaustion: accept_partial              # or fail (the default)
```

`step:` names the step kind — `process` (implied by `run:`) or `noop`. A `noop`
returns its resolved `config:` as its output, which makes it a way to compute a value:

```yaml
summary:
  join: all
  step: noop
  config:
    passed: ${{ !contains(pluck(inputs, 'status'), 'failure') }}
```

`config:` is passed to the step kind. Strings containing `${{ }}` are resolved against
the firing's environment before the step runs; a lone `${{ e }}` keeps `e`'s type.
