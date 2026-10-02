import PetriModel.Control
import PetriModel.Thm.Flow

/-!
# Stop theorems

The run's log lists its stops, starts, retries and routings, newest first,
and every entry carries the proof of its rule against the entries below it
(`Control.Valid`). These theorems read the rules back out of any run:

* `unmarked_never_starts_after_cancel`: a node a stop reached, or a firing
  fed by cancelled work, starts only when it is `run_on_cancel` (§5).
* `nothing_starts_after_kill`: nothing starts after a kill reached it.
* `cancelled_not_retried`: a firing is never retried once a stop reached its
  node.
* `killed_routes_nothing`: nothing routes out of a killed closure.
* `stop_status`: a settled run whose log holds a stop of the root ends
  `cancelled`, and one without ends otherwise.
* `run_settles`: every run ends with nothing live, within the steps of its
  schedule plus one per firing its budgets allow.
-/

namespace PetriModel.Control

/-! ## The log -/

/-- Every entry of a log keeps its rule against the entries below it. -/
theorem valid_of_logOK {c : Case} :
    ∀ {post pre : List Entry} {e : Entry}, LogOK c (post ++ e :: pre) → Valid c e pre
  | [], _, _, h => h.1
  | _ :: post, _, _, h => valid_of_logOK (post := post) h.2

/-- The log of any run keeps every rule. -/
theorem log_ok (c : Case) : LogOK c (runState c).log.entries :=
  (runState c).log.ok

/-- After a stop reached a node, or on a token from a cancelled firing, the
node starts only when it is `run_on_cancel`. -/
theorem unmarked_never_starts_after_cancel (c : Case) {post pre : List Entry} {node : Nat}
    {tainted : Bool} (h : (runState c).log.entries = post ++ .started node tainted :: pre)
    (hc : cancelledIn c pre node = true ∨ tainted = true) : c.runOnCancel node = true := by
  have hv : Valid c (.started node tainted) pre := valid_of_logOK (h ▸ log_ok c)
  apply hv.2
  rcases hc with hc | hc <;> simp [hc]

/-- Nothing starts after a kill reached its node. -/
theorem nothing_starts_after_kill (c : Case) {post pre : List Entry} {node : Nat}
    {tainted : Bool} (h : (runState c).log.entries = post ++ .started node tainted :: pre) :
    killedIn c pre node = false := by
  have hv : Valid c (.started node tainted) pre := valid_of_logOK (h ▸ log_ok c)
  exact hv.1

/-- A firing is never retried once a stop reached its node. -/
theorem cancelled_not_retried (c : Case) {post pre : List Entry} {node : Nat}
    (h : (runState c).log.entries = post ++ .retried node :: pre) :
    cancelledIn c pre node = false := by
  have hv : Valid c (.retried node) pre := valid_of_logOK (h ▸ log_ok c)
  exact hv

/-- Nothing routes out of a killed closure. -/
theorem killed_routes_nothing (c : Case) {post pre : List Entry} {node : Nat}
    (h : (runState c).log.entries = post ++ .routed node :: pre) :
    killedIn c pre node = false := by
  have hv : Valid c (.routed node) pre := valid_of_logOK (h ▸ log_ok c)
  exact hv

/-! ## The status -/

theorem failed_or_success_ne_cancelled (failed : Bool) :
    (if failed = true then "failed" else "success") ≠ "cancelled" := by
  cases failed <;> decide

/-- A settled run ends `cancelled` exactly when a stop of the root is in its
log: a root cancel or kill outranks every failure, and a group stop leaves
the status to the records. -/
theorem stop_status (c : Case) (h : (runState c).live = []) :
    (run c).status = "cancelled" ↔ rootStopped (runState c).log.entries = true := by
  simp only [run, h, List.isEmpty_nil, Bool.not_true, Bool.false_eq_true, ↓reduceIte]
  split
  · simp_all
  · rename_i hr
    simp only [hr, Bool.false_eq_true, iff_false]
    exact failed_or_success_ne_cancelled _

/-! ## Every run ends -/

open Flow (sum_set_succ sum_le_of_le)

variable {c : Case}

/-- The counts `run_settles` rests on: one firing count per node, each within
its budget, and every live or finished firing counted once. -/
def Inv (c : Case) (s : State c) : Prop :=
  s.firings.length = c.nodes.length ∧
    s.live.length + s.finished.length ≤ s.firings.sum ∧
      ∀ n node, c.nodes[n]? = some node → s.firingsOf n ≤ node.maxFirings

/-- Counting one more firing of a node with room in its budget keeps the
counts, and raises their sum by one. -/
theorem bump_inv {s : State c} {n : Nat} {node : Flow.SpecNode} (h : Inv c s)
    (hn : c.nodes[n]? = some node) (hroom : ¬node.maxFirings ≤ s.firingsOf n) :
    (s.bump n).firings.length = c.nodes.length ∧
      (s.bump n).firings.sum = s.firings.sum + 1 ∧
        ∀ m node', c.nodes[m]? = some node' → (s.bump n).firingsOf m ≤ node'.maxFirings := by
  obtain ⟨hlen, _, hbudget⟩ := h
  have hlt : n < s.firings.length := by
    rw [hlen]; exact (List.getElem?_eq_some_iff.mp hn).1
  refine ⟨by simpa [State.bump] using hlen, ?_, ?_⟩
  · simpa [State.bump, State.firingsOf] using sum_set_succ s.firings n hlt
  · intro m node' hm
    simp only [State.firingsOf] at hroom ⊢
    rw [List.getElem?_set]
    split
    · rename_i heq
      subst heq
      rw [hn] at hm
      cases hm
      simp only [Option.getD_some]
      omega
    · exact hbudget m node' hm

theorem deliverOne_inv (s : State c) (t : Token) (h : Inv c s) :
    Inv c (deliverOne s t).1 ∧ (deliverOne s t).1.finished = s.finished := by
  obtain ⟨hlen, hcount, hbudget⟩ := h
  unfold deliverOne
  split
  · exact ⟨⟨hlen, hcount, hbudget⟩, rfl⟩
  · rename_i node hn
    dsimp only
    split
    · exact ⟨⟨hlen, hcount, hbudget⟩, rfl⟩
    · split
      · exact ⟨⟨hlen, hcount, hbudget⟩, rfl⟩
      · split
        · exact ⟨⟨hlen, hcount, hbudget⟩, rfl⟩
        · rename_i hroom
          obtain ⟨hlen', hsum, hbudget'⟩ := bump_inv (s := (s.setKey _ _).markTainted _ _)
            ⟨hlen, hcount, hbudget⟩ hn hroom
          split
          · refine ⟨⟨hlen', ?_, hbudget'⟩, rfl⟩
            simp only [List.length_append, List.length_singleton] at hsum ⊢
            rw [hsum]
            omega
          · exact ⟨⟨hlen', by rw [hsum]; exact Nat.le_succ_of_le hcount, hbudget'⟩, rfl⟩

theorem deliverOne_finished (s : State c) (t : Token) :
    (deliverOne s t).1.finished = s.finished := by
  unfold deliverOne
  split
  · rfl
  · dsimp only
    split
    · rfl
    · split
      · rfl
      · split
        · rfl
        · split <;> rfl

theorem deliver_finished : ∀ (ts : List Token) (s : State c),
    (deliver s ts).1.finished = s.finished
  | [], _ => rfl
  | t :: rest, s => (deliver_finished rest _).trans (deliverOne_finished s t)

theorem decideAll_finished : ∀ (fuel : Nat) (s : State c) (ds : List Decision),
    (decideAll fuel s ds).1.finished = s.finished
  | 0, _, _ => rfl
  | _ + 1, _, [] => rfl
  | fuel + 1, s, .admit id :: rest => by
    unfold decideAll
    split
    · exact decideAll_finished fuel s rest
    · exact decideAll_finished fuel _ rest
  | fuel + 1, s, .route r :: rest => by
    unfold decideAll
    split
    · exact decideAll_finished fuel s rest
    · dsimp only
      exact (decideAll_finished fuel _ _).trans (deliver_finished _ _)

theorem deliver_inv : ∀ (ts : List Token) (s : State c), Inv c s → Inv c (deliver s ts).1
  | [], _, h => h
  | t :: rest, s, h => deliver_inv rest _ (deliverOne_inv s t h).1

theorem decideAll_inv : ∀ (fuel : Nat) (s : State c) (ds : List Decision), Inv c s →
    Inv c (decideAll fuel s ds).1
  | 0, _, _, h => h
  | _ + 1, _, [], h => h
  | fuel + 1, s, .admit id :: rest, h => by
    unfold decideAll
    split
    · exact decideAll_inv fuel s rest h
    · obtain ⟨hlen, hcount, hbudget⟩ := h
      exact decideAll_inv fuel _ rest ⟨hlen, by simpa using hcount, hbudget⟩
  | fuel + 1, s, .route r :: rest, h => by
    unfold decideAll
    split
    · exact decideAll_inv fuel s rest h
    · dsimp only
      exact decideAll_inv fuel _ _ (deliver_inv _ _ h)

/-- A state that differs from one keeping the counts only in its log, its
live firings and its finished ones, with no more of them together, keeps the
counts. -/
theorem inv_of_counts {s S : State c} (h : Inv c s) (hf : S.firings = s.firings)
    (hcount : S.live.length + S.finished.length ≤ s.live.length + s.finished.length) :
    Inv c S := by
  obtain ⟨hlen, hc, hbudget⟩ := h
  refine ⟨hf ▸ hlen, by rw [hf]; omega, fun n node hn => ?_⟩
  simp only [State.firingsOf, hf]
  exact hbudget n node hn

/-- Updating a live firing in place keeps the live count and every id. -/
theorem update_live (s : State c) (f : Live) :
    (s.update f).live.length = s.live.length ∧
      ∀ id, (∃ g ∈ s.live, g.id = id) → ∃ g ∈ (s.update f).live, g.id = id := by
  refine ⟨by simp [State.update], ?_⟩
  rintro id ⟨g, hg, rfl⟩
  refine ⟨if g.id == f.id then f else g, ?_, ?_⟩
  · simp only [State.update, List.mem_map]
    exact ⟨g, hg, rfl⟩
  · split
    · rename_i heq
      exact (beq_iff_eq.mp heq).symm
    · rfl

/-- Removing a live firing by id shortens the live list. -/
theorem filter_id_lt {live : List Live} {id : Nat} (h : ∃ g ∈ live, g.id = id) :
    (live.filter (·.id != id)).length + 1 ≤ live.length := by
  obtain ⟨g, hg, rfl⟩ := h
  have := (List.length_filter_lt_length_iff_exists (p := fun x : Live => x.id != g.id)
    (l := live)).mpr ⟨g, hg, by simp⟩
  omega

theorem runAttempts_inv (every : Bool) :
    ∀ (fuel : Nat) (s : State c) (f : Live), Inv c s → (∃ g ∈ s.live, g.id = f.id) →
      Inv c (runAttempts every fuel s f).1 ∧
        s.finished.length ≤ (runAttempts every fuel s f).1.finished.length
  | 0, _, _, h, _ => ⟨h, Nat.le_refl _⟩
  | fuel + 1, s, f, h, hf => by
    have hu := update_live s
    unfold runAttempts
    dsimp only
    split
    · split
      · obtain ⟨hl, hids⟩ := hu { f with attempt := f.attempt + 1 }
        exact runAttempts_inv every fuel _ _
          (inv_of_counts h rfl (by simp only [State.update] at hl ⊢; omega)) (hids f.id hf)
      · obtain ⟨hl, _⟩ := hu { f with waiting := true }
        exact ⟨inv_of_counts h rfl (by simp only [State.update] at hl ⊢; omega), Nat.le_refl _⟩
    · have hlt := filter_id_lt hf
      refine ⟨decideAll_inv _ _ _ (inv_of_counts h rfl ?_), ?_⟩
      · simp only [List.length_append, List.length_singleton]
        omega
      · rw [decideAll_finished]
        simp

/-- With fuel for every attempt left, the host's attempts end in a record. -/
theorem runAttempts_finishes :
    ∀ (fuel : Nat) (s : State c) (f : Live) (n : Nat),
      (retryOf c f.key.1).limit - f.attempt < fuel → n ≤ s.finished.length →
        n + 1 ≤ (runAttempts true fuel s f).1.finished.length
  | 0, _, _, _, h, _ => absurd h (Nat.not_lt_zero _)
  | fuel + 1, s, f, n, hfuel, hn => by
    unfold runAttempts
    dsimp only
    split
    · rename_i hr
      simp only [Bool.and_eq_true, decide_eq_true_eq] at hr
      simp only [↓reduceIte]
      exact runAttempts_finishes fuel _ { f with attempt := f.attempt + 1 } n
        (show (retryOf c f.key.1).limit - (f.attempt + 1) < fuel by omega) hn
    · rw [decideAll_finished]
      simp only [List.length_append, List.length_singleton]
      omega

/-- The most attempts any node allows bounds each node's limit. -/
theorem foldl_max_ge_init : ∀ (l : List Nat) (a : Nat), a ≤ l.foldl max a
  | [], _ => Nat.le_refl _
  | x :: xs, a => Nat.le_trans (Nat.le_max_left a x) (foldl_max_ge_init xs (max a x))

theorem foldl_max_ge_mem : ∀ (l : List Nat) (a x : Nat), x ∈ l → x ≤ l.foldl max a
  | [], _, _, h => absurd h List.not_mem_nil
  | y :: ys, a, x, h => by
    simp only [List.foldl_cons]
    rcases List.mem_cons.mp h with rfl | h
    · exact Nat.le_trans (Nat.le_max_right a x) (foldl_max_ge_init ys _)
    · exact foldl_max_ge_mem ys _ x h

theorem limit_lt_attemptFuel (c : Case) (n : Nat) : (retryOf c n).limit < attemptFuel c := by
  have hinit := foldl_max_ge_init (c.nodes.map (·.retry.maxAttempts)) 1
  simp only [attemptFuel, retryOf, Flow.Retry.limit]
  cases hn : c.nodes[n]? with
  | none => simp only [Option.map_none, Option.getD_none, Nat.max_self]; omega
  | some node =>
    have hmem : node.retry.maxAttempts ∈ c.nodes.map (·.retry.maxAttempts) :=
      List.mem_map.mpr ⟨node, List.mem_of_getElem? hn, rfl⟩
    have := foldl_max_ge_mem _ 1 _ hmem
    simp only [Option.map_some, Option.getD_some]
    have := Nat.max_le.mpr ⟨this, hinit⟩
    omega

theorem length_filter_add_not {α : Type} (p : α → Bool) :
    ∀ l : List α, (l.filter p).length + (l.filter (not ∘ p)).length = l.length
  | [] => rfl
  | x :: xs => by
    have := length_filter_add_not p xs
    simp only [List.filter_cons, Function.comp_apply]
    by_cases hx : p x = true <;> simp [hx] <;> omega

/-- The host step `finish` or `attempt` keeps the counts and finishes no fewer
firings. -/
theorem hostFinish_inv (every : Bool) (s : State c) (choice : Nat) (h : Inv c s) :
    Inv c (hostFinish every s choice) ∧
      s.finished.length ≤ (hostFinish every s choice).finished.length := by
  unfold hostFinish
  dsimp only
  split
  · exact ⟨h, Nat.le_refl _⟩
  · rename_i f hf
    have hmem : f ∈ s.live := List.mem_mergeSort.mp (List.mem_of_getElem? hf)
    dsimp only
    generalize hf' : (if f.waiting = true then { f with attempt := f.attempt + 1, waiting := false }
      else f) = f'
    have hid : f'.id = f.id := by rw [← hf']; split <;> rfl
    obtain ⟨hl, hids⟩ := update_live s f'
    exact runAttempts_inv every _ _ _
      (inv_of_counts h rfl (by simp only [State.update] at hl ⊢; omega))
      (hids f'.id ⟨f, hmem, hid.symm⟩)

/-- After the schedule, a host step finishes a live firing. -/
theorem hostFinish_progress (s : State c) (h : s.live ≠ []) :
    s.finished.length + 1 ≤ (hostFinish true s 0).finished.length := by
  unfold hostFinish
  dsimp only
  split
  · rename_i hnone
    rw [List.getElem?_eq_none_iff, List.length_mergeSort] at hnone
    have hpos : 0 < s.live.length := List.length_pos_iff.mpr h
    simp only [Nat.zero_mod] at hnone
    omega
  · rename_i f _
    dsimp only
    generalize hf' : (if f.waiting = true then { f with attempt := f.attempt + 1, waiting := false }
      else f) = f'
    exact runAttempts_finishes _ _ _ _
      (Nat.lt_of_le_of_lt (Nat.sub_le _ _) (limit_lt_attemptFuel c f'.key.1)) (Nat.le_refl _)

/-- A stop keeps the counts: every firing it settles leaves the live list for
the finished one. -/
theorem hostStop_inv (s : State c) (tier : Tier) (target : Target) (h : Inv c s) :
    Inv c (hostStop s tier target) ∧
      s.finished.length ≤ (hostStop s tier target).finished.length := by
  unfold hostStop
  dsimp only
  have hparts := length_filter_add_not (fun f : Live => c.covers target f.key.1 && f.waiting) s.live
  refine ⟨decideAll_inv _ _ _ (inv_of_counts h rfl ?_), ?_⟩
  · simp only [List.partition_eq_filter_filter, List.length_map, List.length_append, sorted,
      List.length_mergeSort]
    omega
  · rw [decideAll_finished]
    simp

theorem act_inv (s : State c) (a : Action) (h : Inv c s) :
    Inv c (act s a) ∧ s.finished.length ≤ (act s a).finished.length := by
  cases a with
  | finish choice => exact hostFinish_inv true s choice h
  | attempt choice => exact hostFinish_inv false s choice h
  | stop tier target => exact hostStop_inv s tier target h

theorem loop_inv (c : Case) :
    ∀ (fuel k : Nat) (s : State c), Inv c s → Inv c (loop c fuel k s)
  | 0, _, _, h => h
  | fuel + 1, k, s, h => by
    unfold loop
    split
    · exact h
    · exact loop_inv c fuel (k + 1) _ (act_inv s _ h).1

theorem start_inv (c : Case) : Inv c (start c) := by
  unfold start
  dsimp only
  refine decideAll_inv _ _ _ (deliver_inv _ _ ⟨by simp, by simp, fun n node _ => ?_⟩)
  simp only [State.firingsOf, List.getElem?_replicate]
  split <;> simp

theorem loop_of_empty (c : Case) {s : State c} (h : s.live = []) :
    ∀ (fuel k : Nat), loop c fuel k s = s
  | 0, _ => rfl
  | _ + 1, _ => by simp [loop, h]

theorem loop_succ (c : Case) (fuel k : Nat) (s : State c) :
    loop c (fuel + 1) k s =
      if s.live.isEmpty then s else loop c fuel (k + 1) (act s ((c.schedule[k]?).getD (.finish 0))) :=
  rfl

/-- The schedule's steps, then the rest. -/
theorem loop_split (c : Case) :
    ∀ (n m k : Nat) (s : State c), loop c (n + m) k s = loop c m (k + n) (loop c n k s)
  | 0, m, k, s => by simp [loop]
  | n + 1, m, k, s => by
    rw [show n + 1 + m = (n + m) + 1 by omega, loop_succ c (n + m) k s, loop_succ c n k s]
    by_cases he : s.live.isEmpty = true
    · simp only [he, ↓reduceIte]
      exact (loop_of_empty c (List.isEmpty_iff.mp he) _ _).symm
    · simp only [he, Bool.false_eq_true, ↓reduceIte]
      rw [loop_split c n m (k + 1), show k + 1 + n = k + (n + 1) by omega]

/-- After the schedule, every host step finishes a firing until nothing is
live. -/
theorem loop_progress (c : Case) :
    ∀ (fuel k : Nat) (s : State c), Inv c s → c.schedule.length ≤ k →
      (loop c fuel k s).live = [] ∨ s.finished.length + fuel ≤ (loop c fuel k s).finished.length
  | 0, _, _, _, _ => Or.inr (by simp [loop])
  | fuel + 1, k, s, h, hk => by
    unfold loop
    split
    · exact Or.inl (List.isEmpty_iff.mp (by assumption))
    · rename_i hne
      have ha : (c.schedule[k]?).getD (.finish 0) = .finish 0 := by
        rw [List.getElem?_eq_none (by omega)]; rfl
      rw [ha]
      have hlive : s.live ≠ [] := fun he => hne (List.isEmpty_iff.mpr he)
      have hprog := hostFinish_progress s hlive
      rcases loop_progress c fuel (k + 1) _ (hostFinish_inv true s 0 h).1 (by omega) with hl | hl
      · exact Or.inl hl
      · exact Or.inr (by simp only [act] at hl ⊢; omega)

/-- The budgets bound how many firings a run counts, so how many it can
finish. -/
theorem finished_le_budgets (c : Case) {s : State c} (h : Inv c s) :
    s.finished.length ≤ (c.nodes.map (·.maxFirings)).sum := by
  obtain ⟨hlen, hcount, hbudget⟩ := h
  have hsum : s.firings.sum ≤ (c.nodes.map (·.maxFirings)).sum := by
    apply sum_le_of_le _ _ (by simp [hlen])
    intro i hl hm
    have hi : i < c.nodes.length := by simpa using hm
    have := hbudget i c.nodes[i] (List.getElem?_eq_getElem hi)
    simp only [State.firingsOf, List.getElem?_eq_getElem hl, Option.getD_some] at this
    simpa using this
  omega

/-- Every run ends with nothing live, within the steps of its schedule plus
one per firing its budgets allow, so `run` never reports `unsettled`. -/
theorem run_settles (c : Case) : (runState c).live = [] := by
  unfold runState
  rw [show c.schedule.length + (c.nodes.map (·.maxFirings)).sum + 1 =
      c.schedule.length + ((c.nodes.map (·.maxFirings)).sum + 1) by omega,
    loop_split, Nat.zero_add]
  have hI := loop_inv c c.schedule.length 0 (start c) (start_inv c)
  rcases loop_progress c _ c.schedule.length _ hI (Nat.le_refl _) with h | h
  · exact h
  · exfalso
    have := finished_le_budgets c
      (loop_inv c ((c.nodes.map (·.maxFirings)).sum + 1) c.schedule.length _ hI)
    omega

theorem run_not_unsettled (c : Case) : (run c).status ≠ "unsettled" := by
  simp only [run, run_settles, List.isEmpty_nil, Bool.not_true, Bool.false_eq_true, ↓reduceIte]
  split
  · decide
  · split <;> decide

end PetriModel.Control
