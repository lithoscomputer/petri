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
* `run_settles`: every run ends with nothing live and no decision open, even
  when the host holds its decisions and stops land between them. It rests on
  `Held`: every live firing was admitted, or its admission is open. A stop
  keeps it only because it settles each firing whose admission it withdraws.
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
theorem stop_status (c : Case) (h : (runState c).live = []) (hd : (runState c).held = []) :
    (run c).status = "cancelled" ↔ rootStopped (runState c).log.entries = true := by
  simp only [run, h, hd, List.isEmpty_nil, Bool.not_true, Bool.or_false, Bool.false_eq_true,
    ↓reduceIte]
  split
  · simp_all
  · rename_i hr
    simp only [hr, Bool.false_eq_true, iff_false]
    exact failed_or_success_ne_cancelled _

/-! ## Every run ends -/

open Flow (sum_set_succ sum_le_of_le recorded)

variable {c : Case}

/-- The budget counts: one firing count per node, each within its budget. -/
def Inv (c : Case) (s : State c) : Prop :=
  s.firings.length = c.nodes.length ∧
    ∀ n node, c.nodes[n]? = some node → s.firingsOf n ≤ node.maxFirings

/-- Every live firing was admitted, or its admission is open: held by the
host, or among the decisions `ds` still to answer. -/
def Held (s : State c) (ds : List Decision) : Prop :=
  ∀ f ∈ s.live, f.admitted = true ∨ Decision.admit f.id ∈ s.held ∨ Decision.admit f.id ∈ ds

/-- `Held` survives a step that keeps every live firing's admission or its
open admission. -/
theorem Held.mono {s S : State c} {ds ds' : List Decision} (h : Held s ds)
    (hlive : ∀ f ∈ S.live, ∃ g ∈ s.live, g.id = f.id ∧ (g.admitted = true → f.admitted = true))
    (hopen : ∀ id, Decision.admit id ∈ s.held ∨ Decision.admit id ∈ ds →
      Decision.admit id ∈ S.held ∨ Decision.admit id ∈ ds') :
    Held S ds' := by
  intro f hf
  obtain ⟨g, hg, hid, hadm⟩ := hlive f hf
  rcases h g hg with ha | ho | ho
  · exact Or.inl (hadm ha)
  · exact Or.inr (hid ▸ hopen g.id (Or.inl ho))
  · exact Or.inr (hid ▸ hopen g.id (Or.inr ho))

/-- A step that leaves the live firings and the held decisions alone keeps
`Held`, for any list of decisions holding the old ones. -/
theorem Held.same {s S : State c} {ds ds' : List Decision} (h : Held s ds)
    (hlive : S.live = s.live) (hheld : S.held = s.held) (hds : ∀ x ∈ ds, x ∈ ds') :
    Held S ds' :=
  h.mono (fun f hf => ⟨f, hlive ▸ hf, rfl, id⟩)
    (fun _ ho => hheld ▸ ho.imp id (hds _))

theorem sum_le_budget {s : State c} (h : Inv c s) : s.firings.sum ≤ budget c := by
  obtain ⟨hlen, hbudget⟩ := h
  apply sum_le_of_le _ _ (by simp [hlen])
  intro i hl hm
  have hi : i < c.nodes.length := by simpa using hm
  have := hbudget i c.nodes[i] (List.getElem?_eq_getElem hi)
  simp only [State.firingsOf, List.getElem?_eq_getElem hl, Option.getD_some] at this
  simpa using this

/-- Counting one more firing of a node with room in its budget keeps the
counts, and raises their sum by one. -/
theorem bump_inv {s : State c} {n : Nat} {node : Flow.SpecNode} (h : Inv c s)
    (hn : c.nodes[n]? = some node) (hroom : ¬node.maxFirings ≤ s.firingsOf n) :
    Inv c (s.bump n) ∧ (s.bump n).firings.sum = s.firings.sum + 1 := by
  obtain ⟨hlen, hbudget⟩ := h
  have hlt : n < s.firings.length := by
    rw [hlen]; exact (List.getElem?_eq_some_iff.mp hn).1
  refine ⟨⟨by simpa [State.bump] using hlen, ?_⟩, ?_⟩
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
  · simpa [State.bump, State.firingsOf] using sum_set_succ s.firings n hlt

/-- One token: it keeps the counts, raises at most one decision for each
firing it counts, and the new firing's admission is among them. -/
theorem deliverOne_inv (s : State c) (t : Token) (h : Inv c s) :
    Inv c (deliverOne s t).1 ∧
      (deliverOne s t).2.length + owed (deliverOne s t).1 ≤ owed s ∧
        ∀ ds, Held s ds → Held (deliverOne s t).1 ((deliverOne s t).2 ++ ds) := by
  unfold deliverOne
  split
  · exact ⟨h, by simp, fun _ hh => hh⟩
  · rename_i node hn
    dsimp only
    split
    · exact ⟨h, by simp, fun _ hh => hh⟩
    · split
      · exact ⟨h, by simp [owed], fun _ hh => hh.same rfl rfl (fun _ hx => by simpa using hx)⟩
      · split
        · exact ⟨h, by simp [owed], fun _ hh => hh.same rfl rfl (fun _ hx => by simpa using hx)⟩
        · rename_i hroom
          obtain ⟨hinv, hsum⟩ := bump_inv (s := (s.setKey _ _).markTainted _ _) h hn hroom
          have hle := sum_le_budget hinv
          simp only [State.bump] at hinv hsum hle
          split
          · refine ⟨hinv, ?_, fun ds hh => ?_⟩
            · simp only [owed, List.length_append, List.length_singleton] at hsum hle ⊢
              omega
            · intro f hf
              simp only [List.mem_append, List.mem_singleton] at hf
              rcases hf with hf | rfl
              · rcases hh f hf with ha | ho | ho
                · exact Or.inl ha
                · exact Or.inr (Or.inl ho)
                · exact Or.inr (Or.inr (by simp [ho]))
              · exact Or.inr (Or.inr (by simp))
          · refine ⟨hinv, ?_, fun ds hh => hh.same rfl rfl (fun _ hx => by simp [hx])⟩
            simp only [owed, List.length_singleton] at hsum hle ⊢
            omega

theorem deliver_inv : ∀ (ts : List Token) (s : State c), Inv c s →
    Inv c (deliver s ts).1 ∧ (deliver s ts).2.length + owed (deliver s ts).1 ≤ owed s ∧
      ∀ ds, Held s ds → Held (deliver s ts).1 ((deliver s ts).2 ++ ds)
  | [], _, h => ⟨h, by simp [deliver], fun _ hh => hh⟩
  | t :: rest, s, h => by
    obtain ⟨h1, o1, k1⟩ := deliverOne_inv s t h
    obtain ⟨h2, o2, k2⟩ := deliver_inv rest _ h1
    simp only [deliver]
    refine ⟨h2, ?_, fun ds hh => ?_⟩
    · simp only [List.length_append]
      omega
    · exact (k2 _ (k1 ds hh)).same rfl rfl (fun x hx => by
        simp only [List.mem_append] at hx ⊢
        rcases hx with hx | hx | hx <;> simp [hx])

/-- The admission of `id` admits its firing, and changes no count. -/
theorem admit_live (s : State c) (id node : Nat) :
    ∀ f ∈ (s.admit id node).live, ∃ g ∈ s.live, g.id = f.id ∧
      (f.admitted = true ∨ g.id ≠ id ∧ f = g) := by
  intro f hf
  simp only [List.mem_map] at hf
  obtain ⟨g, hg, rfl⟩ := hf
  refine ⟨g, hg, ?_, ?_⟩
  · split <;> rfl
  · split
    · exact Or.inl rfl
    · rename_i hne
      exact Or.inr ⟨by simpa using hne, rfl⟩

/-- A decision that admits no first attempt leaves the pending list without
breaking `Held`. -/
theorem Held.drop_other {s : State c} {d : Decision} {ds : List Decision} (h : Held s (d :: ds))
    (hd : ∀ id, d ≠ .admit id) : Held s ds := by
  intro f hf
  rcases h f hf with ha | ho | ho
  · exact Or.inl ha
  · exact Or.inr (Or.inl ho)
  · rcases List.mem_cons.mp ho with ho | ho
    · exact absurd ho.symm (hd f.id)
    · exact Or.inr (Or.inr ho)

/-- Removing a live firing by id shortens the live list. -/
theorem filter_id_lt {live : List Live} {id : Nat} (h : ∃ g ∈ live, g.id = id) :
    (live.filter (·.id != id)).length + 1 ≤ live.length := by
  obtain ⟨g, hg, rfl⟩ := h
  have := (List.length_filter_lt_length_iff_exists (p := fun x : Live => x.id != g.id)
    (l := live)).mpr ⟨g, hg, by simp⟩
  omega

/-- A firing the host's answer ended leaves the live firings, owes one less,
and keeps every other live firing's admission. -/
theorem endBy_inv (s : State c) (f : Live) (v : Verdict) (h : Inv c s)
    (hf : ∃ g ∈ s.live, g.id = f.id) :
    Inv c (s.endBy f v).1 ∧ (s.endBy f v).2.length + owed (s.endBy f v).1 + 1 ≤ owed s ∧
      ∀ ds, (∀ g ∈ s.live, g.id ≠ f.id →
          g.admitted = true ∨ Decision.admit g.id ∈ s.held ∨ Decision.admit g.id ∈ ds) →
        Held (s.endBy f v).1 ((s.endBy f v).2 ++ ds) := by
  have hlt := filter_id_lt hf
  refine ⟨h, ?_, fun ds hh g hg => ?_⟩
  · simp only [State.endBy, owed, List.length_singleton]
    omega
  · simp only [State.endBy, List.mem_filter, bne_iff_ne, ne_eq] at hg
    obtain ⟨hg, hne⟩ := hg
    rcases hh g hg hne with ha | ho | ho
    · exact Or.inl ha
    · exact Or.inr (Or.inl ho)
    · exact Or.inr (Or.inr (List.mem_append_right _ ho))

/-- Answering a routing group changes no count, no live firing and no held
decision. -/
theorem pickArm_keeps (s : State c) (status : Flow.Status) (arms : List Flow.Arm) (roll : Nat) :
    (pickArm s status arms roll).1.firings = s.firings ∧
      (pickArm s status arms roll).1.live = s.live ∧ (pickArm s status arms roll).1.held = s.held := by
  unfold pickArm
  split <;> exact ⟨rfl, rfl, rfl⟩

theorem routeTokens_keeps (k : Flow.Key) (status : Flow.Status) :
    ∀ (groups : List (List Flow.Arm)) (s : State c),
      (routeTokens s k status groups).1.firings = s.firings ∧
        (routeTokens s k status groups).1.live = s.live ∧
          (routeTokens s k status groups).1.held = s.held
  | [], _ => ⟨rfl, rfl, rfl⟩
  | arms :: rest, s => by
    obtain ⟨f1, l1, h1⟩ := pickArm_keeps (s.roll).2 status arms (s.roll).1
    obtain ⟨f2, l2, h2⟩ := routeTokens_keeps k status rest (pickArm (s.roll).2 status arms (s.roll).1).1
    simp only [routeTokens]
    exact ⟨f2.trans f1, l2.trans l1, h2.trans h1⟩

/-- Answering a routing, group by group, then delivering its tokens keeps the
counts, raises at most one decision per firing it counts, and keeps `Held`. -/
theorem route_answer_inv (S : State c) (k : Flow.Key) (status : Flow.Status)
    (groups : List (List Flow.Arm)) (h : Inv c S) :
    Inv c (deliver (routeTokens S k status groups).1 (routeTokens S k status groups).2).1 ∧
      (deliver (routeTokens S k status groups).1 (routeTokens S k status groups).2).2.length +
          owed (deliver (routeTokens S k status groups).1
            (routeTokens S k status groups).2).1 ≤ owed S ∧
        ∀ ds, Held S ds →
          Held (deliver (routeTokens S k status groups).1 (routeTokens S k status groups).2).1
            ((deliver (routeTokens S k status groups).1
              (routeTokens S k status groups).2).2 ++ ds) := by
  obtain ⟨hf, hl, hd⟩ := routeTokens_keeps k status groups S
  have hi : Inv c (routeTokens S k status groups).1 := by
    obtain ⟨hlen, hbudget⟩ := h
    refine ⟨hf ▸ hlen, fun n node hn => ?_⟩
    simp only [State.firingsOf, hf]
    exact hbudget n node hn
  obtain ⟨hi', ho, hk'⟩ := deliver_inv _ _ hi
  refine ⟨hi', ?_, fun ds hh => hk' ds (hh.same hl hd (fun _ hx => hx))⟩
  simp only [owed, hf, hl, hd] at ho ⊢
  exact ho

/-- Answering one decision keeps the counts, raises at most one decision per
firing it counts, and leaves every other live firing's admission open. -/
theorem answer_inv (s : State c) (d : Decision) (h : Inv c s) :
    Inv c (answer s d).1 ∧ (answer s d).2.1.length + owed (answer s d).1 ≤ owed s ∧
      ∀ ds, Held s (d :: ds) → Held (answer s d).1 ((answer s d).2.1 ++ ds) := by
  cases d with
  | admit id =>
    cases hfind : s.live.find? (·.id == id) with
    | none =>
      simp only [answer, hfind]
      refine ⟨h, by simp, fun ds hh f hf => ?_⟩
      rcases hh f hf with ha | ho | ho
      · exact Or.inl ha
      · exact Or.inr (Or.inl ho)
      · rcases List.mem_cons.mp ho with ho | ho
        · exact absurd (by simp [Decision.admit.inj ho]) (List.find?_eq_none.mp hfind f hf)
        · exact Or.inr (Or.inr (by simpa using ho))
    | some f =>
      have hmem := List.mem_of_find?_eq_some hfind
      have hid : f.id = id := by simpa using List.find?_some hfind
      cases hv : admitVerdict (s.roll).1
      case admit =>
        simp only [answer, hfind, hv]
        refine ⟨h, by simp [owed], fun ds hh f' hf' => ?_⟩
        obtain ⟨g, hg, hid', hcase⟩ := admit_live (s.roll).2 id f.key.1 f' hf'
        rcases hcase with ha | ⟨hne, rfl⟩
        · exact Or.inl ha
        · rcases hh f' hg with ha | ho | ho
          · exact Or.inl ha
          · exact Or.inr (Or.inl ho)
          · rcases List.mem_cons.mp ho with ho | ho
            · exact absurd (Decision.admit.inj ho) hne
            · exact Or.inr (Or.inr (by simpa using ho))
      all_goals
        simp only [answer, hfind, hv]
        obtain ⟨hi, ho, hk⟩ := endBy_inv (s.roll).2 f _ h ⟨f, hmem, rfl⟩
        refine ⟨hi, by simp only [owed] at ho ⊢; omega, fun ds hh => hk ds fun g hg hne => ?_⟩
        rcases hh g hg with ha | ho | ho
        · exact Or.inl ha
        · exact Or.inr (Or.inl ho)
        · rcases List.mem_cons.mp ho with ho | ho
          · exact absurd ((Decision.admit.inj ho).trans hid.symm) hne
          · exact Or.inr (Or.inr ho)
  | retry id =>
    cases hfind : s.live.find? (·.id == id) with
    | none =>
      simp only [answer, hfind]
      exact ⟨h, by simp, fun ds hh => by simpa using hh.drop_other (fun _ h => by cases h)⟩
    | some f =>
      have hmem := List.mem_of_find?_eq_some hfind
      cases hv : admitVerdict (s.roll).1
      case admit =>
        simp only [answer, hfind, hv]
        exact ⟨h, by simp [owed], fun ds hh => by
          rw [List.nil_append]
          exact (hh.drop_other (fun _ h => by cases h)).same rfl rfl (fun _ hx => hx)⟩
      all_goals
        simp only [answer, hfind, hv]
        obtain ⟨hi, ho, hk⟩ := endBy_inv (s.roll).2 f _ h ⟨f, hmem, rfl⟩
        refine ⟨hi, by simp only [owed] at ho ⊢; omega, fun ds hh => hk ds fun g hg _ => ?_⟩
        exact (hh.drop_other (fun _ h => by cases h)) g hg
  | route r =>
    by_cases hk : killedIn c s.log.entries r.key.1 = true
    · simp only [answer, hk, ↓reduceDIte]
      exact ⟨h, by simp, fun ds hh => by simpa using hh.drop_other (fun _ h => by cases h)⟩
    · simp only [answer, hk, Bool.false_eq_true, ↓reduceDIte]
      refine ⟨(route_answer_inv _ _ _ _ (by exact h)).1, (route_answer_inv _ _ _ _ (by exact h)).2.1,
        fun ds hh => (route_answer_inv _ _ _ _ (by exact h)).2.2 ds ?_⟩
      exact hh.drop_other (fun _ h => by cases h)

/-- Holding decisions open keeps the counts, and owes one more per decision. -/
theorem hold_inv (s : State c) (ds : List Decision) (h : Inv c s) :
    Inv c { s with held := s.held ++ ds } ∧ owed { s with held := s.held ++ ds } = owed s + ds.length ∧
      (Held s ds → Held { s with held := s.held ++ ds } []) := by
  refine ⟨h, by simp only [owed, List.length_append]; omega, fun hh => ?_⟩
  exact hh.mono (fun f hf => ⟨f, hf, rfl, id⟩) (fun _ ho => Or.inl (List.mem_append.mpr ho))

theorem decideAll_inv : ∀ (fuel : Nat) (s : State c) (ds : List Decision), Inv c s →
    Inv c (decideAll fuel s ds).1 ∧ owed (decideAll fuel s ds).1 ≤ owed s + ds.length ∧
      (Held s ds → Held (decideAll fuel s ds).1 [])
  | 0, s, ds, h => by
    obtain ⟨hi, ho, hk⟩ := hold_inv s ds h
    exact ⟨hi, Nat.le_of_eq ho, hk⟩
  | _ + 1, s, [], h => ⟨h, by simp [decideAll], fun hh => by simpa [decideAll] using hh⟩
  | fuel + 1, s, d :: rest, h => by
    obtain ⟨h1, o1, k1⟩ := answer_inv s d h
    obtain ⟨h2, o2, k2⟩ := decideAll_inv fuel _ ((answer s d).2.1 ++ rest) h1
    simp only [decideAll]
    refine ⟨h2, ?_, fun hh => k2 (k1 rest hh)⟩
    simp only [List.length_append, List.length_cons] at o2 ⊢
    omega

theorem raise_inv (s : State c) (ds : List Decision) (h : Inv c s) :
    Inv c (raise s ds).1 ∧ owed (raise s ds).1 ≤ owed s + ds.length ∧
      (Held s ds → Held (raise s ds).1 []) := by
  unfold raise
  split
  · obtain ⟨hi, ho, hk⟩ := hold_inv s ds h
    exact ⟨hi, Nat.le_of_eq ho, hk⟩
  · exact decideAll_inv _ s ds h

/-- Ending a live firing by the host's answer and handing the host its
routing keeps the counts and `Held`, and owes less. -/
theorem endBy_raise_owed (s : State c) (f : Live) (v : Verdict) (h : Inv c s)
    (hf : ∃ g ∈ s.live, g.id = f.id) :
    owed (raise (s.endBy f v).1 (s.endBy f v).2).1 + 1 ≤ owed s := by
  obtain ⟨hi, ho, _⟩ := endBy_inv s f v h hf
  obtain ⟨_, ho', _⟩ := raise_inv (s.endBy f v).1 (s.endBy f v).2 hi
  omega

theorem endBy_raise (s : State c) (f : Live) (v : Verdict) (h : Inv c s) (hh : Held s [])
    (hf : ∃ g ∈ s.live, g.id = f.id) :
    Inv c (raise (s.endBy f v).1 (s.endBy f v).2).1 ∧
      Held (raise (s.endBy f v).1 (s.endBy f v).2).1 [] ∧
        owed (raise (s.endBy f v).1 (s.endBy f v).2).1 + 1 ≤ owed s := by
  obtain ⟨hi, _, hk⟩ := endBy_inv s f v h hf
  obtain ⟨hi', _, hk'⟩ := raise_inv (s.endBy f v).1 (s.endBy f v).2 hi
  exact ⟨hi', hk' (by simpa using hk [] (fun g hg _ => hh g hg)), endBy_raise_owed s f v h hf⟩

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

/-- Updating a live firing in place with an admitted one keeps `Held`, and
owes the same. -/
theorem update_held (s : State c) (f : Live) (hf : f.admitted = true) {ds : List Decision}
    (h : Held s ds) : Held (s.update f) ds ∧ owed (s.update f) = owed s := by
  refine ⟨h.mono (fun f' hf' => ?_) (fun _ ho => ho), by simp [owed, State.update]⟩
  simp only [State.update, List.mem_map] at hf'
  obtain ⟨g, hg, rfl⟩ := hf'
  refine ⟨g, hg, ?_, fun ha => ?_⟩
  · split
    · rename_i heq
      exact beq_iff_eq.mp heq
    · rfl
  · split
    · exact hf
    · exact ha

/-- The host's attempts of an admitted firing keep the counts and `Held`, and
owe no more. -/
theorem runAttempts_inv (every : Bool) :
    ∀ (fuel : Nat) (s : State c) (f : Live), Inv c s → Held s [] → f.admitted = true →
      (∃ g ∈ s.live, g.id = f.id) →
      Inv c (runAttempts every fuel s f).1 ∧ Held (runAttempts every fuel s f).1 [] ∧
        owed (runAttempts every fuel s f).1 ≤ owed s
  | 0, _, _, h, hh, _, _ => ⟨h, hh, Nat.le_refl _⟩
  | fuel + 1, s, f, h, hh, ha, hf => by
    unfold runAttempts
    dsimp only
    split
    · split
      · split
        · have hu := update_live s { f with attempt := f.attempt + 1 }
          obtain ⟨hk, ho⟩ := update_held s { f with attempt := f.attempt + 1 } ha hh
          refine And.imp id (And.imp id (fun hle => ?hle))
            (runAttempts_inv every fuel _ _ h ?hk ha ?hf)
          case hle => simp only [owed, State.update] at ho hle ⊢; omega
          case hk => exact hk
          case hf => simpa [State.update] using hu.2 f.id hf
        · -- The host's answer ended the firing before its next attempt.
          refine (endBy_raise _ _ _ ?h ?hh ?hf).imp id (And.imp id fun hle => ?hle)
          case h => exact h
          case hh => exact hh
          case hf => exact hf
          case hle => simp only [owed] at hle ⊢; omega
      · obtain ⟨hk, ho⟩ := update_held s { f with waiting := true } ha hh
        exact ⟨h, hk, by simp only [owed, State.update] at ho ⊢; omega⟩
    · have hlt := filter_id_lt hf
      obtain ⟨hi, ho, hk⟩ := raise_inv
        { s with
          live := s.live.filter (·.id != f.id)
          finished := s.finished ++ [f.key]
          attempts := s.attempts ++ [(f.key.1, f.key.2, f.attempt,
            (recorded (retryOf c f.key.1) f.attempt (report c f)).tag)]
          failed := s.failed || (recorded (retryOf c f.key.1) f.attempt (report c f)).isFailure }
        (if killedIn c s.log.entries f.key.1 then [] else
          [.route ⟨f.key, recorded (retryOf c f.key.1) f.attempt (report c f)⟩]) h
      refine ⟨hi, hk ?_, ?_⟩
      · refine hh.mono (fun g hg => ⟨g, (List.mem_filter.mp hg).1, rfl, id⟩) (fun _ ho => ?_)
        simpa using ho
      · have hlen : (if killedIn c s.log.entries f.key.1 = true then ([] : List Decision) else
            [.route ⟨f.key, recorded (retryOf c f.key.1) f.attempt (report c f)⟩]).length ≤ 1 := by
          split <;> simp
        simp only [owed] at ho ⊢
        omega

/-- With fuel for every attempt left, the host's attempts end in a record,
and owe less. -/
theorem runAttempts_progress :
    ∀ (fuel : Nat) (s : State c) (f : Live), (retryOf c f.key.1).limit - f.attempt < fuel →
      Inv c s → (∃ g ∈ s.live, g.id = f.id) → owed (runAttempts true fuel s f).1 + 1 ≤ owed s
  | 0, _, _, h, _, _ => absurd h (Nat.not_lt_zero _)
  | fuel + 1, s, f, hfuel, h, hf => by
    unfold runAttempts
    dsimp only
    split
    · rename_i hr
      simp only [Bool.and_eq_true, decide_eq_true_eq] at hr
      simp only [↓reduceIte]
      split
      · have hu := update_live s { f with attempt := f.attempt + 1 }
        refine Nat.le_trans (runAttempts_progress fuel _ _ ?hfuel h ?hf) ?hle
        case hfuel => show (retryOf c f.key.1).limit - (f.attempt + 1) < fuel; omega
        case hf => simpa [State.update] using hu.2 f.id hf
        case hle => simp [owed, State.update]
      · refine Nat.le_trans (endBy_raise_owed _ _ _ ?h ?hf) ?hle
        case h => exact h
        case hf => exact hf
        case hle => simp [owed]
    · have hlt := filter_id_lt hf
      obtain ⟨_, ho, _⟩ := raise_inv
        { s with
          live := s.live.filter (·.id != f.id)
          finished := s.finished ++ [f.key]
          attempts := s.attempts ++ [(f.key.1, f.key.2, f.attempt,
            (recorded (retryOf c f.key.1) f.attempt (report c f)).tag)]
          failed := s.failed || (recorded (retryOf c f.key.1) f.attempt (report c f)).isFailure }
        (if killedIn c s.log.entries f.key.1 then [] else
          [.route ⟨f.key, recorded (retryOf c f.key.1) f.attempt (report c f)⟩]) h
      have hlen : (if killedIn c s.log.entries f.key.1 = true then ([] : List Decision) else
          [.route ⟨f.key, recorded (retryOf c f.key.1) f.attempt (report c f)⟩]).length ≤ 1 := by
        split <;> simp
      simp only [owed] at ho ⊢
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

/-- A step that ends by handing the host decisions keeps the counts and
`Held`. -/
theorem raise_steps_inv (S : State c) (D : List Decision) (x : List (List Flow.Key)) (h : Inv c S)
    (hh : Held S D) :
    Inv c { (raise S D).1 with steps := x } ∧ Held { (raise S D).1 with steps := x } [] :=
  ⟨(raise_inv S D h).1, (raise_inv S D h).2.2 hh⟩

theorem hostRun_inv (every : Bool) (s : State c) (f : Live) (h : Inv c s) (hh : Held s [])
    (ha : f.admitted = true) (hf : ∃ g ∈ s.live, g.id = f.id) :
    Inv c (hostRun every s f) ∧ Held (hostRun every s f) [] := by
  have hu := update_live s f
  obtain ⟨hk, _⟩ := update_held s f ha hh
  obtain ⟨h', hk', _⟩ := runAttempts_inv every (attemptFuel c) (s.update f) f h hk ha (hu.2 f.id hf)
  exact ⟨h', hk'⟩

theorem hostRun_progress (s : State c) (f : Live) (h : Inv c s) (hf : ∃ g ∈ s.live, g.id = f.id) :
    owed (hostRun true s f) + 1 ≤ owed s := by
  have hu := update_live s f
  have := runAttempts_progress (attemptFuel c) (s.update f) f
    (Nat.lt_of_le_of_lt (Nat.sub_le _ _) (limit_lt_attemptFuel c f.key.1)) h (hu.2 f.id hf)
  simp only [hostRun, owed, State.update, List.length_map] at this ⊢
  omega

/-- The firing a `finish` or `attempt` step chose is live and admitted. -/
theorem chosen_admitted {s : State c} {i : Nat} {f : Live}
    (hf : ((s.live.filter (·.admitted)).mergeSort fun a b => keyLe a.key b.key)[i]? = some f) :
    f ∈ s.live ∧ f.admitted = true :=
  List.mem_filter.mp (List.mem_mergeSort.mp (List.mem_of_getElem? hf))

/-- The host answering a firing's next attempt's admission inline keeps the
counts and `Held`. -/
theorem hostAnswer_inv (every : Bool) (s : State c) (f : Live) (h : Inv c s) (hh : Held s [])
    (ha : f.admitted = true) (hf : ∃ g ∈ s.live, g.id = f.id) :
    Inv c (hostAnswer every s f) ∧ Held (hostAnswer every s f) [] := by
  unfold hostAnswer
  dsimp only
  split
  · exact hostRun_inv every _ f (by exact h) (by exact hh) ha (by exact hf)
  · exact ⟨(endBy_raise _ f _ (by exact h) (by exact hh) (by exact hf)).1,
      (endBy_raise _ f _ (by exact h) (by exact hh) (by exact hf)).2.1⟩

/-- Answered inline, the admission runs the firing's attempts to a record,
or ends it: either way the host owes less. -/
theorem hostAnswer_progress (s : State c) (f : Live) (h : Inv c s)
    (hf : ∃ g ∈ s.live, g.id = f.id) : owed (hostAnswer true s f) + 1 ≤ owed s := by
  unfold hostAnswer
  dsimp only
  split
  · exact hostRun_progress _ f (by exact h) (by exact hf)
  · exact endBy_raise_owed _ f _ (by exact h) (by exact hf)

theorem hostFinish_inv (every : Bool) (s : State c) (choice : Nat) (h : Inv c s) (hh : Held s []) :
    Inv c (hostFinish every s choice) ∧ Held (hostFinish every s choice) [] := by
  unfold hostFinish
  dsimp only
  split
  · exact ⟨h, hh⟩
  · rename_i f hf
    obtain ⟨hmem, ha⟩ := chosen_admitted hf
    split
    · split
      · obtain ⟨hk, _⟩ := update_held s { f with attempt := f.attempt + 1, waiting := false } ha hh
        refine ⟨h, hk.mono (fun g hg => ⟨g, hg, rfl, id⟩) (fun _ ho => ?_)⟩
        rcases ho with ho | ho
        · exact Or.inl (List.mem_append.mpr (Or.inl ho))
        · exact Or.inr ho
      · exact hostAnswer_inv every s _ h hh ha ⟨f, hmem, rfl⟩
    · split
      · refine hostAnswer_inv every _ f h ?_ ha ⟨f, hmem, rfl⟩
        refine hh.mono (fun g hg => ⟨g, hg, rfl, id⟩) (fun _ ho => ?_)
        rcases ho with ho | ho
        · exact Or.inl (List.mem_filter.mpr ⟨ho, by simp [Decision.retries]⟩)
        · exact Or.inr ho
      · exact hostRun_inv every s f h hh ha ⟨f, hmem, rfl⟩

/-- With no decision open and something live, a `finish` finishes a firing,
and owes less. -/
theorem hostFinish_progress (s : State c) (h : Inv c s) (hh : Held s []) (hheld : s.held = [])
    (hlive : s.live ≠ []) : owed (hostFinish true s 0) + 1 ≤ owed s := by
  have hall : s.live.filter (·.admitted) = s.live := List.filter_eq_self.mpr fun f hf => by
    rcases hh f hf with ha | ho | ho
    · exact ha
    · simp [hheld] at ho
    · simp at ho
  unfold hostFinish
  dsimp only
  split
  · rename_i hnone
    rw [List.getElem?_eq_none_iff, List.length_mergeSort, hall] at hnone
    have hpos : 0 < s.live.length := List.length_pos_iff.mpr hlive
    simp only [Nat.zero_mod] at hnone
    omega
  · rename_i f hf
    obtain ⟨hmem, _⟩ := chosen_admitted hf
    split
    · simp only [Bool.not_true, Bool.and_false, Bool.false_eq_true, ↓reduceIte]
      exact hostAnswer_progress s _ h ⟨f, hmem, rfl⟩
    · simp only [hheld, List.any_nil, Bool.false_eq_true, ↓reduceIte]
      exact hostRun_progress s f h ⟨f, hmem, rfl⟩

theorem mem_eraseIdx_or {l : List Decision} {i : Nat} {d x : Decision} (hd : l[i]? = some d)
    (hx : x ∈ l) : x ∈ l.eraseIdx i ∨ x = d := by
  obtain ⟨j, hj⟩ := List.mem_iff_getElem?.mp hx
  by_cases hji : j = i
  · subst hji
    rw [hd] at hj
    exact Or.inr (Option.some.inj hj).symm
  · exact Or.inl (List.mem_eraseIdx_iff_getElem?.mpr ⟨j, hji, hj⟩)

/-- A `decide` step keeps the counts and `Held`; with a decision open, it owes
less. -/
theorem hostDecide_inv (s : State c) (choice : Nat) (h : Inv c s) (hh : Held s []) :
    Inv c (hostDecide s choice) ∧ Held (hostDecide s choice) [] ∧
      (s.held ≠ [] → owed (hostDecide s choice) + 1 ≤ owed s) := by
  unfold hostDecide
  split
  · rename_i hnone
    refine ⟨h, hh, fun hne => absurd hnone ?_⟩
    have hpos : 0 < s.held.length := List.length_pos_iff.mpr hne
    simp [Nat.mod_lt _ hpos]
  · rename_i d hd
    have hi : choice % s.held.length < s.held.length := (List.getElem?_eq_some_iff.mp hd).1
    have hk : Held { s with held := s.held.eraseIdx (choice % s.held.length) } [d] :=
      hh.mono (fun f hf => ⟨f, hf, rfl, id⟩) (fun _ ho => by
        rcases ho with ho | ho
        · rcases mem_eraseIdx_or hd ho with ho | ho
          · exact Or.inl ho
          · exact Or.inr (by simp [ho])
        · simp at ho)
    obtain ⟨h1, o1, k1⟩ := answer_inv { s with held := s.held.eraseIdx (choice % s.held.length) } d h
    obtain ⟨h2, o2, k2⟩ := raise_inv _ _ h1
    refine ⟨h2, k2 (by simpa using k1 [] hk), fun _ => ?_⟩
    have hlen := List.length_eraseIdx (l := s.held) (i := choice % s.held.length)
    simp only [hi, ↓reduceIte] at hlen
    simp only [owed] at o1 o2 ⊢
    rw [hlen] at o1
    omega

/-- Signalling a firing keeps its id and whether it was admitted. -/
theorem signal_keeps (p : Bool) (g : Live) :
    (if p = true then { g with signalled := true } else g).id = g.id ∧
      (if p = true then { g with signalled := true } else g).admitted = g.admitted := by
  split <;> exact ⟨rfl, rfl⟩

/-- A stop keeps the counts and `Held`: every firing it leaves live was
admitted or keeps its open admission, because every firing whose admission
it withdraws, it settles. -/
theorem hostStop_inv (s : State c) (tier : Tier) (target : Target) (h : Inv c s)
    (hh : Held s []) : Inv c (hostStop s tier target) ∧ Held (hostStop s tier target) [] := by
  unfold hostStop
  dsimp only
  refine raise_steps_inv _ _ _ h ?_
  · intro f hf
    simp only [List.partition_eq_filter_filter, List.mem_map, List.mem_filter] at hf
    obtain ⟨g, ⟨hg, hrest⟩, rfl⟩ := hf
    rw [(signal_keeps _ g).1, (signal_keeps _ g).2]
    rcases hh g hg with ha | ho | ho
    · exact Or.inl ha
    · refine Or.inr (Or.inl (List.mem_filter.mpr ⟨ho, ?_⟩))
      simp only [keeps, List.partition_eq_filter_filter, List.any_eq_true, List.mem_filter,
        beq_iff_eq]
      exact ⟨g, ⟨hg, hrest⟩, rfl⟩
    · simp at ho

theorem act_inv (s : State c) (a : Action) (h : Inv c s) (hh : Held s []) :
    Inv c (act s a) ∧ Held (act s a) [] := by
  cases a with
  | finish choice => exact hostFinish_inv true s choice h hh
  | attempt choice => exact hostFinish_inv false s choice h hh
  | stop tier target => exact hostStop_inv s tier target h hh
  | decide choice => exact ⟨(hostDecide_inv s choice h hh).1, (hostDecide_inv s choice h hh).2.1⟩

theorem loop_inv (c : Case) : ∀ (fuel k : Nat) (s : State c), Inv c s → Held s [] →
    Inv c (loop c fuel k s) ∧ Held (loop c fuel k s) []
  | 0, _, _, h, hh => ⟨h, hh⟩
  | fuel + 1, k, s, h, hh => by
    unfold loop
    split
    · exact ⟨h, hh⟩
    · obtain ⟨h', hh'⟩ := act_inv s _ h hh
      exact loop_inv c fuel (k + 1) _ h' hh'

theorem start_inv (c : Case) : Inv c (start c) ∧ Held (start c) [] := by
  unfold start
  dsimp only
  have h₀ : Inv c ({
      log := ⟨[], trivial⟩, keys := [], tainted := []
      firings := List.replicate c.nodes.length 0, started := List.replicate c.nodes.length 0
      nextId := 1, live := [], held := [], steps := [], finished := [], budgetExceeded := []
      completed := [], attempts := [], retries := [], controls := [], failed := false,
      used := 0 } : State c) :=
    ⟨by simp, fun n node _ => by
      simp only [State.firingsOf, List.getElem?_replicate]
      split <;> simp⟩
  refine raise_steps_inv _ _ _ (deliver_inv _ _ h₀).1 ?_
  simpa using (deliver_inv _ _ h₀).2.2 [] (fun f hf => by simp at hf)

/-- After the schedule, each step answers a decision or finishes a firing,
and owes less, until nothing is live or open. -/
theorem loop_settles (c : Case) : ∀ (fuel k : Nat) (s : State c), Inv c s → Held s [] →
    c.schedule.length ≤ k → owed s < fuel →
      (loop c fuel k s).live = [] ∧ (loop c fuel k s).held = []
  | 0, _, _, _, _, _, hf => absurd hf (Nat.not_lt_zero _)
  | fuel + 1, k, s, h, hh, hk, hf => by
    unfold loop
    split
    · rename_i he
      simp only [Bool.and_eq_true, List.isEmpty_iff] at he
      exact he
    · rename_i hne
      have ha : (c.schedule[k]?).getD (fallback s) = fallback s := by
        rw [List.getElem?_eq_none (by omega)]; rfl
      rw [ha]
      have hstep : Inv c (act s (fallback s)) ∧ Held (act s (fallback s)) [] ∧
          owed (act s (fallback s)) + 1 ≤ owed s := by
        unfold fallback
        split
        · rename_i hheld
          have hheld' : s.held = [] := List.isEmpty_iff.mp hheld
          have hlive : s.live ≠ [] := fun hl => hne (by simp [hl, hheld'])
          exact ⟨(hostFinish_inv true s 0 h hh).1, (hostFinish_inv true s 0 h hh).2,
            hostFinish_progress s h hh hheld' hlive⟩
        · rename_i hheld
          have hheld' : s.held ≠ [] := fun he => hheld (by simp [he])
          obtain ⟨h', hh', ho⟩ := hostDecide_inv s 0 h hh
          exact ⟨h', hh', ho hheld'⟩
      obtain ⟨h', hh', ho⟩ := hstep
      exact loop_settles c fuel (k + 1) _ h' hh' (by omega) (by omega)

/-- Every run ends with nothing live and no decision open, so `run` never
reports `unsettled`. -/
theorem run_settles (c : Case) : (runState c).live = [] ∧ (runState c).held = [] := by
  unfold runState
  dsimp only
  obtain ⟨h, hh⟩ := loop_inv c c.schedule.length 0 (start c) (start_inv c).1 (start_inv c).2
  exact loop_settles c _ c.schedule.length _ h hh (Nat.le_refl _) (Nat.lt_succ_self _)

theorem run_not_unsettled (c : Case) : (run c).status ≠ "unsettled" := by
  simp only [run, (run_settles c).1, (run_settles c).2, List.isEmpty_nil, Bool.not_true,
    Bool.or_false, Bool.false_eq_true, ↓reduceIte]
  split
  · decide
  · split <;> decide

end PetriModel.Control
