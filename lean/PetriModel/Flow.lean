import PetriModel.Join

/-!
# Flows

A whole run of the acyclic flows `crates/core/engine/tests/flow/mod.rs`
generates: every step is a `noop` that succeeds or fails as scripted, every
routing group picks its first arm whose guard passes (`FirstMatch` with
`Fallthrough::NoEmit`), and each node's join is `Join.arrive`. The host
finishes one live node per step, chosen by the case's schedule.

This is the model the Rust check runs against the real core. It leaves out
everything the generator does not produce: loops, retries, cancellation,
expansions, preconditions and budgets.
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
  edge : Nat
  deriving Repr

structure Node where
  join : Join.Policy
  fails : Bool
  groups : List (List Arm)
  deriving Repr

structure Case where
  nodes : List Node
  schedule : List Nat
  deriving Repr

/-- What a run looks like from outside the core; `Observed` in the Rust
test. Keys are `(node, generation)`; this model has only generation 0 and
no budgets, so `budgetExceeded` is always empty. -/
structure Observed where
  steps : List (List (Nat × Nat))
  finished : List (Nat × Nat)
  parked : List (Nat × Nat × Nat)
  budgetExceeded : List Nat
  status : String
  deriving Repr

/-- `always()`, `success()` and `failure()` over the node's own outcome. -/
def Guard.passes : Guard → Bool → Bool
  | .always, _ => true
  | .success, failed => !failed
  | .failure, failed => failed

/-- A group emits on its first arm whose guard passes, or not at all. -/
def emit (failed : Bool) (group : List Arm) : Option Arm :=
  group.find? (·.guard.passes failed)

def arms (c : Case) : List Arm :=
  c.nodes.flatMap (·.groups.flatten)

def failed (c : Case) (node : Nat) : Bool :=
  (c.nodes[node]?.map (·.fails)).getD false

/-- Entry nodes: those no arm targets, in node order (`GraphBuilder::build`). -/
def entries (c : Case) : List Nat :=
  (List.range c.nodes.length).filter fun n => !(arms c).any (·.to == n)

/-- Seed edges are numbered after the largest declared edge, one per entry
node in order (`EngineState::new`, `seed_execution`). -/
def seeds (c : Case) : List (Nat × Nat) :=
  let base := match ((arms c).map (·.edge)).max? with
    | some m => m + 1
    | none => 0
  (entries c).zipIdx.map fun (node, i) => (node, base + i)

/-- The edges that count toward a node's join (`incoming_edges`). -/
def incoming (c : Case) (node : Nat) : List Nat :=
  ((arms c).filter (·.to == node)).map (·.edge) ++
    ((seeds c).filter (·.1 == node)).map (·.2)

structure State where
  keys : List Join.Key
  /-- Running nodes, ascending. -/
  live : List Nat
  steps : List (List Nat)
  finished : List Nat

/-- Deliver tokens `(target, edge)` in order; returns the nodes that fired. -/
def deliver (c : Case) (keys : List Join.Key) : List (Nat × Nat) → List Join.Key × List Nat
  | [] => (keys, [])
  | (target, edge) :: rest =>
    let (keys, fired) := match keys[target]?, c.nodes[target]? with
      | some key, some node =>
        let (key, fired) := Join.arrive node.join (incoming c target) key edge
        (keys.set target key, fired)
      | _, _ => (keys, false)
    let (keys, started) := deliver c keys rest
    (keys, if fired then target :: started else started)

def start (c : Case) : State :=
  let (keys, started) := deliver c (List.replicate c.nodes.length {}) (seeds c)
  { keys, live := started.mergeSort, steps := [started.mergeSort], finished := [] }

/-- The host finishes `node`: record it, route it, deliver its tokens. -/
def finish (c : Case) (s : State) (node : Nat) : State :=
  let groups := (c.nodes[node]?.map (·.groups)).getD []
  let tokens := (groups.filterMap (emit (failed c node))).map fun arm => (arm.to, arm.edge)
  let (keys, started) := deliver c s.keys tokens
  { keys
    live := (s.live.erase node ++ started).mergeSort
    steps := s.steps ++ [started.mergeSort]
    finished := s.finished ++ [node] }

/-- Host steps until nothing runs. Each node fires at most once, so
`nodes + 1` steps are always enough; a run that is still live after them
reports `unsettled`. -/
def loop (c : Case) : Nat → Nat → State → State
  | 0, _, s => s
  | fuel + 1, k, s =>
    match s.live[(c.schedule[k]?.getD 0) % s.live.length]? with
    | none => s
    | some node => loop c fuel (k + 1) (finish c s node)

def run (c : Case) : Observed :=
  let s := loop c (c.nodes.length + 1) 0 (start c)
  let parked := (s.keys.zipIdx.flatMap fun (key, node) =>
      if key.fired then [] else key.tokens.map (node, ·)).mergeSort
    fun a b => a.1 < b.1 || (a.1 == b.1 && a.2 ≤ b.2)
  let status :=
    if !s.live.isEmpty then "unsettled"
    else if s.finished.any (failed c) then "failed"
    else "success"
  let key := fun (node : Nat) => (node, 0)
  { steps := s.steps.map (·.map key)
    finished := s.finished.map key
    parked := parked.map fun (node, edge) => (node, 0, edge)
    budgetExceeded := []
    status }

end PetriModel.Flow
