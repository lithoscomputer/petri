import PetriModel.Retry

/-!
# Retry theorems

* `attempts_le_limit`: a firing never takes more attempts than its limit.
* `retried_only_retryable`: every attempt before the last is one the policy
  retries; so `success_is_final`: a success always ends the firing.
* `accept_partial_keeps_the_failure`: a partial success is recorded only under
  `AcceptPartial`, and keeps the retryable failure it came from (§3.1 rule 3).
* `retries_invisible`: a run and the same run with every firing cut to its last
  attempt, one attempt allowed, look the same to routing and the run context
  (§4). Only the attempt counts and the retries differ.
-/

namespace PetriModel.Flow

theorem limit_pos (r : Retry) : 1 ≤ r.limit :=
  Nat.le_max_right _ _

theorem retries_success (r : Retry) : r.retries .success = false := rfl

/-! ## The attempt loop -/

theorem finalFrom_le (r : Retry) (script : List Outcome) :
    ∀ (fuel n : Nat), n ≤ r.limit → (finalFrom r script n fuel).1 ≤ r.limit
  | 0, _, h => h
  | fuel + 1, n, h => by
    unfold finalFrom
    split
    · rename_i hc
      simp only [Bool.and_eq_true, decide_eq_true_eq] at hc
      exact finalFrom_le r script fuel (n + 1) (by omega)
    · exact h

/-- A firing never takes more attempts than its limit. -/
theorem attempts_le_limit (r : Retry) (script : List Outcome) :
    (attempts r script).1 ≤ r.limit :=
  finalFrom_le r script r.limit 1 (limit_pos r)

theorem finalFrom_retried (r : Retry) (script : List Outcome) :
    ∀ (fuel n m : Nat), n ≤ m → m < (finalFrom r script n fuel).1 →
      r.retries (attemptAt script m) = true
  | 0, n, m, hn, hm => by simp [finalFrom] at hm; omega
  | fuel + 1, n, m, hn, hm => by
    unfold finalFrom at hm
    split at hm
    · rename_i hc
      simp only [Bool.and_eq_true, decide_eq_true_eq] at hc
      by_cases hmn : m = n
      · subst hmn
        exact hc.1
      · exact finalFrom_retried r script fuel (n + 1) m (by omega) hm
    · simp at hm; omega

/-- Every attempt before the last is one the policy retries. -/
theorem retried_only_retryable (r : Retry) (script : List Outcome) {m : Nat} (h₁ : 1 ≤ m)
    (h₂ : m < (attempts r script).1) : r.retries (attemptAt script m) = true :=
  finalFrom_retried r script r.limit 1 m h₁ h₂

/-- A success always ends the firing. -/
theorem success_is_final (r : Retry) (script : List Outcome) {m : Nat} (h₁ : 1 ≤ m)
    (hs : attemptAt script m = .success) : (attempts r script).1 ≤ m := by
  cases Nat.lt_or_ge m (attempts r script).1 with
  | inl hlt =>
    have := retried_only_retryable r script h₁ hlt
    rw [hs, retries_success] at this
    exact absurd this (by decide)
  | inr hge => exact hge

theorem finalFrom_stops (r : Retry) (script : List Outcome) :
    ∀ (fuel n : Nat), r.limit ≤ n + fuel →
      r.retries (finalFrom r script n fuel).2 = true → r.limit ≤ (finalFrom r script n fuel).1
  | 0, n, h, _ => by simpa [finalFrom] using h
  | fuel + 1, n, h, hr => by
    unfold finalFrom at hr ⊢
    split at hr
    · rename_i hc
      simp only [hc, ↓reduceIte]
      exact finalFrom_stops r script fuel (n + 1) (by omega) hr
    · rename_i hc
      simp only [hc, ↓reduceIte, Bool.false_eq_true]
      simp only [Bool.and_eq_true, decide_eq_true_eq, not_and] at hc
      simp only at hr ⊢
      exact Nat.le_of_not_lt (hc hr)

/-- The loop ends early only on an outcome the policy does not retry: a
retryable last attempt means the limit was reached. -/
theorem attempts_exhausted (r : Retry) (script : List Outcome)
    (h : r.retries (attempts r script).2 = true) : r.limit ≤ (attempts r script).1 :=
  finalFrom_stops r script r.limit 1 (by omega) h

/-! ## The record -/

/-- A partial success is recorded only under `AcceptPartial`, and keeps the
retryable failure it came from. -/
theorem accept_partial_keeps_the_failure {r : Retry} {count : Nat} {o u : Outcome}
    (h : recorded r count o = .partialSuccess u) :
    u = o ∧ o ≠ .success ∧ r.acceptPartial = true ∧ r.retries o = true := by
  unfold recorded at h
  split at h
  · rename_i hc
    simp only [Bool.and_eq_true, decide_eq_true_eq] at hc
    simp only [Status.partialSuccess.injEq] at h
    refine ⟨h.symm, ?_, hc.1.1, hc.1.2⟩
    rintro rfl
    have := hc.1.2
    rw [retries_success] at this
    exact absurd this (by decide)
  · cases o <;> simp [Outcome.status] at h

/-! ## Retries are invisible -/

/-- One attempt of a firing's last outcome records what the whole script did. -/
theorem recorded_finalized (r : Retry) (script : List Outcome) :
    recorded { r with maxAttempts := 1 }
        (attempts { r with maxAttempts := 1 } [(attempts r script).2]).1
        (attempts { r with maxAttempts := 1 } [(attempts r script).2]).2 =
      recorded r (attempts r script).1 (attempts r script).2 := by
  have hex := attempts_exhausted r script
  generalize attempts r script = a at hex ⊢
  obtain ⟨k, fo⟩ := a
  have h₁ : attempts { r with maxAttempts := 1 } [fo] = (1, fo) := by
    simp [attempts, finalFrom, Retry.limit, attemptAt]
  have hl : ({ r with maxAttempts := 1 } : Retry).limit = 1 := by simp [Retry.limit]
  have hrr : ({ r with maxAttempts := 1 } : Retry).retries fo = r.retries fo := by
    cases fo <;> rfl
  rw [h₁]
  unfold recorded
  rw [hl, hrr]
  by_cases hret : r.retries fo = true
  · simp [hret, hex hret]
  · simp [hret]

/-- An empty script reports a success on its one attempt. -/
theorem attempts_nil (r : Retry) : attempts r [] = (1, .success) := by
  unfold attempts
  cases r.limit with
  | zero => simp [finalFrom, attemptAt]
  | succ k => simp [finalFrom, attemptAt, retries_success]

theorem script_min (n : SpecNode) (ordinal : Nat) :
    n.script (min ordinal (n.outcomes.length - 1)) = n.script ordinal := by
  simp [SpecNode.script]

/-- Cutting every firing to its last attempt leaves routing's view alone. -/
theorem view_finalized (s : Spec) : s.finalized.view = s.view := by
  have hnodes : s.finalized.nodes.map (fun n : SpecNode =>
      ({ join := n.join, maxFirings := n.maxFirings, groups := n.groups } : Node)) =
      s.nodes.map (fun n => { join := n.join, maxFirings := n.maxFirings, groups := n.groups }) := by
    simp [Spec.finalized, Function.comp_def]
  have hrecord : s.finalized.record = s.record := by
    funext node ordinal
    simp only [Spec.record, Spec.finalized, List.getElem?_map]
    cases hn : s.nodes[node]? with
    | none => rfl
    | some n =>
      simp only [Option.map_some]
      by_cases hlen : n.outcomes.length = 0
      · have hs : n.script ordinal = [] := by
          simp [SpecNode.script, List.length_eq_zero_iff.mp hlen]
        have hs₁ : ({ n with
            retry := { n.retry with maxAttempts := 1 }
            outcomes := (List.range n.outcomes.length).map fun o =>
              [(attempts n.retry (n.script o)).2] } : SpecNode).script ordinal = [] := by
          simp [SpecNode.script, hlen]
        rw [hs₁, hs, attempts_nil, attempts_nil]
        simp [recorded, Retry.retries]
      · have hidx : min ordinal (n.outcomes.length - 1) < n.outcomes.length := by omega
        have hs : ({ n with
            retry := { n.retry with maxAttempts := 1 }
            outcomes := (List.range n.outcomes.length).map fun o =>
              [(attempts n.retry (n.script o)).2] } : SpecNode).script ordinal =
            [(attempts n.retry (n.script ordinal)).2] := by
          simp only [SpecNode.script, List.length_map, List.length_range]
          rw [List.getElem?_map, List.getElem?_range hidx]
          simp [Nat.min_assoc]
        rw [hs]
        exact recorded_finalized n.retry (n.script ordinal)
  simp only [Spec.view, hnodes, hrecord]
  rfl

/-- Retries are invisible outside the log: a run and the same run with every
firing cut to its last attempt look the same to routing and the run
context. -/
theorem retries_invisible (s : Spec) : s.finalized.run.routing = s.run.routing := by
  have hv := view_finalized s
  have hr : s.finalized.record = s.record := congrArg Case.record hv
  simp only [Spec.run, Observed.routing, hv, hr, List.map_map, Function.comp_def]

end PetriModel.Flow
