import PetriModel.Join

/-!
# Flows

A whole run of the flows `crates/core/engine/tests/flow/mod.rs` generates:
every step is a `noop`, every routing group picks its first arm whose guard
passes (`FirstMatch` with `Fallthrough::NoEmit`), and each
`(node, generation)` key's join is `Join.arrive`. A back arm starts the next
generation; a node fires at most `maxFirings` times over all generations, and
a key the budget refuses is marked fired with no record and no routing
(`engine-spec.md` §4, "Budget refusal"). The host finishes one live firing per
step, chosen by the case's schedule.

A `Case` is what routing sees: the graph, and the status each firing
records. Retries never reach it; `PetriModel/Retry.lean` builds a `Case` from
the full generated case, retry policies and scripted attempts included.

This is the model the Rust check runs against the real core. It leaves out
everything the generator does not produce: cancellation, expansions and
preconditions.
-/

namespace PetriModel.Flow

inductive Guard where
  | always
  | success
  | failure
  deriving Repr, DecidableEq

structure Arm where
  to : Nat
  guard : Guard
  back : Bool
  edge : Nat
  deriving Repr

/-- What the host reports for one attempt. -/
inductive Outcome where
  | success
  | failure
  /-- A failure of class `flaky`. -/
  | flaky
  | timedOut
  deriving Repr, DecidableEq

/-- A recorded status (`Status`). A partial success keeps the outcome it was
converted from (§3.1 rule 3). -/
inductive Status where
  | success
  | partialSuccess (underlying : Outcome)
  | failure
  | timedOut
  deriving Repr, DecidableEq

/-- `Status::is_success_like`. -/
def Status.isSuccessLike : Status → Bool
  | .success | .partialSuccess _ => true
  | .failure | .timedOut => false

/-- `Status::is_failure`: a failure or a timeout. -/
def Status.isFailure : Status → Bool
  | .failure | .timedOut => true
  | .success | .partialSuccess _ => false

/-- `Status::tag`. -/
def Status.tag : Status → String
  | .success => "success"
  | .partialSuccess _ => "partial_success"
  | .failure => "failure"
  | .timedOut => "timed_out"

/-- A node as routing sees it. -/
structure Node where
  join : Join.Policy
  maxFirings : Nat
  groups : List (List Arm)
  deriving Repr

structure Case where
  nodes : List Node
  schedule : List Nat
  /-- The status node `n`'s firing number `ordinal` (from 0) records. -/
  record : Nat → Nat → Status

/-- A `(node, generation)` key. -/
abbrev Key := Nat × Nat

/-- What a run looks like from outside the core; `Observed` in the Rust
test. -/
structure Observed where
  steps : List (List Key)
  finished : List Key
  parked : List (Nat × Nat × Nat)
  budgetExceeded : List Nat
  status : String
  /-- `(node, generation, attempts, status tag)` per finished firing. -/
  attempts : List (Nat × Nat × Nat × String)
  /-- `(node, generation, next attempt, base delay in nanoseconds)` per
  scheduled retry. -/
  retries : List (Nat × Nat × Nat × Nat)
  deriving Repr

/-- What routing and the run context see: everything but the attempt counts
and the retries; `Observed::routing` in the Rust test. -/
def Observed.routing (o : Observed) : Observed :=
  { o with
    attempts := o.attempts.map fun (node, generation, _, tag) => (node, generation, 1, tag)
    retries := [] }

/-- `always()`, `success()` and `failure()` over the node's recorded status,
as the expression builtins define them: `success()` is success-like, and
`failure()` is the `failure` status only, so a timeout passes neither. -/
def Guard.passes : Guard → Status → Bool
  | .always, _ => true
  | .success, status => status.isSuccessLike
  | .failure, status => status == .failure

/-- A group emits on its first arm whose guard passes, or not at all. -/
def emit (status : Status) (group : List Arm) : Option Arm :=
  group.find? (·.guard.passes status)

def arms (c : Case) : List Arm :=
  c.nodes.flatMap (·.groups.flatten)

/-- Entry nodes: those no forward arm targets, in node order. A loop head may
be one: seeding considers forward edges only (§8). -/
def entries (c : Case) : List Nat :=
  (List.range c.nodes.length).filter fun n => !(arms c).any fun a => !a.back && a.to == n

/-- Seed edges are numbered after the largest declared edge, one per entry
node in order (`EngineState::new`, `seed_execution`). -/
def seeds (c : Case) : List (Nat × Nat) :=
  let base := match ((arms c).map (·.edge)).max? with
    | some m => m + 1
    | none => 0
  (entries c).zipIdx.map fun (node, i) => (node, base + i)

/-- The edges that count toward a node's join (`incoming_edges`): every arm
aimed at it, back arms included, and its seed edge. -/
def incoming (c : Case) (node : Nat) : List Nat :=
  ((arms c).filter (·.to == node)).map (·.edge) ++
    ((seeds c).filter (·.1 == node)).map (·.2)

/-- A token on its way to a key. -/
structure Token where
  target : Nat
  generation : Nat
  edge : Nat
  deriving Repr

structure State where
  /-- Keys that have seen a token; any other key is `{}`. -/
  keys : List (Key × Join.Key)
  /-- Firings so far, per node. -/
  firings : List Nat
  /-- Running firings with their node's firing number, ascending by key. -/
  live : List (Key × Nat)
  steps : List (List Key)
  finished : List (Key × Nat)
  budgetExceeded : List Nat

def State.key (s : State) (k : Key) : Join.Key :=
  ((s.keys.find? (·.1 == k)).map (·.2)).getD {}

def State.setKey (s : State) (k : Key) (v : Join.Key) : State :=
  { s with keys := (k, v) :: s.keys.filter (·.1 != k) }

def State.firingsOf (s : State) (node : Nat) : Nat :=
  s.firings[node]?.getD 0

/-- Count one more firing of `node`. -/
def State.bump (s : State) (node : Nat) : State :=
  { s with firings := s.firings.set node (s.firingsOf node + 1) }

/-- Deliver tokens in order; returns the state and the firings started, each
with its node's firing number. A key the join fires is refused when its node's
budget is spent (§4, "Budget refusal"). -/
def deliver (c : Case) : State → List Token → State × List (Key × Nat)
  | s, [] => (s, [])
  | s, t :: rest =>
    match c.nodes[t.target]? with
    | none => deliver c s rest
    | some node =>
      let k := (t.target, t.generation)
      let step := Join.arrive node.join (incoming c t.target) (s.key k) t.edge
      let s := s.setKey k step.1
      if !step.2 then deliver c s rest
      else if node.maxFirings ≤ s.firingsOf t.target then
        deliver c { s with budgetExceeded := s.budgetExceeded ++ [t.target] } rest
      else
        let r := deliver c (s.bump t.target) rest
        (r.1, (k, s.firingsOf t.target) :: r.2)

def keyLe (a b : Key × Nat) : Bool :=
  a.1.1 < b.1.1 || (a.1.1 == b.1.1 && a.1.2 ≤ b.1.2)

def start (c : Case) : State :=
  let s₀ : State := {
    keys := [], firings := List.replicate c.nodes.length 0, live := []
    steps := [], finished := [], budgetExceeded := [] }
  let r := deliver c s₀ ((seeds c).map fun (node, edge) => ⟨node, 0, edge⟩)
  let started := r.2.mergeSort keyLe
  { r.1 with live := started, steps := [started.map (·.1)] }

/-- The tokens a finished firing routes, in group order. -/
def tokensOf (c : Case) (k : Key) (status : Status) : List Token :=
  let groups := (c.nodes[k.1]?.map (·.groups)).getD []
  (groups.filterMap (emit status)).map fun arm =>
    ⟨arm.to, if arm.back then k.2 + 1 else k.2, arm.edge⟩

/-- The host finishes one live firing: record it, route it, deliver its
tokens. -/
def finish (c : Case) (s : State) (firing : Key × Nat) : State :=
  let s := { s with
    live := s.live.erase firing
    finished := s.finished ++ [firing] }
  let r := deliver c s (tokensOf c firing.1 (c.record firing.1.1 firing.2))
  { r.1 with
    live := (r.1.live ++ r.2).mergeSort keyLe
    steps := r.1.steps ++ [(r.2.mergeSort keyLe).map (·.1)] }

/-- Host steps until nothing runs. -/
def loop (c : Case) : Nat → Nat → State → State
  | 0, _, s => s
  | fuel + 1, k, s =>
    match s.live[(c.schedule[k]?.getD 0) % s.live.length]? with
    | none => s
    | some firing => loop c fuel (k + 1) (finish c s firing)

def parkedLe (a b : Nat × Nat × Nat) : Bool :=
  a.1 < b.1 || (a.1 == b.1 && (a.2.1 < b.2.1 || (a.2.1 == b.2.1 && a.2.2 ≤ b.2.2)))

/-- The final state of a run: host steps until nothing runs, within the
steps the budgets allow (`run_settles`). -/
def runState (c : Case) : State :=
  loop c ((c.nodes.map (·.maxFirings)).sum + 1) 0 (start c)

def run (c : Case) : Observed :=
  let s := runState c
  let parked := (s.keys.flatMap fun ((node, generation), key) =>
      if key.fired then [] else key.tokens.map fun edge => (node, generation, edge)).mergeSort parkedLe
  let status :=
    if !s.live.isEmpty then "unsettled"
    else if s.finished.any (fun (k, ordinal) => (c.record k.1 ordinal).isFailure) ||
        !s.budgetExceeded.isEmpty then "failed"
    else "success"
  { steps := s.steps
    finished := s.finished.map (·.1)
    parked
    budgetExceeded := s.budgetExceeded
    status
    attempts := s.finished.map fun ((node, generation), ordinal) =>
      (node, generation, 1, (c.record node ordinal).tag)
    retries := [] }

end PetriModel.Flow
