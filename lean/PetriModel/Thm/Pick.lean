import PetriModel.Pick

/-!
# Pick theorems

* `select_count`: a weighted draw is proportional. Of the rolls
  `0 ≤ roll < total`, exactly `weight i` land on candidate `i`.
* `select_weight_pos`: so a zero-weight candidate is never picked.
* `pick_weighted_ok`: a draw that matches its proposal is never refused.
  The Rust function's last `Err` ("the weighted draw did not select a
  candidate") is unreachable.
* `pick_mem`: whatever the policy, a pick names one of the candidates.
* `highest_max`: `HighestWeightThenLexical` picks a heaviest candidate.
* `lowest_min`, `lowest_eq_none`: `LowestRankThenArmOrder` picks a candidate
  with the smallest rank key, and picks nothing only when no candidate has a
  rank.
-/

namespace PetriModel.Pick

/-! ## Weighted draws -/

theorem select_lt_length {ws : List Nat} {roll i : Nat} (h : select ws roll = some i) :
    i < ws.length := by
  induction ws generalizing roll i with
  | nil => simp [select] at h
  | cons w ws ih =>
    simp only [select] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      simp
    · cases hs : select ws (roll - w) with
      | none => simp [hs] at h
      | some j =>
        simp only [hs, Option.map_some, Option.some.injEq] at h
        have := ih hs
        simp only [List.length_cons]
        omega

theorem select_weight_pos {ws : List Nat} {roll i : Nat} (h : select ws roll = some i) :
    ∃ w, ws[i]? = some w ∧ 0 < w := by
  induction ws generalizing roll i with
  | nil => simp [select] at h
  | cons w ws ih =>
    simp only [select] at h
    split at h
    · simp only [Option.some.injEq] at h
      subst h
      exact ⟨w, rfl, by omega⟩
    · cases hs : select ws (roll - w) with
      | none => simp [hs] at h
      | some j =>
        simp only [hs, Option.map_some, Option.some.injEq] at h
        subst h
        simpa using ih hs

theorem select_isSome {ws : List Nat} {roll : Nat} (h : roll < ws.sum) :
    (select ws roll).isSome := by
  induction ws generalizing roll with
  | nil => simp at h
  | cons w ws ih =>
    simp only [select]
    split
    · rfl
    · simp only [List.sum_cons] at h
      simpa using ih (show roll - w < ws.sum by omega)

/-- A weighted draw is proportional: exactly `ws[i]` of the rolls below the
total land on position `i`. -/
theorem select_count (ws : List Nat) (i : Nat) :
    (List.range ws.sum).countP (fun roll => decide (select ws roll = some i)) = ws[i]?.getD 0 := by
  induction ws generalizing i with
  | nil => simp
  | cons w ws ih =>
    rw [List.sum_cons, List.range_add, List.countP_append, List.countP_map]
    have low : (List.range w).countP (fun roll => decide (select (w :: ws) roll = some i)) =
        if i = 0 then w else 0 := by
      split
      · subst_vars
        rw [List.countP_eq_length.mpr]
        · simp
        · intro roll hr
          simp only [List.mem_range] at hr
          simp [select, hr]
      · rw [List.countP_eq_zero.mpr]
        intro roll hr
        simp only [List.mem_range] at hr
        simp only [select, hr, ↓reduceIte, Option.some.injEq, decide_eq_true_eq]
        omega
    rw [low]
    cases i with
    | zero =>
      simp only [↓reduceIte, List.getElem?_cons_zero, Option.getD_some]
      suffices h : (List.range ws.sum).countP
          ((fun roll => decide (select (w :: ws) roll = some 0)) ∘ (w + ·)) = 0 by omega
      rw [List.countP_eq_zero]
      intro x _
      simp only [Function.comp, select, show ¬ w + x < w by omega, ↓reduceIte,
        Nat.add_sub_cancel_left, decide_eq_true_eq]
      intro h
      cases hs : select ws x <;> simp [hs] at h
    | succ j =>
      simp only [Nat.succ_ne_zero, ↓reduceIte, Nat.zero_add, List.getElem?_cons_succ]
      rw [← ih j]
      congr 1
      funext x
      simp only [Function.comp, select, show ¬ w + x < w by omega, ↓reduceIte,
        Nat.add_sub_cancel_left]
      cases select ws x <;> simp

/-! ## The pick -/

/-- A draw that matches its proposal is never refused, and it picks a
candidate with a positive weight. -/
theorem pick_weighted_ok {proposal : Proposal} {first : Candidate} {rest : List Candidate}
    {d : Draw} (hpick : proposal.pick = some .weightedRandom)
    (hcands : proposal.candidates = first :: rest) (htier : proposal.tier = some d.tier)
    (hedges : d.candidates = (first :: rest).map (·.edge))
    (htotal : d.total = ((first :: rest).map (·.weight)).sum) (hroll : d.roll < d.total) :
    ∃ c ∈ first :: rest, 0 < c.weight ∧ pick proposal (some d) = .ok (some c.edge) := by
  have hsome := select_isSome (ws := (first :: rest).map (·.weight)) (htotal ▸ hroll)
  obtain ⟨i, hi⟩ := Option.isSome_iff_exists.mp hsome
  obtain ⟨w, hw, hpos⟩ := select_weight_pos hi
  have hlen : i < (first :: rest).length := by
    simpa using select_lt_length hi
  let c := (first :: rest)[i]
  have hc : (first :: rest)[i]? = some c := List.getElem?_eq_getElem hlen
  have hcw : c.weight = w := by
    rw [List.getElem?_map, hc] at hw
    simpa using hw
  refine ⟨c, List.getElem_mem hlen, hcw ▸ hpos, ?_⟩
  have hne : ((first :: rest).map (·.weight)).sum ≠ 0 := by omega
  have hlt : d.roll < ((first :: rest).map (·.weight)).sum := htotal ▸ hroll
  unfold pick
  simp only [hcands]
  generalize hws : (first :: rest).map (·.weight) = ws at *
  simp [hpick, htier, hedges, htotal, hi, hc, hne, Nat.not_le.mpr hlt]

theorem highest_mem (winner : Candidate) (rest : List Candidate) :
    highest winner rest ∈ winner :: rest := by
  induction rest generalizing winner with
  | nil => simp [highest]
  | cons c rest ih =>
    simp only [highest]
    split
    · exact List.mem_cons_of_mem _ (ih c)
    · have := ih winner
      simp only [List.mem_cons] at this ⊢
      rcases this with h | h
      · exact Or.inl h
      · exact Or.inr (Or.inr h)

/-- `HighestWeightThenLexical` picks a heaviest candidate. -/
theorem highest_max (winner : Candidate) (rest : List Candidate) :
    ∀ c ∈ winner :: rest, c.weight ≤ (highest winner rest).weight := by
  induction rest generalizing winner with
  | nil => simp [highest]
  | cons c rest ih =>
    intro x hx
    simp only [highest]
    split <;> rename_i h
    · have hge : winner.weight ≤ c.weight := by
        simp only [Bool.or_eq_true, decide_eq_true_eq, Bool.and_eq_true, beq_iff_eq] at h
        omega
      simp only [List.mem_cons] at hx
      rcases hx with hx | hx | hx
      · rw [hx]; exact Nat.le_trans hge (ih c c List.mem_cons_self)
      · rw [hx]; exact ih c c List.mem_cons_self
      · exact ih c x (List.mem_cons_of_mem _ hx)
    · have hle : c.weight ≤ winner.weight := by
        simp only [Bool.or_eq_true, decide_eq_true_eq, Bool.and_eq_true, beq_iff_eq,
          not_or] at h
        omega
      simp only [List.mem_cons] at hx
      rcases hx with hx | hx | hx
      · rw [hx]; exact ih winner winner List.mem_cons_self
      · rw [hx]; exact Nat.le_trans hle (ih winner winner List.mem_cons_self)
      · exact ih winner x (List.mem_cons_of_mem _ hx)

theorem lowest_mem {winner : Option Candidate} {cs : List Candidate} {w : Candidate}
    (h : lowest winner cs = some w) : winner = some w ∨ w ∈ cs := by
  induction cs generalizing winner with
  | nil => exact Or.inl h
  | cons c rest ih =>
    simp only [lowest] at h
    split at h
    · rcases ih h with h | h
      · exact Or.inl h
      · exact Or.inr (List.mem_cons_of_mem _ h)
    · rcases ih h with h' | h'
      · split at h'
        · simp only [Option.some.injEq] at h'
          exact Or.inr (h' ▸ List.mem_cons_self)
        · exact Or.inl h'
      · exact Or.inr (List.mem_cons_of_mem _ h')

/-- `LowestRankThenArmOrder` picks a candidate whose rank key is no larger
than any ranked candidate's. -/
theorem lowest_min {winner : Option Candidate} {cs : List Candidate} {w : Candidate}
    (h : lowest winner cs = some w) :
    (∀ v, winner = some v → w.key ≤ v.key) ∧ ∀ c ∈ cs, c.rank.isSome → w.key ≤ c.key := by
  induction cs generalizing winner with
  | nil =>
    simp only [lowest] at h
    subst h
    exact ⟨fun v hv => by simp only [Option.some.injEq] at hv; subst hv; exact Nat.le_refl _,
      by simp⟩
  | cons c rest ih =>
    simp only [lowest] at h
    split at h <;> rename_i hr
    · obtain ⟨h₁, h₂⟩ := ih h
      refine ⟨h₁, fun x hx hs => ?_⟩
      simp only [List.mem_cons] at hx
      rcases hx with hx | hx
      · subst hx
        simp [hr] at hs
      · exact h₂ x hx hs
    · rename_i r
      have hkey : c.key = rankKey r := by simp [Candidate.key, hr]
      split at h <;> rename_i hrep
      · obtain ⟨h₁, h₂⟩ := ih h
        have hc := h₁ c rfl
        refine ⟨fun v hv => ?_, fun x hx hs => ?_⟩
        · subst hv
          simp only [replaces, decide_eq_true_eq] at hrep
          omega
        · simp only [List.mem_cons] at hx
          rcases hx with hx | hx
          · rw [hx]; exact hc
          · exact h₂ x hx hs
      · obtain ⟨h₁, h₂⟩ := ih h
        refine ⟨h₁, fun x hx hs => ?_⟩
        simp only [List.mem_cons] at hx
        rcases hx with hx | hx
        · rw [hx]
          cases winner with
          | none => simp [replaces] at hrep
          | some v =>
            simp only [replaces, decide_eq_true_eq] at hrep
            have := h₁ v rfl
            omega
        · exact h₂ x hx hs

/-- `LowestRankThenArmOrder` picks nothing only when no candidate has a rank. -/
theorem lowest_eq_none {cs : List Candidate} :
    lowest none cs = none ↔ ∀ c ∈ cs, c.rank = none := by
  suffices h : ∀ winner, lowest winner cs = none ↔ winner = none ∧ ∀ c ∈ cs, c.rank = none by
    simpa using h none
  induction cs with
  | nil => simp [lowest]
  | cons c rest ih =>
    intro winner
    simp only [lowest]
    split <;> rename_i hr
    · simp [ih, hr]
    · rw [ih]
      constructor
      · rintro ⟨h₁, _⟩
        split at h₁
        · simp at h₁
        · rename_i hrep
          subst h₁
          simp [replaces] at hrep
      · rintro ⟨_, h₂⟩
        have := h₂ c List.mem_cons_self
        rw [hr] at this
        cases this

/-- Whatever the policy, a pick names one of the candidates. -/
theorem pick_mem {proposal : Proposal} {draw : Option Draw} {edge : Nat}
    (h : pick proposal draw = .ok (some edge)) : ∃ c ∈ proposal.candidates, c.edge = edge := by
  unfold pick at h
  split at h
  · split at h <;> simp at h
  · rename_i first rest hcands
    rw [hcands]
    generalize proposal.pick.getD Policy.first = policy at h
    cases policy with
    | first =>
      cases draw <;> simp at h
      exact ⟨first, List.mem_cons_self, h⟩
    | highestWeightThenLexical =>
      cases draw <;> simp at h
      exact ⟨_, highest_mem first rest, h⟩
    | lowestRankThenArmOrder =>
      cases draw <;> simp only [Option.isSome_none, Option.isSome_some, Bool.and_false,
        Bool.and_true, Bool.false_eq_true, ↓reduceIte] at h
      · simp only [Except.ok.injEq, Option.map_eq_some_iff] at h
        obtain ⟨w, hw, rfl⟩ := h
        rcases lowest_mem hw with h | h
        · simp at h
        · exact ⟨w, h, rfl⟩
      · simp at h
    | weightedRandom =>
      cases draw with
      | none => simp at h
      | some d =>
        simp only [bne_self_eq_false, Bool.false_and, Bool.false_eq_true, ↓reduceIte] at h
        split at h
        · simp at h
        · split at h
          · simp at h
          · split at h
            · simp at h
            · split at h
              · rename_i c hc
                simp only [Except.ok.injEq, Option.some.injEq] at h
                obtain ⟨i, _, hi⟩ := Option.bind_eq_some_iff.mp hc
                exact ⟨c, List.mem_of_getElem? hi, h⟩
              · simp at h

end PetriModel.Pick
