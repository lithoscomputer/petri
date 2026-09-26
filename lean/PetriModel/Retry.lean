import PetriModel.Flow

/-!
# Retries

The full generated case, retry policies and scripted attempts included, and
how it reduces to the `Case` routing sees. Mirrors `RetryPolicy` in
`crates/core/ir/src/graph.rs` and the retry path of `on_step_finished` in
`crates/core/engine/src/apply.rs` (`engine-spec.md` §3.1, §4):

* a firing retries an outcome its policy retries, never a success, until the
  attempt limit (`attempts`);
* its record is the last attempt, and an exhausted retryable failure becomes a
  partial success under `AcceptPartial`, keeping the failure (`recorded`);
* each retry waits `initial * factor^(n-1)`, capped at `max`, computed in
  IEEE 754 double precision as the engine computes it in `f64` (`baseDelay`).

Routing sees only the recorded status (`Spec.view`), which is why retries are
invisible outside the log.
-/

namespace PetriModel.Flow

/-- Which outcomes a policy retries (`RetryOn`). -/
inductive RetryOn where
  /-- The `Failure` and `TimedOut` statuses. -/
  | default
  /-- Failures of class `flaky` only. -/
  | flaky
  deriving Repr, DecidableEq

structure Retry where
  maxAttempts : Nat
  retryOn : RetryOn
  acceptPartial : Bool
  initialNanos : Nat
  /-- `Backoff.factor`'s bit pattern. -/
  factorBits : UInt64
  maxNanos : Nat
  deriving Repr

structure SpecNode where
  join : Join.Policy
  maxFirings : Nat
  retry : Retry
  /-- Each attempt's outcome for the node's first, second, … firing. The last
  firing's list repeats, and within a list the last attempt repeats. -/
  outcomes : List (List Outcome)
  groups : List (List Arm)
  deriving Repr

/-- A generated case: `FlowCase` in the Rust test. -/
structure Spec where
  nodes : List SpecNode
  schedule : List Nat
  deriving Repr

/-- `RetryPolicy::attempts` reads 0 as 1. -/
def Retry.limit (r : Retry) : Nat := max r.maxAttempts 1

/-- `RetryPolicy::should_retry`: never a success; the default retries a
failure or a timeout, `flaky` a failure of that class only. -/
def Retry.retries (r : Retry) : Outcome → Bool
  | .success => false
  | .flaky => true
  | .failure | .timedOut => r.retryOn == .default

def SpecNode.script (node : SpecNode) (ordinal : Nat) : List Outcome :=
  (node.outcomes[min ordinal (node.outcomes.length - 1)]?).getD []

/-- The outcome of attempt `n`, from 1. -/
def attemptAt (script : List Outcome) (n : Nat) : Outcome :=
  (script[min (n - 1) (script.length - 1)]?).getD .success

/-- From attempt `n`, retry while the policy retries and attempts remain. -/
def finalFrom (r : Retry) (script : List Outcome) (n : Nat) : Nat → Nat × Outcome
  | 0 => (n, attemptAt script n)
  | fuel + 1 =>
    if r.retries (attemptAt script n) && n < r.limit then finalFrom r script (n + 1) fuel
    else (n, attemptAt script n)

/-- How many attempts a firing takes, and its last attempt's outcome. -/
def attempts (r : Retry) (script : List Outcome) : Nat × Outcome :=
  finalFrom r script 1 r.limit

def Outcome.status : Outcome → Status
  | .success => .success
  | .failure | .flaky => .failure
  | .timedOut => .timedOut

/-- `RetryPolicy::finalize`: the last attempt's status, or a partial success
keeping it when `AcceptPartial` meets an exhausted retryable failure. -/
def recorded (r : Retry) (count : Nat) (o : Outcome) : Status :=
  if r.acceptPartial && r.retries o && r.limit ≤ count then .partialSuccess o else o.status

def Spec.record (s : Spec) (node ordinal : Nat) : Status :=
  match s.nodes[node]? with
  | none => .success
  | some n =>
    let a := attempts n.retry (n.script ordinal)
    recorded n.retry a.1 a.2

/-- What routing sees of a generated case. -/
def Spec.view (s : Spec) : Case where
  nodes := s.nodes.map fun n => { join := n.join, maxFirings := n.maxFirings, groups := n.groups }
  schedule := s.schedule
  record := s.record

/-- `RetryPolicy::base_delay`: `initial * factor^(failed - 1)` by repeated
multiplication in double precision, capped at `max`, then truncated to whole
nanoseconds (`Float.toUInt64` saturates and reads NaN as 0, like Rust's
`as u64`). -/
def baseDelay (r : Retry) (failed : Nat) : Nat :=
  let cap := Float.ofNat r.maxNanos
  let factor := Float.ofBits r.factorBits
  let grow : Option Float → Option Float := fun acc => acc.bind fun x =>
    let y := x * factor
    if y ≥ cap then none else some y
  match (List.replicate (max failed 1 - 1) ()).foldl (fun acc _ => grow acc)
      (some (Float.ofNat r.initialNanos)) with
  | none => r.maxNanos
  | some x => if x ≥ cap then r.maxNanos else x.toUInt64.toNat

/-- A run of a generated case: the routing run of its view, with each firing's
attempts and each retry's base delay. -/
def Spec.run (s : Spec) : Observed :=
  let st := runState s.view
  let count := fun (node ordinal : Nat) =>
    match s.nodes[node]? with
    | none => 1
    | some n => (attempts n.retry (n.script ordinal)).1
  let delay := fun (node failed : Nat) =>
    match s.nodes[node]? with
    | none => 0
    | some n => baseDelay n.retry failed
  { Flow.run s.view with
    attempts := st.finished.map fun ((node, generation), ordinal) =>
      (node, generation, count node ordinal, (s.record node ordinal).tag)
    retries := st.finished.flatMap fun ((node, generation), ordinal) =>
      (List.range (count node ordinal - 1)).map fun i =>
        (node, generation, i + 2, delay node (i + 1)) }

/-- The same case with every firing cut to its last attempt's outcome, and
one attempt allowed. -/
def Spec.finalized (s : Spec) : Spec :=
  { s with
    nodes := s.nodes.map fun n =>
      { n with
        retry := { n.retry with maxAttempts := 1 }
        outcomes := (List.range n.outcomes.length).map fun ordinal =>
          [(attempts n.retry (n.script ordinal)).2] } }

end PetriModel.Flow
