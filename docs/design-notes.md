# Design notes

These notes record implementation decisions and findings from earlier handoffs.
Counts and unresolved findings describe those investigations, not current support.
For current behavior, see the [engine specification](https://github.com/lithoscomputer/petri/blob/main/engine-spec.md),
[frontend contract](https://github.com/lithoscomputer/petri/blob/main/frontend-handoff.md), and
[GitHub Actions support matrix](https://github.com/lithoscomputer/petri/blob/main/crates/github/SUPPORT.md).

## Decisions the design document left open

**The expression language.** §3 names `ExprId` and says guards are "boolean
expressions over (outcome, contexts)" without fixing the language. `ir::expr`
defines a small total one: literals, variable lookup, field and index access,
arithmetic and comparison, short-circuiting boolean operators, conditionals, array
and object construction, and a fixed function set. Missing fields evaluate to `null`
rather than erroring, so a guard is always total. Two list functions, `sort_by_key`
and `pluck`, exist so a collector can put clone results back in `items` order
without needing lambdas.

Evaluation is total by design: a guard must always produce a boolean, so a run never
fails at the wrong moment over a typo. The cost is that a typo is silently falsy
instead. A strict mode, or an unknown-field lint at load time, is a v2 seam (§8).

**Expression environment.** One `EvalEnv` for guards, `map`, preconditions and
`Expansion.items` alike. `token` and `input` resolve to the payload on the first input
edge and shadow any static of the same name; `nodes.*` and `kv.*` read the run
context; everything else — scope `env`, node identity, generation, attempt, the
firing's own outcome, `item` / `index` in a clone — comes from `StaticCtx`.

The earlier ad-hoc firing-context mechanism is gone rather than coexisting: `outputs`
and `upstream` bindings no longer exist, and a precondition's `success()` folds the
upstream statuses it reads out of `RunContext` by looking up each input edge's source
node. Nothing is threaded through token payloads to carry status.

## Folded back into the design document

These began as implementation notes and are now rules in `engine-spec.md`, because
each follows from something the design already said rather than from a choice the code
made:

- **A splice supersedes the whole template region** (§6, splice semantics). `All` is
  defined over incoming edges *at firing time*, so a template edge that no token can
  cross is a deadlock generator. Superseding is not deletion: the edges stay in the
  graph and only leave the join count. Regression test:
  `superseded_template_edges_do_not_deadlock_the_collector`.
- **Scope release is irreversible** (§3, and a §7 warning). A scope is held until no
  firing, token or deferred join needs it; a path that leaves and returns gets a fresh
  runtime and workspace. Validation warns rather than errors, because whether a given
  path really releases the scope depends on runtime state. The check is a static
  over-approximation.
- **A loop head must join with `Any`** (§7, invariant 8), with the corollary that a
  node cannot be both a multi-branch `All` join and a loop head.
- **`Token.generation`** (§4) and **`Command::StartStep(ResolvedFiring)`** (§5).

Core Semantics Patch 01 landed on top of those: attempts and `RetryPolicy` (§3, §4),
`PartialSuccess` with `is_success_like` as the single classification point (§4),
`RunContext` and `EvalEnv` (§4), event log v2 with replay (§5), and
`RuntimeSpec.requirements` (§3).

## Where the code departs from the documents

Each of these is a place the literal text did not survive contact with the compiler or
with a working execution path.

### From the IR design

1. **Entry nodes and clone entries get synthetic seed edges.** §3 seeds entry nodes
   with a token, but a token names an edge and an entry node has none. The engine
   allocates one seed edge per entry, above every declared edge id, so joins count it
   like any other incoming edge and the firing rule needs no special case: `All` over
   a single seed edge is satisfied by the seed token. Seed edges never appear in a
   `Routing` group and never collide with a declared id
   (`crates/core/engine/tests/seeding.rs`).

2. **`Command::ExpandNode` is defined but not emitted.** `items` is a pure
   expression, so the core evaluates it and builds the splice itself, in the same
   `apply` call. The variant is marked `// reserved: external expansion`.

3. **`ExpandTarget::Subgraph { entry, .. }` requires `entry` to be the expanding
   node.** Otherwise it is ambiguous whether the expanding node runs before the region
   is cloned. Violations are a run error, not a panic.

4. **An entry node may have a back edge pointing at it.** A loop head that is also the
   graph entry is legitimate: the seed starts generation 0 and the back edge starts
   each later one. Validation rejects only *forward* edges into an entry.

5. **Budget exhaustion drops the tokens.** §7 requires a finite cap on looped nodes
   but does not say what happens at the cap. The firing is refused, the tokens are
   dropped, a `RunError::BudgetExceeded` is recorded, and the run folds to failed.

6. **`apply` drains an internal event queue.** The signature is the document's, but
   routing emits tokens by feeding `Event::TokenEmitted` back to itself, each logged
   before it is applied.

### From Core Semantics Patch 01

7. **`RetryOn.statuses` is `Vec<StatusKind>`, not `SmallVec<Status>`.** `Status`
   carries payloads — `Failure(FailureInfo)`, `PartialSuccess { underlying }` — so a
   list of them cannot express "match this variant". `StatusKind` is the payload-free
   discriminant. It is not a second classification point: the never-retry-a-success
   rule still goes through `Status::is_success_like`. `Vec` rather than `SmallVec`
   avoids adding a dependency for a list that is almost always one or two entries.

8. **`RuntimeSpec` became a struct.** It was an enum, and an enum cannot gain a
   common `requirements` field. It is now `RuntimeSpec { target: RuntimeTarget,
   requirements: Vec<SmolStr> }`, with the old variants moved to `RuntimeTarget`.

9. **Log records carry `EventSource`.** §7 test 1 asks for byte-identical replay, but
   nothing said how replay tells a core-emitted event from a host-fed one — and
   replaying core events instead of regenerating them would make the check a copy
   rather than a determinism test. Each record now says which it is, and
   `verify_replay` feeds back only the `External` ones.

10. **`context_updates` merge on final attempts only.** §1.2 pins "run-context
    recording of the final status" to the final attempt but does not say what happens
    to `kv` on an attempt that is then retried. Merging a discarded attempt's writes
    would leave state from work that was thrown away, so the merge follows the same
    rule as the status record. **Worth a second opinion** — a step that wants to leave
    a breadcrumb on every attempt cannot, today.

11. **A retry re-resolves the step config.** The attempt after a backoff binds its
    config against the run context as it stands then, not as it stood before attempt
    one. Deterministic either way; this way a retry can see what changed.

12. **`Exhaustion::AcceptPartial` fires whenever a retryable status has no attempts
    left**, including `max_attempts: 1` where no retry was ever possible. Reading it
    the other way would make `allow_partial` silently inert unless retries were also
    configured. The driver applies it (`RetryPolicy::finalize`) to every returned
    attempt before a host prepares the result, so the engine records the finish it
    is given and a host's prepared result is never converted again; the engine
    applies it only to the `invalid_splice` failure it makes itself.

13. **`base_delay` uses repeated multiplication, not `powi`.** `powi` is not
    guaranteed bit-identical across platforms, and the delay goes into the log.

14. **`Status::is_success()` and `FailureInfo.retryable` are gone.** The first was a
    near-duplicate of `is_success_like` sitting next to it in the same `impl` —
    exactly the footgun the one-classification-point rule warns about. The second was
    a second retry signal competing with `RetryPolicy` and `FailureInfo.class`.

15. **A firing awaiting a retry stays live.** That is what keeps its scope held and
    the run non-quiescent across the backoff, rather than a run appearing to finish
    mid-retry.

16. **`NodeRecord.gen` is `generation`**, for the same keyword reason as `Token`.

17. **`StaticCtx` is a defined type.** §2.3 names `statics: &StaticCtx` without
    saying what it holds. It holds the per-firing bindings that are neither the token
    nor run state: scope `env`, node identity, generation, attempt, the firing's own
    outcome, and `item` / `index`.

### From the executor handoff

18. **`Executor::release` takes a `ScopeOutcome`.** The handoff's signature is
    `release(&self, env: EnvHandle)`, but the retention default is *keep on failure*
    and an environment cannot know whether the work inside it failed. The driver
    knows, so it says.

19. **`ProcessHandle` exposes one merged `lines()` stream, not `stdout()` and
    `stderr()`.** §3.4 requires log lines in arrival order across both streams, and
    merging two receivers after the fact cannot recover an order that was never
    recorded. Each line carries its stream tag, so nothing is lost.

20. **The cancellation escalation is in `output`, not a failure class.** §2.4 asks
    for `Status::Cancelled` with class `"cancel_forced"`, and §7 test 5 for a class
    recording the TERM-to-KILL escalation — but `Status::Cancelled` carries no
    `FailureInfo`, and adding one would widen a closed enum that core §4 says is
    permanent. So `cancel_escalation` is a field on the outcome's output object,
    valued `sigterm`, `sigkill` or `cancel_forced`.

21a. **In-container signals are `kill -SIG -PGID`, with no `--` separator.** The
    handoff spells it `docker exec <c> kill -<SIG> -- -<PGID>`, which is the POSIX
    form — and busybox rejects it outright: `sh: invalid number '--'`. A rejected
    signal is a silent one, so cancellation never reached the step; the ladder waited
    out its whole grace period and the step ran on until the container was torn down.
    Since alpine is the obvious base image, the separator cannot be used.
    `kill -TERM -123` is understood by busybox ash, dash and bash alike, and the call
    goes through `sh -c` so it is the shell builtin rather than whichever `kill`
    binary the image carries.

    This one hid behind a green test. `docker_cancel_kills_the_exec_process_group`
    asserted that a backgrounded grandchild stopped ticking after a cancel — which it
    did, but because release removed the container, not because the signal landed.
    The test now also asserts the escalation was `sigterm`: that step does not trap
    TERM, so TERM alone must have ended it, and a signal that never arrives shows up
    as a `sigkill` escalation instead. **A cancellation test that only checks the
    process is gone will pass on teardown alone; it has to pin how it went.**

21b. **Docker `wait` follows the process group, not the `docker exec` client.** `setsid` forks when its caller is already a process-group
    leader, and whether `docker exec` hands it one is not something to rely on. When
    it forks, `setsid` exits as soon as the child is running and `docker exec` returns
    0 while the step is still going — the status is lost, and so is the step's real
    duration. A test caught this: `exit 7` came back `Success`. The wrapper now
    records the status beside the pgid, and `wait` polls the process group's liveness
    rather than trusting an early return, which makes the executor correct whichever
    way `setsid` behaves. `docker_wait_follows_the_step_not_the_client` pins both the
    status and the duration.

    The same reasoning covers a second way the client lies. After `SIGTERM` the
    wrapper dies at once — it traps nothing — so the client returns while a step that
    *does* trap TERM is still running. Believing it there reports a graceful exit,
    skips the escalation to `SIGKILL`, and leaves the step running. So `wait` waits
    for a recorded status or a dead process group, whichever comes first; `SIGKILL`
    guarantees the second, which is what bounds the loop.
    `docker_cancel_without_a_recorded_status_still_reports_cancelled` pins the path
    where the wrapper is killed before it can record anything.

22. **The ladder lives in the step kind; the driver owns the outer deadline and the
    timeout-versus-cancel decision.** A step reports `Cancelled` however it was
    stopped, because it cannot know why. Only the driver sees which terminal arrived
    first, so it rewrites `Cancelled` to `TimedOut` when its timer got there first.
    This keeps `Control` closed at `Cancel` rather than growing a variant per reason.

23. **Retry jitter is a hash of `(firing, attempt)`, not an RNG.** Jitter's job is
    decorrelating *different* retries so they do not stampede, which a per-firing hash
    does. It avoids a dependency and leaves the driver reproducible, so retry timing
    is testable.

24. **`ResolvedFiring` never actually reaches the event log here.** §6.1 motivates
    the secret-reference amendment with "`ResolvedFiring` is serialized into
    `StartStep` records", but in this implementation commands are not events, so no
    command is ever logged. The amendment is implemented anyway — it is the right
    invariant for any host that persists commands — and the secret test greps the
    whole serialized `EngineState`, not just the log, which is the stronger check.

25. **`split(string, separator)` joined the expression language**, and the language
    now has a gate. The outputs-file protocol yields strings, so a step that produces
    a list of regions produces one string, and `for_each` needs an array — acceptance
    test 1 cannot be written without it.

    Three functions had by then accreted under pressure from individual tests, which
    is how ad-hoc scripting languages are born. `ir::expr::BUILTINS` is now a table
    that **gates dispatch** rather than describing it: a call is looked up there
    before any match arm is reached, and arity is checked once from the entry. A
    function missing from the table is unknown however many arms exist, and an entry
    with no arm fails its own conformance test. The bar for adding one — pure, total,
    tested including error cases, and justified by something that cannot be written
    without it — is documented on the table itself.

26. **Docker images must provide `setsid`.** busybox and util-linux both do, so
    alpine, debian and ubuntu are all fine. A spawn into an image without it fails
    with a spawn error rather than silently losing the process group.

27. **Log draining after a process ends is bounded at 5 seconds.** §3.4 says capture
    continues "until both streams close", which a grandchild holding the pipe open can
    delay indefinitely. The driver's hard deadline would eventually fire, but bounding
    the drain keeps the failure local and legible.

28. **`ScopeSpec`, `EnvHandle` and `Teardown` are defined here.** §1 names the first
    two without giving their shape.

29. **`RunHandle` is how a cancel gets in.** The handoff has `CancelRequested`
    arriving as an event without saying who sends it; `Driver::handle()` returns a
    handle that can inject one into a run in flight.

30. **Cancelled outcomes route, and stopping has two tiers.** Frontend finding 3:
    GitHub runs `if: always()` and `if: cancelled()` cleanup after a cancel, and
    the engine could not, ever — the spec had cancellation drop the scope's
    tokens, making `Cancelled` the one terminal status that was recorded but
    never routed. Spec §5 now says: a cancelled firing's outcome routes like any
    other, pending tokens survive, and what stops work from restarting is
    **structural** — a node in a cancelled scope, or fed by a token from a firing
    that recorded `Cancelled`, completes `Cancelled` without evaluating anything
    unless it carries `Node.run_on_cancel`. No gate anywhere has to be right for
    cancellation to be safe (review showed the implicit gates could not be: a
    job's first step, a dependent whose needs finished pre-cancel, and a
    `max_parallel`-deferred clone all pass them). The engine's *previous* cancel
    behavior was renamed rather than deleted: `Event::KillRequested` is the
    forced tier — tokens drop, nothing routes, nothing is admitted,
    `Control::Kill` goes straight to `SIGKILL` — and it is in the log, so replay
    gets the hard stop for free and the mode of stopping is recorded, never
    inferred. The driver wires the tiers as cleanup grace and second-cancel; the
    host executor grew sentinel-pinned process groups so release can end
    stragglers without ever signalling a recycled pgid. Log v2 → v3. (The GHA
    frontend has since stopped choosing where the flag lands from condition
    text: it sets `run_on_cancel` on every node it emits — matrix expansion
    heads excepted — and lets each step's gate evaluate the condition against
    the post-cancel state. The engine's structural rule is unchanged; the flag
    just stopped being an admission *decision* in that frontend.)

## Frontends

Both frontends are pure: text in, `Graph` and diagnostics out. One parser reads the
`${{ }}` grammar; two lowerings give it meaning. The GHA lowering maps `==` onto
`loose_eq`, `&&` onto a value-returning conditional, and every GitHub function onto a
table entry — there is no second evaluator, and a property test asserts that whatever
parses lowers only onto `ir::expr::BUILTINS`. The native lowering maps the same syntax
onto the engine's own `Eq` / `And` / `Not`.

### Findings for the spec

These are places a real workflow needs something the engine or the expression table
cannot express. They are reported, not worked around; each is rejected with the code
in brackets.

1. **There was no per-run input channel.** The `github`, `vars` and `runner` contexts
   are values a host knows and the graph does not. I added `Graph.params`: a read-only
   map merged into every firing's statics at the lowest precedence, filled in by the
   host before the run. It lives on the graph rather than in a `RunStarted` payload so
   the graph a run used is self-describing and replay needs nothing more. This is an
   IR extension, made because nothing in the package works without it; if it should
   live elsewhere, it is a small swap.

2. **`hashFiles()` cannot be a total builtin** [`unsupported.expression.hashFiles`].
   It reads the workspace at run time. Resolved for step config: a literal-pattern
   call in `run:`, `env:` or `with:` lowers to a sentinel — the second permitted
   non-literal across the executor boundary, like `$secret` — which the GitHub step
   kinds replace at spawn with the hash, computed in the job environment
   (`crates/github/actions/src/hashfiles.rs`). Resolved for step-level conditions
   too: an `if:` lowers to a config gate the step evaluates at spawn, and the
   sentinel rides in the gate's literals (the React sizebot shape,
   `if: hashFiles(...) != ''`, runs). In a position the engine evaluates — a job
   `if:`, an output, a matrix, or under another function in a condition — it
   stays rejected, and so do computed patterns.

3. **Nothing ran after a cancel** — resolved; see departure 30 and spec §5.
   `if: cancelled()` and `if: always()` cleanup can now run: cancelled outcomes
   route, admission is the structural `Node.run_on_cancel` flag, and the forced
   tier (`KillRequested`) keeps the old stop-everything semantics under its own
   logged event. The pinning test flipped into
   `cancelled_steps_run_after_a_cancel`.

4. **One scope per job, so no per-leg runtime** [`unsupported.runs_on.expression`].
   `runs-on: ${{ matrix.os }}` is the single most common reason a matrix job is
   rejected: 47 workflows. Each leg would need its own environment. Either scopes
   become per-clone, or the frontend splits a matrix over `runs-on` into one job per
   OS at lowering time.

5. **Secrets in expressions** [`unsupported.secrets.expression`]. `if: ${{ secrets.X
   != '' }}` and `${{ secrets.A || secrets.B }}` are real idioms — 12 workflows. The
   engine keeps secrets out of `EvalEnv` by construction, and the docs' context
   matrix gives conditions no `secrets` context either, so the rejection stands
   (we adhere to the docs even where GitHub accidentally accepts more). The
   sanctioned form works now: pass the secret through an environment variable and
   test `env.NAME` in the step's `if:` — the gate resolves it at spawn,
   step-side, so no value ever enters an expression the engine evaluates. A job
   output that is a whole-value secret is dropped with a warning
   (`ignored.secret_output`), as GitHub drops it.

6. **Background steps are a job-local DAG** — resolved. `background: true`
   fans a `run:` or `uses:` step out from the foreground chain. `wait`,
   `wait-all`, and `parallel` add explicit joins, and an implicit wait joins
   remaining work before post-action cleanup. A private environment channel
   defers outputs, `GITHUB_ENV`, and `GITHUB_PATH` until the join publishes the
   step. `cancel: id` stops that branch through an engine cancellation group,
   joins it, and publishes its effects. The target records `cancelled`; the
   cancel control succeeds and the foreground job continues. Matrix legs and
   deferred composite actions preserve their cancellation boundaries.

7. **Action manifests resolve when their steps run** — resolved. Remote
   references still pin at lowering, but local and pinned manifests use one
   run-time resolver path. `uses: ./localClone` and
   `uses: ./node/.github/actions/x` can name directories created by an earlier
   step. A remote composite or called workflow can also use a `./` action from
   its own pinned repository.

8. **`GITHUB_ENV` and `GITHUB_PATH`** are job-scoped mutable env across steps.
   Resolved: the `github/run` and `github/action` step kinds (`crates/github/actions`)
   accumulate them in files in the job's workspace and apply them to every later
   step, with no core support .

9. **The positional YAML reader has two gaps** [`unsupported.yaml.anchors`,
   `unsupported.yaml.multiline_flow`]: no anchors or aliases, and a multi-line
   `[ … ]` value followed by a dedent is misread. Three cpython workflows. Both are
   library limitations, named as such.

Custom shells are resolved: `python`, `pwsh` and any `{0}` template (`bash -el {0}`,
`/usr/bin/env bash {0}`) lower now — the step writes the script to a file and runs
the template over it, as GitHub does; only the Windows-only `cmd` and `powershell`
stay rejected. `concurrency:` is ignored with a warning (`ignored.concurrency`):
cross-run mutual exclusion has nothing to race in a single local run, and its
cross-run semantics stay with the multi-run driver layer. Windows and macOS
runners are out of scope (`runs_on.windows`, `runs_on.macos`): the local executor
emulates Linux runners only. The declared support matrix is
`crates/github/SUPPORT.md`, held in sync with the corpus report by a harness test.
Reusable workflows now run as child invocations, including a call matrix with
a job matrix inside the child. Remaining `runs-on` expression limits are listed
in the support matrix.

### Departures from the frontend handoff

30. **`Graph.params`** — see finding 1.

31. **Twenty-three new builtins, one gate — and none of them GitHub's.** The
    handoff's mapping rule (no second evaluator) means GitHub's operators and
    functions must become table entries, so the question is what *kind* of entry.
    The rule adopted: a primitive may enter the core when it is generic — loose
    JavaScript-family coercion (`loose_eq`/`lt`/`le`/`gt`/`ge`/`truthy`/`number`/
    `string`), string functions (`contains_ci`, `starts_with`, `ends_with`, `format`,
    `join`, `to_json`, `from_json`), record access (`get_ci`, `values`,
    `pluck_present`, `keys`, `omit`) and record combinators (`cartesian`,
    `reject_where`, `extend_where`). Nothing uniquely GitHub's may. GitHub's matrix
    `include`/`exclude` rule, which an earlier draft had as a `matrix_combinations`
    builtin, is now a *composition* the GHA frontend emits:
    `extend_where(reject_where(cartesian(omit(m, [include, exclude])), m.exclude),
    m.include, keys(axes))` — the engine has combinators, GitHub has a use of them,
    and Buildkite's `adjustments` will compose the same pieces differently. The table
    grew from 19 to 42 and still gates dispatch. See "What the core may know" above.

32. **Coercion follows the runner, not the docs' wording.** GitHub's docs say a string
    coerces "from any legal JSON number format"; the runner uses .NET's
    `AllowLeadingSign | AllowDecimalPoint | AllowExponent`, which also takes `+1`, `01`
    and `1.`. Workflows run against the runner.

33. **`.` on a plain array is `null`, not a filter.** The handoff says "`.` on arrays
    (object-filter semantics)". In the runner, property access maps over elements only
    on a *filtered* array — the result of `*`; on an ordinary array it is `null`.
    `a.list.k` is `null` and `a.list.*.k` is the mapped list, and the test pins both.
    Filtered-ness is tracked statically in the lowering, so no runtime flag exists.

34. **Every GHA property access is `get_ci`.** Contexts are case-insensitive in
    GitHub; `Field` is not. `github.Event_Name` works, at the cost of a call per
    property.

35. **Status functions expand per site, not to the core builtins.** A step's
    `success()` means "no earlier step of this job failed, and the job was not
    cancelled out from under it", built as an expression over the earlier steps'
    `nodes.*` records (plus the `scope_cancelled` static, waived when the job's
    own gate admitted it *after* the cancel — cleanup jobs run their plain steps,
    as GitHub's do) and conjoined with "the job started"; a job's means "every
    needed job's `done` reports success and the run is not cancelled". The core's
    `success()` builtin folds immediate upstream edges, which is not GitHub's
    job-status rule. A step or job `if:` naming no status function gets
    `success() &&` in front, as GitHub does; one that names any status function
    stands alone. Job conditions stay engine preconditions; step conditions ride
    as config gates the step evaluates at spawn (which is what lets them read
    `env.*` — `GITHUB_ENV` appends included — and `hashFiles`), with the same
    expressions inside as `$expr` leaves.

36. **Jobs get `start` and `done` noop nodes.** `start` is the gate (`needs` + `if:`);
    `done` folds the job — `{ result, outputs }` — from the payload the last step's
    edge carries, one per matrix leg, and fans out to dependents. Every `needs.J.*`
    reads `J/done`. `NoopStep` is a step kind whose output is its resolved config,
    which makes a structural node also a way to compute a value.

37. **`kv` is unused by the GHA lowering.** Job outputs ride the summary payload
    instead; there was no need for run-scoped state.

38. **`secrets.misplaced` became `unsupported.secrets.expression`.** The handoff
    asked for an Error diagnostic; this is one, coded as a rejection because it is an
    engine property by construction, not a user mistake — and so it counts as
    specific in the corpus.

39. **`permissions:` and `on:` are accepted.** Both describe the GitHub side —
    token scope, triggers — and change nothing about how the graph runs locally, so
    they are not "parse and ignore" in the sense the rule forbids. `permissions` gets
    an `ignored.permissions` warning so nobody wonders; `on:` is the host's business.

40. **Fuzzing is proptest, not cargo-fuzz.** Random text, random bytes lossily
    decoded, and random glued syntax fragments, 600 cases each, assert the parser
    never panics. cargo-fuzz needs nightly; this runs in the ordinary suite.

41. **`serde_json` has `preserve_order` on, workspace-wide.** Matrix axes,
    `include` order and outputs follow document order in GitHub; a sorted map
    silently reordered them (the matrix test caught it).

42. **The driver now records `StepStarted`.** It never had, only the test harness
    did — so a real run's log had no start events. A driver gap, found by the first
    test that read a real driver's log for them.

43. **Reusable-workflow callers are not asked for `runs-on`**, `strategy:` without
    `matrix:` is legal, `uses: ./` is the repository root, and current GitHub-hosted
    labels (`ubuntu-slim`, `ubuntu-26.04`, `ubuntu-24.04-arm`, `macos-15`) are known.
    All four came from the corpus; each cost a real workflow a generic error before.
    (macOS labels have since moved out of scope — `runs_on.macos`; the local executor
    emulates Linux runners only.)

44. **`KNOWN_RUNS_ON` lives in the frontend.** The handoff puts label mapping in the
    executor; `petri check` still needs to reject unknown labels without one, so the
    frontend carries the list the local executor honours. Third-party runner labels
    (`depot-*`, `namespace-profile-*`, `*-16-core-*`) are rejected per label: 23
    workflows.

45. **Composite outputs are expression substitution.** `steps.<caller>.outputs.<n>`
    resolves to the action's declared `value:` expression, lowered in the composite's
    own site — no node, no record.

46. **`GITHUB_OUTPUT` is an alias**, via `ProcessConfig.output_env_aliases`. A `run:`
    step that appends to it lands in the outputs file without a shim.

## Scope boundaries

Cross-run concurrency belongs to the host. The GitHub Actions frontend ignores
`concurrency:` with a warning. `RuntimeSpec.requirements` carries placement labels
without interpreting them. See the format contracts for supported features;
these historical notes are not a roadmap.
