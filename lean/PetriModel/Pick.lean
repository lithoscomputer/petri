/-!
# Deterministic pick

`deterministic_pick` in `crates/core/engine/src/apply.rs`: the one definition
of the selection tie-breaks, shared by the core's validator and the default
host resolver.

Ranks are `f64` in Rust and are compared with `f64::total_cmp`. The model
carries a rank as its IEEE 754 bit pattern and compares `rankKey`, the
unsigned key `total_cmp` orders by, so NaN, infinities and signed zeros
behave exactly as they do in Rust.
-/

namespace PetriModel.Pick

/-- `PickPolicy`. -/
inductive Policy where
  | first
  | highestWeightThenLexical
  | weightedRandom
  | lowestRankThenArmOrder
  deriving Repr, DecidableEq

/-- `RoutingCandidate`, without the fields the pick never reads. -/
structure Candidate where
  edge : Nat
  weight : Nat
  target : String
  /-- The `f64` rank's bit pattern. -/
  rank : Option UInt64
  deriving Repr

/-- `RoutingProposal`, without the group index the pick never reads. -/
structure Proposal where
  tier : Option Nat
  pick : Option Policy
  candidates : List Candidate
  deriving Repr

/-- `WeightedDraw`. -/
structure Draw where
  tier : Nat
  candidates : List Nat
  roll : Nat
  total : Nat
  deriving Repr

/-- Why a pick is refused, one case per `Err` in the Rust function. -/
inductive Refusal where
  | drawOnEmpty
  | drawOnUnweighted
  | missingDraw
  | wrongTier
  | candidatesDiffer
  | invalidTotal
  | noSelection
  deriving Repr, DecidableEq

/-- The key `f64::total_cmp` orders by: negative bit patterns flip every bit,
the others set the sign bit, and the results compare as unsigned numbers. -/
def rankKey (bits : UInt64) : Nat :=
  if 2 ^ 63 ≤ bits.toNat then (~~~bits).toNat else bits.toNat + 2 ^ 63

/-- `HighestWeightThenLexical`: a later candidate wins on a higher weight, or
on an equal weight and a lexically smaller target name. -/
def highest (winner : Candidate) : List Candidate → Candidate
  | [] => winner
  | c :: rest =>
    highest
      (if c.weight > winner.weight || (c.weight == winner.weight && decide (c.target < winner.target))
       then c else winner)
      rest

/-- `f64::INFINITY`'s bit pattern: what Rust compares a winner without a rank
against. A winner always has a rank, so the fallback never decides. -/
def infinityBits : UInt64 := 0x7FF0000000000000

/-- A candidate's rank key, with Rust's `unwrap_or(f64::INFINITY)`. -/
def Candidate.key (c : Candidate) : Nat := rankKey (c.rank.getD infinityBits)

/-- Whether a ranked candidate replaces the current winner: always when
there is none, otherwise on a strictly smaller key. -/
def replaces (rank : UInt64) : Option Candidate → Bool
  | none => true
  | some w => decide (rankKey rank < w.key)

/-- `LowestRankThenArmOrder`: the first candidate with the smallest rank key;
unranked candidates never win. -/
def lowest (winner : Option Candidate) : List Candidate → Option Candidate
  | [] => winner
  | c :: rest =>
    match c.rank with
    | none => lowest winner rest
    | some r => lowest (if replaces r winner then some c else winner) rest

/-- `WeightedRandom`: walk the weights, subtracting each one the roll passes.
The position of the candidate the roll lands in. -/
def select : List Nat → Nat → Option Nat
  | [], _ => none
  | w :: ws, roll => if roll < w then some 0 else (select ws (roll - w)).map (· + 1)

def pick (proposal : Proposal) (draw : Option Draw) : Except Refusal (Option Nat) :=
  match proposal.candidates with
  | [] => if draw.isSome then .error .drawOnEmpty else .ok none
  | first :: rest =>
    let policy := proposal.pick.getD .first
    if policy != .weightedRandom && draw.isSome then .error .drawOnUnweighted
    else
      match policy with
      | .first => .ok (some first.edge)
      | .highestWeightThenLexical => .ok (some (highest first rest).edge)
      | .lowestRankThenArmOrder => .ok ((lowest none (first :: rest)).map (·.edge))
      | .weightedRandom =>
        match draw with
        | none => .error .missingDraw
        | some d =>
          let all := first :: rest
          let weights := all.map (·.weight)
          if some d.tier != proposal.tier then .error .wrongTier
          else if d.candidates != all.map (·.edge) then .error .candidatesDiffer
          else if weights.sum == 0 || d.total != weights.sum || d.roll ≥ weights.sum then
            .error .invalidTotal
          else
            match (select weights d.roll).bind (all[·]?) with
            | some c => .ok (some c.edge)
            | none => .error .noSelection

end PetriModel.Pick
