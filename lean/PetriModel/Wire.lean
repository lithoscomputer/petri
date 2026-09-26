import Lean.Data.Json.Parser
import Lean.Data.Json.Printer
import Lean.Data.Json.FromToJson.Basic
import PetriModel.Flow
import PetriModel.Pick

/-!
# Wire

The line protocol `crates/core/engine/tests/lean_model.rs` speaks: one JSON
query per line in, one JSON answer per line out.

* `{"flow": <FlowCase>}` answers the `Observed` of `Flow.run`.
* `{"pick": {"proposal": …, "draw": …}}` answers `{"ok": <edge or null>}` or
  `{"refused": "<reason>"}`. A rank travels as its `f64` bit pattern, since
  JSON cannot carry NaN or the infinities.

A query the model cannot read answers `{"error": "<message>"}`.
-/

namespace PetriModel.Wire

open Lean (Json toJson)

def field (j : Json) (key : String) : Except String Json :=
  j.getObjVal? key

def list (j : Json) (f : Json → Except String α) : Except String (List α) := do
  (← j.getArr?).toList.mapM f

def optional (j : Json) (key : String) (f : Json → Except String α) : Except String (Option α) :=
  match j.getObjValD key with
  | .null => pure none
  | value => some <$> f value

/-! ## Flow cases -/

def join (j : Json) : Except String Join.Policy := do
  match ← (← field j "kind").getStr? with
  | "all" => pure .all
  | "any" => pure .any
  | "quorum" => return .quorum (← (← field j "n").getNat?)
  | other => throw s!"unknown join kind `{other}`"

def guard (j : Json) : Except String Flow.Guard := do
  match ← j.getStr? with
  | "always" => pure .always
  | "success" => pure .success
  | "failure" => pure .failure
  | other => throw s!"unknown guard `{other}`"

def arm (j : Json) : Except String Flow.Arm := do
  return {
    to := ← (← field j "to").getNat?
    guard := ← guard (← field j "guard")
    back := ← (← field j "back").getBool?
    edge := ← (← field j "edge").getNat? }

def node (j : Json) : Except String Flow.Node := do
  return {
    join := ← join (← field j "join")
    maxFirings := ← (← field j "max_firings").getNat?
    outcomes := ← list (← field j "outcomes") (·.getBool?)
    groups := ← list (← field j "groups") (list · arm) }

def flowCase (j : Json) : Except String Flow.Case := do
  return {
    nodes := ← list (← field j "nodes") node
    schedule := ← list (← field j "schedule") (·.getNat?) }

def nats (ns : List Nat) : Json :=
  .arr (ns.map toJson).toArray

def observed (o : Flow.Observed) : Json :=
  let key := fun ((n, g) : Nat × Nat) => nats [n, g]
  Json.mkObj [
    ("steps", .arr (o.steps.map fun step => .arr (step.map key).toArray).toArray),
    ("finished", .arr (o.finished.map key).toArray),
    ("parked", .arr (o.parked.map fun (n, g, e) => nats [n, g, e]).toArray),
    ("budget_exceeded", nats o.budgetExceeded),
    ("status", .str o.status)]

/-! ## Picks -/

/-- `PickPolicy`'s serde names. -/
def policy (j : Json) : Except String Pick.Policy := do
  match ← j.getStr? with
  | "First" => pure .first
  | "HighestWeightThenLexical" => pure .highestWeightThenLexical
  | "WeightedRandom" => pure .weightedRandom
  | "LowestRankThenArmOrder" => pure .lowestRankThenArmOrder
  | other => throw s!"unknown pick policy `{other}`"

def candidate (j : Json) : Except String Pick.Candidate := do
  return {
    edge := ← (← field j "edge").getNat?
    weight := ← (← field j "weight").getNat?
    target := ← (← field j "target").getStr?
    rank := ← optional j "rank" fun r => UInt64.ofNat <$> r.getNat? }

def proposal (j : Json) : Except String Pick.Proposal := do
  return {
    tier := ← optional j "tier" (·.getNat?)
    pick := ← optional j "pick" policy
    candidates := ← list (← field j "candidates") candidate }

def draw (j : Json) : Except String Pick.Draw := do
  return {
    tier := ← (← field j "tier").getNat?
    candidates := ← list (← field j "candidates") (·.getNat?)
    roll := ← (← field j "roll").getNat?
    total := ← (← field j "total").getNat? }

def refusal : Pick.Refusal → String
  | .drawOnEmpty => "draw_on_empty"
  | .drawOnUnweighted => "draw_on_unweighted"
  | .missingDraw => "missing_draw"
  | .wrongTier => "wrong_tier"
  | .candidatesDiffer => "candidates_differ"
  | .invalidTotal => "invalid_total"
  | .noSelection => "no_selection"

def picked : Except Pick.Refusal (Option Nat) → Json
  | .ok edge => Json.mkObj [("ok", match edge with | some e => toJson e | none => .null)]
  | .error reason => Json.mkObj [("refused", .str (refusal reason))]

/-! ## Queries -/

def answer (line : String) : Except String Json := do
  let query ← Json.parse line
  match query.getObjValD "flow", query.getObjValD "pick" with
  | .null, .null => throw "expected a `flow` or `pick` query"
  | .null, q => return picked (Pick.pick (← proposal (← field q "proposal")) (← optional q "draw" draw))
  | q, _ => return observed (Flow.run (← flowCase q))

def respond (line : String) : String :=
  match answer line with
  | .ok json => json.compress
  | .error message => (Json.mkObj [("error", .str message)]).compress

end PetriModel.Wire
