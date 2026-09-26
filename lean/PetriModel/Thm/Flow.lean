import PetriModel.Flow

/-!
# Flow theorems

About whole runs of `Flow.run`, loops included:

* `token_generation`: a routed token's generation is its source firing's,
  plus one on a back arm (`engine-spec.md` §4).
* `started_once`: in any run, each `(node, generation)` key starts at most
  once.
* `firings_le_budget`: no node fires more often than its budget.
* `budget_exceeded_fails`: a run in which a budget refused a firing does not
  report success.
* `run_settles`: every run ends with nothing live within the host steps the
  budgets allow, so `run` never reports `unsettled`.
-/

namespace PetriModel.Flow

open Join (arrive)

/-! ## Arrivals -/

theorem arrive_of_fired {policy : Join.Policy} {incoming : List Nat} {key : Join.Key}
    (h : key.fired = true) (edge : Nat) : arrive policy incoming key edge = (key, false) := by
  simp [arrive, h]

/-- A firing arrival finds the key unfired and leaves it fired. -/
theorem arrive_fires {policy : Join.Policy} {incoming : List Nat} {key : Join.Key} {edge : Nat}
    (h : (arrive policy incoming key edge).2 = true) :
    key.fired = false ∧ (arrive policy incoming key edge).1.fired = true := by
  unfold arrive at h ⊢
  split at h <;> rename_i hf
  · simp at h
  · dsimp only at h ⊢
    split at h
    · simp_all
    · simp at h

theorem arrive_keeps_fired {policy : Join.Policy} {incoming : List Nat} {key : Join.Key}
    (h : key.fired = true) (edge : Nat) : (arrive policy incoming key edge).1.fired = true := by
  rw [arrive_of_fired h]
  exact h

/-! ## Keys -/

theorem key_setKey_self (s : State) (k : Key) (v : Join.Key) : (s.setKey k v).key k = v := by
  simp [State.setKey, State.key]

/-- Dropping one key's entries does not change the lookup of another. -/
theorem find?_filter_other (l : List (Key × Join.Key)) {k k' : Key} (h : k' ≠ k) :
    (l.filter (·.1 != k)).find? (·.1 == k') = l.find? (·.1 == k') := by
  induction l with
  | nil => rfl
  | cons x xs ih =>
    by_cases hx : x.1 = k
    · have hkk : (k == k') = false := by simpa using Ne.symm h
      simp [hx, hkk, ih]
    · simp [List.find?_cons, hx, ih]

theorem key_setKey_other (s : State) {k k' : Key} (v : Join.Key) (h : k' ≠ k) :
    (s.setKey k v).key k' = s.key k' := by
  have hk : ((k, v).1 == k') = false := by simpa using Ne.symm h
  simp only [State.setKey, State.key, List.find?_cons, hk, find?_filter_other _ h]

/-- The key a delivery sets, seen from any key: a key that was fired stays
fired. -/
theorem setKey_arrive_fired {s : State} {k k' : Key} {policy : Join.Policy} {incoming : List Nat}
    {edge : Nat} (h : (s.key k').fired = true) :
    ((s.setKey k (arrive policy incoming (s.key k) edge).1).key k').fired = true := by
  by_cases hk : k' = k
  · subst hk
    rw [key_setKey_self]
    exact arrive_keeps_fired h edge
  · rw [key_setKey_other _ _ hk]
    exact h

/-- And a key unfired after the delivery was unfired before it. -/
theorem setKey_arrive_unfired {s : State} {k k' : Key} {policy : Join.Policy}
    {incoming : List Nat} {edge : Nat}
    (h : ((s.setKey k (arrive policy incoming (s.key k) edge).1).key k').fired = false) :
    (s.key k').fired = false := by
  cases hf : (s.key k').fired
  · rfl
  · have := setKey_arrive_fired (policy := policy) (incoming := incoming) (edge := edge)
      (k := k) hf
    rw [h] at this
    exact absurd this (by decide)

/-! ## Deliveries -/

/-- A delivery never unfires a key. -/
theorem deliver_fired_mono (c : Case) :
    ∀ (ts : List Token) (s : State) (k : Key),
      (s.key k).fired = true → ((deliver c s ts).1.key k).fired = true
  | [], _, _, h => h
  | t :: rest, s, k, h => by
    unfold deliver
    split
    · exact deliver_fired_mono c rest s k h
    · rename_i node _
      have h' := setKey_arrive_fired (policy := node.join) (incoming := incoming c t.target)
        (edge := t.edge) (k := (t.target, t.generation)) h
      dsimp only
      split
      · exact deliver_fired_mono c rest _ k h'
      · split
        · exact deliver_fired_mono c rest _ k h'
        · exact deliver_fired_mono c rest _ k h'

/-- A delivery leaves the recorded steps alone. -/
theorem deliver_steps (c : Case) :
    ∀ (ts : List Token) (s : State), (deliver c s ts).1.steps = s.steps
  | [], _ => rfl
  | t :: rest, s => by
    unfold deliver
    split
    · exact deliver_steps c rest s
    · dsimp only
      split
      · exact deliver_steps c rest _
      · split
        · exact deliver_steps c rest _
        · exact deliver_steps c rest _

/-- The keys a delivery starts are distinct, were unfired before it, and are
fired after it. -/
theorem deliver_started (c : Case) :
    ∀ (ts : List Token) (s : State),
      ((deliver c s ts).2.map (·.1)).Nodup ∧
        ∀ k ∈ (deliver c s ts).2.map (·.1),
          (s.key k).fired = false ∧ ((deliver c s ts).1.key k).fired = true
  | [], _ => by simp [deliver]
  | t :: rest, s => by
    unfold deliver
    split
    · exact deliver_started c rest s
    · rename_i node _
      dsimp only
      split
      · obtain ⟨hnd, hks⟩ := deliver_started c rest _
        refine ⟨hnd, fun k hk => ?_⟩
        obtain ⟨hu, hf⟩ := hks k hk
        exact ⟨setKey_arrive_unfired hu, hf⟩
      · split
        · obtain ⟨hnd, hks⟩ := deliver_started c rest _
          refine ⟨hnd, fun k hk => ?_⟩
          obtain ⟨hu, hf⟩ := hks k hk
          exact ⟨setKey_arrive_unfired hu, hf⟩
        · rename_i hfired _
          have hstep := arrive_fires (policy := node.join) (incoming := incoming c t.target)
            (key := s.key (t.target, t.generation)) (edge := t.edge) (by simpa using hfired)
          obtain ⟨hnd, hks⟩ := deliver_started c rest _
          simp only [List.map_cons, List.nodup_cons, List.mem_cons]
          refine ⟨⟨fun hmem => ?_, hnd⟩, fun k hk => ?_⟩
          · obtain ⟨hu, _⟩ := hks _ hmem
            have hu' : ((s.setKey (t.target, t.generation)
                (arrive node.join (incoming c t.target) (s.key (t.target, t.generation))
                  t.edge).1).key (t.target, t.generation)).fired = false := hu
            rw [key_setKey_self, hstep.2] at hu'
            exact absurd hu' (by decide)
          · rcases hk with rfl | hk
            · refine ⟨hstep.1, deliver_fired_mono c rest _ _ ?_⟩
              show ((s.setKey (t.target, t.generation)
                (arrive node.join (incoming c t.target) (s.key (t.target, t.generation))
                  t.edge).1).key (t.target, t.generation)).fired = true
              rw [key_setKey_self]
              exact hstep.2
            · obtain ⟨hu, hf⟩ := hks k hk
              exact ⟨setKey_arrive_unfired hu, hf⟩

/-! ## Runs: each key starts at most once -/

/-- Every key a run has started is recorded once, and is fired. -/
def StartedOnce (s : State) : Prop :=
  s.steps.flatten.Nodup ∧ ∀ k ∈ s.steps.flatten, (s.key k).fired = true

theorem sorted_keys_perm (started : List (Key × Outcome)) :
    ((started.mergeSort keyLe).map (·.1)).Perm (started.map (·.1)) :=
  (List.mergeSort_perm started keyLe).map _

theorem startedOnce_start (c : Case) : StartedOnce (start c) := by
  obtain ⟨hnd, hks⟩ := deliver_started c ((seeds c).map fun (node, edge) => ⟨node, 0, edge⟩)
    { keys := [], firings := List.replicate c.nodes.length 0, live := []
      steps := [], finished := [], budgetExceeded := [] }
  refine ⟨?_, fun k hk => ?_⟩
  · simp only [start, List.flatten_cons, List.flatten_nil, List.append_nil]
    exact (sorted_keys_perm _).nodup_iff.mpr hnd
  · simp only [start, List.flatten_cons, List.flatten_nil, List.append_nil] at hk
    exact (hks k ((sorted_keys_perm _).mem_iff.mp hk)).2

theorem startedOnce_finish (c : Case) {s : State} (h : StartedOnce s) (firing : Key × Outcome) :
    StartedOnce (finish c s firing) := by
  obtain ⟨hnd, hfired⟩ := h
  let s₁ : State := { s with live := s.live.erase firing, finished := s.finished ++ [firing] }
  obtain ⟨hnew, hks⟩ := deliver_started c (tokensOf c firing.1 firing.2) s₁
  have hperm := sorted_keys_perm (deliver c s₁ (tokensOf c firing.1 firing.2)).2
  refine ⟨?_, fun k hk => ?_⟩
  · simp only [finish, deliver_steps, List.flatten_append, List.flatten_cons, List.flatten_nil,
      List.append_nil]
    rw [List.nodup_append]
    refine ⟨hnd, hperm.nodup_iff.mpr hnew, fun a ha b hb hab => ?_⟩
    subst hab
    have hu := (hks a (hperm.mem_iff.mp hb)).1
    have hf : (s₁.key a).fired = true := hfired a ha
    rw [hf] at hu
    exact absurd hu (by decide)
  · simp only [finish, deliver_steps, List.flatten_append, List.flatten_cons,
      List.flatten_nil, List.append_nil, List.mem_append] at hk
    rcases hk with hk | hk
    · exact deliver_fired_mono c _ s₁ k (hfired k hk)
    · exact (hks k (hperm.mem_iff.mp hk)).2

theorem startedOnce_loop (c : Case) :
    ∀ (fuel k : Nat) (s : State), StartedOnce s → StartedOnce (loop c fuel k s)
  | 0, _, _, h => h
  | fuel + 1, k, s, h => by
    unfold loop
    split
    · exact h
    · exact startedOnce_loop c fuel (k + 1) _ (startedOnce_finish c h _)

/-- In any run, each `(node, generation)` key starts at most once. -/
theorem started_once (c : Case) : (run c).steps.flatten.Nodup := by
  simp only [run]
  exact (startedOnce_loop c _ 0 (start c) (startedOnce_start c)).1

/-! ## Runs: budgets -/

/-- No node has fired more often than its budget. -/
def WithinBudget (c : Case) (s : State) : Prop :=
  ∀ n node, c.nodes[n]? = some node → s.firingsOf n ≤ node.maxFirings

theorem deliver_within_budget (c : Case) :
    ∀ (ts : List Token) (s : State), WithinBudget c s → WithinBudget c (deliver c s ts).1
  | [], _, h => h
  | t :: rest, s, h => by
    unfold deliver
    split
    · exact deliver_within_budget c rest s h
    · rename_i node hnode
      dsimp only
      split
      · exact deliver_within_budget c rest _ h
      · split
        · exact deliver_within_budget c rest _ h
        · rename_i hroom
          apply deliver_within_budget c rest
          intro n node' hn
          simp only [State.firingsOf, State.setKey, State.bump] at hroom ⊢
          rw [List.getElem?_set]
          split
          · rename_i heq
            subst heq
            rw [hnode] at hn
            cases hn
            split <;> simp <;> omega
          · exact h n node' hn

theorem within_budget_start (c : Case) : WithinBudget c (start c) := by
  apply deliver_within_budget
  intro n node _
  simp only [State.firingsOf, List.getElem?_replicate]
  split <;> simp

theorem within_budget_finish (c : Case) {s : State} (h : WithinBudget c s)
    (firing : Key × Outcome) : WithinBudget c (finish c s firing) :=
  deliver_within_budget c _ _ h

theorem within_budget_loop (c : Case) :
    ∀ (fuel k : Nat) (s : State), WithinBudget c s → WithinBudget c (loop c fuel k s)
  | 0, _, _, h => h
  | fuel + 1, k, s, h => by
    unfold loop
    split
    · exact h
    · exact within_budget_loop c fuel (k + 1) _ (within_budget_finish c h _)

/-- No node fires more often than its budget. -/
theorem firings_le_budget (c : Case) (fuel : Nat) :
    WithinBudget c (loop c fuel 0 (start c)) :=
  within_budget_loop c fuel 0 _ (within_budget_start c)

/-- A run in which a budget refused a firing does not report success. -/
theorem budget_exceeded_fails (c : Case) (h : (run c).budgetExceeded ≠ []) :
    (run c).status ≠ "success" := by
  simp only [run] at h ⊢
  split
  · decide
  · split
    · decide
    · rename_i h₁ h₂
      simp_all

/-! ## Tokens -/

/-- A routed token comes from an arm of the firing's node, and its generation
is the firing's, plus one on a back arm. -/
theorem token_generation (c : Case) (k : Key) (outcome : Outcome) :
    ∀ t ∈ tokensOf c k outcome, ∃ (node : Node) (group : List Arm) (arm : Arm),
      c.nodes[k.1]? = some node ∧ group ∈ node.groups ∧ arm ∈ group ∧
        t = ⟨arm.to, if arm.back then k.2 + 1 else k.2, arm.edge⟩ := by
  intro t ht
  unfold tokensOf at ht
  cases hn : c.nodes[k.1]? with
  | none => simp [hn] at ht
  | some node =>
    simp only [hn, Option.map_some, Option.getD_some, List.mem_map, List.mem_filterMap] at ht
    obtain ⟨arm, ⟨group, hgroup, hemit⟩, rfl⟩ := ht
    exact ⟨node, group, arm, rfl, hgroup, List.mem_of_find?_eq_some hemit, rfl⟩

/-! ## Runs: every run ends -/

/-- Raising one entry of a list by one raises its sum by one. -/
theorem sum_set_succ : ∀ (l : List Nat) (i : Nat), i < l.length →
    (l.set i (l[i]?.getD 0 + 1)).sum = l.sum + 1
  | [], _, h => absurd h (by simp)
  | x :: xs, 0, _ => by simp; omega
  | x :: xs, i + 1, h => by
    simp only [List.set_cons_succ, List.sum_cons, List.getElem?_cons_succ]
    rw [sum_set_succ xs i (by simpa using h)]
    omega

/-- A list bounded entry by entry by another of the same length has no larger
sum. -/
theorem sum_le_of_le : ∀ (l m : List Nat), l.length = m.length →
    (∀ i (hl : i < l.length) (hm : i < m.length), l[i] ≤ m[i]) → l.sum ≤ m.sum
  | [], [], _, _ => by simp
  | [], _ :: _, h, _ => by simp at h
  | _ :: _, [], h, _ => by simp at h
  | x :: xs, y :: ys, h, hle => by
    simp only [List.sum_cons]
    have hx := hle 0 (by simp) (by simp)
    have := sum_le_of_le xs ys (by simpa using h)
      (fun i hl hm => hle (i + 1) (by simpa using hl) (by simpa using hm))
    simp at hx
    omega

/-- A delivery leaves the live and finished firings alone, keeps the firing
counts' length, and adds one firing per start. -/
theorem deliver_counts (c : Case) :
    ∀ (ts : List Token) (s : State), s.firings.length = c.nodes.length →
      (deliver c s ts).1.live = s.live ∧ (deliver c s ts).1.finished = s.finished ∧
        (deliver c s ts).1.firings.length = c.nodes.length ∧
          (deliver c s ts).1.firings.sum = s.firings.sum + (deliver c s ts).2.length
  | [], _, h => by simp [deliver, h]
  | t :: rest, s, h => by
    unfold deliver
    split
    · exact deliver_counts c rest s h
    · rename_i node hnode
      dsimp only
      split
      · exact deliver_counts c rest _ h
      · split
        · exact deliver_counts c rest _ h
        · have hlt : t.target < s.firings.length := by
            rw [h]; exact (List.getElem?_eq_some_iff.mp hnode).1
          obtain ⟨hl, hf, hlen, hsum⟩ := deliver_counts c rest
            ((s.setKey (t.target, t.generation)
              (arrive node.join (incoming c t.target) (s.key (t.target, t.generation))
                t.edge).1).bump t.target)
            (by simpa [State.bump, State.setKey] using h)
          refine ⟨hl, hf, hlen, ?_⟩
          rw [List.length_cons, hsum]
          have := sum_set_succ s.firings t.target hlt
          simp only [State.bump, State.setKey, State.firingsOf] at this ⊢
          rw [this]
          omega

/-- Every started firing is live or finished, and each start counted once
toward its node's firings. -/
def Counted (c : Case) (s : State) : Prop :=
  s.firings.length = c.nodes.length ∧
    s.steps.flatten.length = s.firings.sum ∧
      s.live.length + s.finished.length = s.steps.flatten.length

theorem counted_start (c : Case) : Counted c (start c) := by
  have h₀ : (List.replicate c.nodes.length 0).length = c.nodes.length := by simp
  obtain ⟨_, hfin, hlen, hsum⟩ := deliver_counts c
    ((seeds c).map fun (node, edge) => ⟨node, 0, edge⟩)
    { keys := [], firings := List.replicate c.nodes.length 0, live := []
      steps := [], finished := [], budgetExceeded := [] } h₀
  simp only at hfin
  refine ⟨hlen, ?_, ?_⟩
  · simp [start, hsum]
  · simp [start, hfin]

theorem counted_finish (c : Case) {s : State} (h : Counted c s) {firing : Key × Outcome}
    (hmem : firing ∈ s.live) :
    Counted c (finish c s firing) ∧
      (finish c s firing).finished.length = s.finished.length + 1 := by
  obtain ⟨hlen, hsteps, hlive⟩ := h
  obtain ⟨hl, hf, hlen', hsum⟩ := deliver_counts c (tokensOf c firing.1 firing.2)
    { s with live := s.live.erase firing, finished := s.finished ++ [firing] } hlen
  have herase := List.length_erase_of_mem hmem
  have hpos : 0 < s.live.length := List.length_pos_of_mem hmem
  refine ⟨⟨hlen', ?_, ?_⟩, ?_⟩
  · simp only [finish, deliver_steps, List.flatten_append, List.flatten_cons, List.flatten_nil,
      List.append_nil, List.length_append, List.length_map, List.length_mergeSort]
    rw [hsum, hsteps]
  · simp only [finish, deliver_steps, hl, hf, List.flatten_append, List.flatten_cons,
      List.flatten_nil, List.append_nil, List.length_append, List.length_map,
      List.length_mergeSort, herase, List.length_cons, List.length_nil]
    omega
  · simp [finish, hf]

/-- A host step finishes one live firing, until nothing is live. -/
theorem loop_progress (c : Case) :
    ∀ (fuel k : Nat) (s : State), Counted c s →
      Counted c (loop c fuel k s) ∧
        ((loop c fuel k s).live = [] ∨
          s.finished.length + fuel ≤ (loop c fuel k s).finished.length)
  | 0, _, s, h => ⟨h, Or.inr (by simp [loop])⟩
  | fuel + 1, k, s, h => by
    unfold loop
    split
    · rename_i hnone
      refine ⟨h, Or.inl ?_⟩
      rw [List.getElem?_eq_none_iff] at hnone
      cases hl : s.live with
      | nil => rfl
      | cons x xs =>
        rw [hl] at hnone
        have := Nat.mod_lt (c.schedule[k]?.getD 0) (show 0 < (x :: xs).length by simp)
        omega
    · rename_i firing hsome
      have hmem := List.mem_of_getElem? hsome
      obtain ⟨hc, hlen⟩ := counted_finish c h hmem
      obtain ⟨hc', hprog⟩ := loop_progress c fuel (k + 1) _ hc
      refine ⟨hc', ?_⟩
      rcases hprog with hprog | hprog
      · exact Or.inl hprog
      · exact Or.inr (by omega)

/-- The budgets bound how many firings a run can start. -/
theorem started_le_budgets (c : Case) {s : State} (hc : Counted c s) (hb : WithinBudget c s) :
    s.steps.flatten.length ≤ (c.nodes.map (·.maxFirings)).sum := by
  obtain ⟨hlen, hsteps, _⟩ := hc
  rw [hsteps]
  apply sum_le_of_le _ _ (by simp [hlen])
  intro i hl hm
  have hi : i < c.nodes.length := by simpa using hm
  have := hb i c.nodes[i] (List.getElem?_eq_getElem hi)
  simp only [State.firingsOf, List.getElem?_eq_getElem hl, Option.getD_some] at this
  simpa using this

/-- Every run ends with nothing live within the host steps its budgets allow,
so `run` never reports `unsettled`. -/
theorem run_settles (c : Case) :
    (loop c ((c.nodes.map (·.maxFirings)).sum + 1) 0 (start c)).live = [] := by
  obtain ⟨hc, hprog⟩ := loop_progress c _ 0 (start c) (counted_start c)
  rcases hprog with h | h
  · exact h
  · exfalso
    have hb := within_budget_loop c ((c.nodes.map (·.maxFirings)).sum + 1) 0 _
      (within_budget_start c)
    have hle := started_le_budgets c hc hb
    have hsum := hc.2.2
    omega

theorem run_not_unsettled (c : Case) : (run c).status ≠ "unsettled" := by
  simp only [run, run_settles, List.isEmpty_nil, Bool.not_true, Bool.false_eq_true, ↓reduceIte]
  split <;> decide

end PetriModel.Flow
