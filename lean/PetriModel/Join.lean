/-!
# Joins

The firing rule for one `(node, generation)` key (`engine-spec.md` §3, §4).
It mirrors `on_token`, `try_fire` and `is_join_satisfied` in
`crates/core/engine/src/apply.rs`, without the parts this model leaves out:
forced entries, cancellation, `max_parallel` and budgets.

Edges are plain numbers. A key's parked tokens are the distinct edges that
hold one: a second token on the same edge replaces the first (`store_token`
inserts into a map keyed by edge), so it never counts twice.
-/

namespace PetriModel.Join

/-- `JoinPolicy`. -/
inductive Policy where
  | all
  | any
  | quorum (n : Nat)
  deriving Repr, DecidableEq

/-- `is_join_satisfied`: no tokens never satisfies a join; `All` needs every
incoming edge, `Any` one token, `Quorum n` at least `max n 1` distinct edges. -/
def satisfied (policy : Policy) (incoming : List Nat) : List Nat → Bool
  | [] => false
  | tokens@(_ :: _) =>
    match policy with
    | .all => incoming.all tokens.contains
    | .any => true
    | .quorum n => max n 1 ≤ tokens.length

/-- One key's state: the distinct edges holding a parked token, and whether
the key has fired. -/
structure Key where
  tokens : List Nat := []
  fired : Bool := false
  deriving Repr

/-- `store_token`: add an edge unless it already holds a token. -/
def store (tokens : List Nat) (edge : Nat) : List Nat :=
  if tokens.contains edge then tokens else tokens ++ [edge]

/-- One token arrives on `edge`. Returns the new key state and whether the
node fires now. A fired key drops every later token (`has_fired`); a firing
takes the parked tokens (`take_tokens`). -/
def arrive (policy : Policy) (incoming : List Nat) (key : Key) (edge : Nat) : Key × Bool :=
  if key.fired then (key, false)
  else
    let tokens := store key.tokens edge
    if satisfied policy incoming tokens then ({ tokens := [], fired := true }, true)
    else ({ tokens, fired := false }, false)

/-- The key after a sequence of arrivals. -/
def arriveAll (policy : Policy) (incoming : List Nat) (key : Key) : List Nat → Key
  | [] => key
  | edge :: rest => arriveAll policy incoming (arrive policy incoming key edge).1 rest

/-- How many of a sequence of arrivals fire the node. -/
def fireCount (policy : Policy) (incoming : List Nat) (key : Key) : List Nat → Nat
  | [] => 0
  | edge :: rest =>
    let step := arrive policy incoming key edge
    (if step.2 then 1 else 0) + fireCount policy incoming step.1 rest

/-- The distinct edges of a sequence, in first-arrival order, appended to
`acc`. -/
def collect (acc : List Nat) : List Nat → List Nat
  | [] => acc
  | edge :: rest => collect (store acc edge) rest

end PetriModel.Join
