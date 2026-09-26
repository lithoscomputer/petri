import PetriModel.Join

/-!
# Flows

A whole run of the flows `crates/core/engine/tests/flow/mod.rs` generates:
every step is a `noop` with a scripted outcome, every routing
group picks its first arm whose guard passes (`FirstMatch` with
`Fallthrough::NoEmit`), and each `(node, generation)` key's join is
`Join.arrive`. A back arm starts the next generation; a node fires at most
`maxFirings` times over all generations, and a key the budget refuses is
marked fired with no record and no routing (`engine-spec.md` §4, "Budget
refusal"). The host finishes one live firing per step, chosen by the case's
schedule.

This is the model the Rust check runs against the real core. It leaves out
everything the generator does not produce: retries, cancellation,
expansions and preconditions.
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

/-- What the host reports for a firing. -/
inductive Outcome where
  | success
  | failure
  /-- A failure of class `flaky`. -/
  | flaky
  | timedOut
  deriving Repr, DecidableEq

/-- `Status::is_failure`: a failure or a timeout. -/
def Outcome.isFailure : Outcome → Bool
  | .success => false
  | .failure | .flaky | .timedOut => true

/-- `Status::tag`. -/
def Outcome.tag : Outcome → String
  | .success => "success"
  | .failure | .flaky => "failure"
  | .timedOut => "timed_out"

structure Node where
  join : Join.Policy
  maxFirings : Nat
  /-- The host's outcome for the node's first, second, … firing; the last
  entry repeats. -/
  outcomes : List Outcome
  groups : List (List Arm)
  deriving Repr

structure Case where
  nodes : List Node
  schedule : List Nat
  deriving Repr

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
  deriving Repr

/-- `always()`, `success()` and `failure()` over the node's own outcome, as
the expression builtins define them: `success()` is success-like, and
`failure()` is the `failure` status only, so a timeout passes neither. -/
def Guard.passes : Guard → Outcome → Bool
  | .always, _ => true
  | .success, o => o == .success
  | .failure, o => o == .failure || o == .flaky

/-- The host's outcome for the node's firing number `ordinal`, from 0. -/
def Node.outcome (node : Node) (ordinal : Nat) : Outcome :=
  (node.outcomes[min ordinal (node.outcomes.length - 1)]?).getD .success

/-- A group emits on its first arm whose guard passes, or not at all. -/
def emit (outcome : Outcome) (group : List Arm) : Option Arm :=
  group.find? (·.guard.passes outcome)

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
  /-- Running firings with their scripted outcome, ascending by key. -/
  live : List (Key × Outcome)
  steps : List (List Key)
  finished : List (Key × Outcome)
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
with its scripted outcome. A key the join fires is refused when its node's
budget is spent (§4, "Budget refusal"). -/
def deliver (c : Case) : State → List Token → State × List (Key × Outcome)
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
        (r.1, (k, node.outcome (s.firingsOf t.target)) :: r.2)

def keyLe (a b : Key × Outcome) : Bool :=
  a.1.1 < b.1.1 || (a.1.1 == b.1.1 && a.1.2 ≤ b.1.2)

def start (c : Case) : State :=
  let s₀ : State := {
    keys := [], firings := List.replicate c.nodes.length 0, live := []
    steps := [], finished := [], budgetExceeded := [] }
  let r := deliver c s₀ ((seeds c).map fun (node, edge) => ⟨node, 0, edge⟩)
  let started := r.2.mergeSort keyLe
  { r.1 with live := started, steps := [started.map (·.1)] }

/-- The tokens a finished firing routes, in group order. -/
def tokensOf (c : Case) (k : Key) (outcome : Outcome) : List Token :=
  let groups := (c.nodes[k.1]?.map (·.groups)).getD []
  (groups.filterMap (emit outcome)).map fun arm =>
    ⟨arm.to, if arm.back then k.2 + 1 else k.2, arm.edge⟩

/-- The host finishes one live firing: record it, route it, deliver its
tokens. -/
def finish (c : Case) (s : State) (firing : Key × Outcome) : State :=
  let s := { s with
    live := s.live.erase firing
    finished := s.finished ++ [firing] }
  let r := deliver c s (tokensOf c firing.1 firing.2)
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

def run (c : Case) : Observed :=
  let fuel := (c.nodes.map (·.maxFirings)).sum + 1
  let s := loop c fuel 0 (start c)
  let parked := (s.keys.flatMap fun ((node, generation), key) =>
      if key.fired then [] else key.tokens.map fun edge => (node, generation, edge)).mergeSort parkedLe
  let status :=
    if !s.live.isEmpty then "unsettled"
    else if s.finished.any (·.2.isFailure) || !s.budgetExceeded.isEmpty then "failed"
    else "success"
  { steps := s.steps
    finished := s.finished.map (·.1)
    parked
    budgetExceeded := s.budgetExceeded
    status }

end PetriModel.Flow
