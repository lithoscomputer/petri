import PetriModel.Thm.Control
import PetriModel.Thm.Retry

/-!
# The control model without stops

`control_free_agrees`: on a case whose host only finishes firings, never
stops anything and answers every decision with the default, `Control.run`
observes exactly what `Spec.run` does. The flow and retry theorems, proved
about `Spec.run`, then hold for those runs of the control model too; the last
section carries three of them over.

The proof runs the two models side by side. `Agrees` relates a control state
to a flow state: the same keys, counts, steps and records, the same live
firings, and nothing stopped, tainted or held open. Both models start in
agreement (`start_agrees`), and a `finish` step keeps it (`finish_agrees`):
the host picks the same firing, runs its attempts as the retry model counts
them (`runAttempts_quiet`), and routes and admits what the flow model
delivers (`raise_route`). Both runs settle, so running either longer changes
nothing, and the final states agree (`runState_agrees`).
-/

namespace PetriModel.Control

open Flow (Key Status Outcome Spec SpecNode Retry attemptAt recorded baseDelay finalFrom)

variable {c : Case}

/-! ## The host's default answers -/

/-- A roll the host answers with the default: admit, and the core's pick. -/
def DefaultRoll (roll : Nat) : Prop :=
  roll % 20 < 14 ∧ roll % 10 < 8

theorem admitVerdict_default {roll : Nat} (h : DefaultRoll roll) : admitVerdict roll = .admit := by
  have := h.1
  unfold admitVerdict
  split <;> first | rfl | omega

theorem routeVerdict_default {roll : Nat} (h : DefaultRoll roll) : routeVerdict roll = .pick := by
  have := h.2
  unfold routeVerdict
  split <;> first | rfl | omega

/-- A case the control model runs as the flow model does: a host that never
holds a decision, answers every one with the default, and whose scripts
never report `cancelled`, which only a host honoring a stop does. -/
structure Quiet (c : Case) : Prop where
  holds : c.holds = false
  rolls : ∀ roll ∈ c.verdicts, DefaultRoll roll
  scripts : ∀ node ∈ c.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script

theorem Quiet.roll (hq : Quiet c) (s : State c) : DefaultRoll (s.roll).1 := by
  show DefaultRoll ((c.verdicts[s.used]?).getD 0)
  cases h : c.verdicts[s.used]? with
  | none => exact ⟨by decide, by decide⟩
  | some roll => exact hq.rolls roll (List.mem_of_getElem? h)

theorem Quiet.admit (hq : Quiet c) (s : State c) : admitVerdict (s.roll).1 = .admit :=
  admitVerdict_default (hq.roll s)

theorem Quiet.pick (hq : Quiet c) (s : State c) : routeVerdict (s.roll).1 = .pick :=
  routeVerdict_default (hq.roll s)

theorem attemptAt_ne_cancelled {script : List Outcome} (h : Outcome.cancelled ∉ script)
    (n : Nat) : attemptAt script n ≠ .cancelled := by
  intro ha
  unfold Flow.attemptAt at ha
  cases hl : script[min (n - 1) (script.length - 1)]? with
  | none => simp [hl] at ha
  | some o =>
    simp only [hl, Option.getD_some] at ha
    exact h (ha ▸ List.mem_of_getElem? hl)

theorem script_ne_cancelled {node : SpecNode}
    (h : ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script) (ordinal : Nat) :
    Outcome.cancelled ∉ node.script ordinal := by
  unfold SpecNode.script
  cases hl : node.outcomes[min ordinal (node.outcomes.length - 1)]? with
  | none => simp
  | some script => exact h script (List.mem_of_getElem? hl)

theorem Quiet.attemptAt (hq : Quiet c) {node : SpecNode} (hn : node ∈ c.nodes) (ordinal n : Nat) :
    attemptAt (node.script ordinal) n ≠ .cancelled :=
  attemptAt_ne_cancelled (script_ne_cancelled (hq.scripts node hn) ordinal) n

/-! ## A log without stops -/

def Entry.isStop : Entry → Bool
  | .stop _ _ => true
  | _ => false

/-- A log with no stop in it. -/
def NoStop (log : List Entry) : Prop :=
  ∀ e ∈ log, e.isStop = false

theorem NoStop.cancelledIn {log : List Entry} (h : NoStop log) (node : Nat) :
    cancelledIn c log node = false := by
  unfold Control.cancelledIn
  rw [List.any_eq_false]
  intro e he
  have := h e he
  cases e <;> simp_all [Entry.isStop]

theorem NoStop.killedIn {log : List Entry} (h : NoStop log) (node : Nat) :
    killedIn c log node = false := by
  unfold Control.killedIn
  rw [List.any_eq_false]
  intro e he
  have := h e he
  cases e <;> simp_all [Entry.isStop]

theorem NoStop.rootStopped {log : List Entry} (h : NoStop log) : rootStopped log = false := by
  unfold Control.rootStopped
  rw [List.any_eq_false]
  intro e he
  have := h e he
  cases e <;> simp_all [Entry.isStop]

theorem NoStop.push {log : Log c} (h : NoStop log.entries) {e : Entry} (he : e.isStop = false)
    (hv : Valid c e log.entries) : NoStop (log.push e hv).entries := by
  intro x hx
  simp only [Log.push, List.mem_cons] at hx
  rcases hx with rfl | hx
  · exact he
  · exact h x hx

/-! ## Firing numbers -/

/-- The firing number each start takes, in order, from each node's count in
`base`. -/
def number : List Nat → List Key → List (Key × Nat)
  | _, [] => []
  | base, k :: rest => (k, base[k.1]?.getD 0) :: number (base.set k.1 (base[k.1]?.getD 0 + 1)) rest

/-- Each node's count after those starts. -/
def tally : List Nat → List Key → List Nat
  | base, [] => base
  | base, k :: rest => tally (base.set k.1 (base[k.1]?.getD 0 + 1)) rest

/-- A delivery numbers the firings it starts from the counts before it. -/
theorem flow_deliver_number (v : Flow.Case) :
    ∀ (ts : List Flow.Token) (fs : Flow.State),
      (Flow.deliver v fs ts).2 = number fs.firings ((Flow.deliver v fs ts).2.map (·.1)) ∧
        (Flow.deliver v fs ts).1.firings = tally fs.firings ((Flow.deliver v fs ts).2.map (·.1))
  | [], fs => by simp [Flow.deliver, number, tally]
  | t :: rest, fs => by
    unfold Flow.deliver
    split
    · exact flow_deliver_number v rest fs
    · dsimp only
      split
      · exact flow_deliver_number v rest _
      · split
        · exact flow_deliver_number v rest _
        · obtain ⟨h1, h2⟩ := flow_deliver_number v rest
            ((fs.setKey (t.target, t.generation) _).bump t.target)
          simp only [List.map_cons, number, tally]
          refine ⟨?_, h2⟩
          conv => lhs; rw [h1]
          rfl

/-! ## Deliveries -/

/-- The flow case a control case runs as, with `choices` as its schedule. -/
def Case.view (c : Case) (choices : List Nat) : Flow.Case :=
  (Spec.mk c.nodes choices).view

theorem incoming_view (choices : List Nat) : Flow.incoming (c.view choices) = Flow.incoming c.flow :=
  rfl

theorem view_node (choices : List Nat) (n : Nat) :
    (c.view choices).nodes[n]? = (c.nodes[n]?).map fun node =>
      { join := node.join, maxFirings := node.maxFirings, groups := node.groups } := by
  simp [Case.view, Spec.view]

theorem key_eq {S : State c} {F : Flow.State} (h : S.keys = F.keys) (k : Key) :
    S.key k = F.key k := by
  simp [State.key, Flow.State.key, h]

@[simp] theorem firingsOf_setKey (S : State c) (k : Key) (v : Join.Key) (n : Nat) :
    (S.setKey k v).firingsOf n = S.firingsOf n :=
  rfl

@[simp] theorem firingsOf_markTainted (S : State c) (k : Key) (b : Bool) (n : Nat) :
    (S.markTainted k b).firingsOf n = S.firingsOf n :=
  rfl

@[simp] theorem flow_firingsOf_setKey (F : Flow.State) (k : Key) (v : Join.Key) (n : Nat) :
    (F.setKey k v).firingsOf n = F.firingsOf n :=
  rfl

theorem firingsOf_eq {S : State c} {F : Flow.State} (h : S.firings = F.firings) (n : Nat) :
    S.firingsOf n = F.firingsOf n := by
  simp [State.firingsOf, Flow.State.firingsOf, h]

/-- A flow token, as the control model carries it: from a firing that did
not record `cancelled`. -/
def lift (t : Flow.Token) : Token :=
  ⟨t.target, t.generation, t.edge, false⟩

/-- The live firings a delivery's starts add, numbered from `id`, before
their admission. -/
def fresh : Nat → List Key → List Live
  | _, [] => []
  | id, k :: rest =>
    { id, key := k, ordinal := 0, attempt := 1, admitted := false, waiting := false,
      signalled := false } :: fresh (id + 1) rest

/-- One token, with no stop in the log and no cancelled work: as the flow
model delivers it, and a start adds a live firing whose admission it
raises. -/
theorem deliverOne_quiet {S : State c} {t : Flow.Token} {node : SpecNode}
    (hn : c.nodes[t.target]? = some node) (hs : NoStop S.log.entries) (ht : S.tainted = []) :
    ∃ log : Log c, NoStop log.entries ∧
      deliverOne S (lift t) =
        if !(Join.arrive node.join (Flow.incoming c.flow t.target) (S.key (t.target, t.generation))
            t.edge).2 then
          (S.setKey (t.target, t.generation)
            (Join.arrive node.join (Flow.incoming c.flow t.target) (S.key (t.target, t.generation))
              t.edge).1, [])
        else if node.maxFirings ≤ S.firingsOf t.target then
          ({ S.setKey (t.target, t.generation)
              (Join.arrive node.join (Flow.incoming c.flow t.target)
                (S.key (t.target, t.generation)) t.edge).1 with
              budgetExceeded := S.budgetExceeded ++ [t.target] }, [])
        else
          ({ (S.setKey (t.target, t.generation)
              (Join.arrive node.join (Flow.incoming c.flow t.target)
                (S.key (t.target, t.generation)) t.edge).1).bump t.target with
              log
              live := S.live ++ fresh S.nextId [(t.target, t.generation)]
              nextId := S.nextId + 1 }, [.admit S.nextId]) := by
  unfold deliverOne
  split
  · rename_i hnone
    simp [lift, hn] at hnone
  · rename_i node' hn'
    have hnode : node' = node := Option.some.inj (hn'.symm.trans hn)
    subst hnode
    simp only [lift, hs.killedIn, Bool.false_eq_true, ↓reduceDIte, ht, List.contains_nil,
      Bool.false_and, Bool.or_false, hs.cancelledIn, Bool.not_false, Bool.true_or]
    cases hstep : (Join.arrive node'.join (Flow.incoming c.flow t.target)
      (S.key (t.target, t.generation)) t.edge).2
    · exact ⟨S.log, hs, by simp⟩
    · by_cases hroom : node'.maxFirings ≤ S.firingsOf t.target
      · exact ⟨S.log, hs, by simp [hroom, ht]⟩
      · refine ⟨?log, ?hns, ?heq⟩
        case heq =>
          simp only [hroom, firingsOf_markTainted, firingsOf_setKey, ↓reduceIte, Bool.not_true,
            Bool.false_eq_true]
          rfl
        case hns => exact hs.push rfl _

theorem deliverOne_none {S : State c} {t : Token} (hn : c.nodes[t.target]? = none) :
    deliverOne S t = (S, []) := by
  unfold deliverOne
  split
  · rfl
  · rename_i hn'
    rw [hn] at hn'
    cases hn'

/-- A delivery with no stop in the log and no cancelled work: the flow
model's, where each start adds a live firing and raises its admission. -/
theorem deliver_quiet (ch : List Nat) : ∀ (ts : List Flow.Token) (S : State c) (F : Flow.State),
    NoStop S.log.entries → S.tainted = [] → S.keys = F.keys → S.firings = F.firings →
      S.budgetExceeded = F.budgetExceeded →
      ∃ log : Log c, NoStop log.entries ∧
        deliver S (ts.map lift) =
          ({ S with
              log
              keys := (Flow.deliver (c.view ch) F ts).1.keys
              firings := (Flow.deliver (c.view ch) F ts).1.firings
              budgetExceeded := (Flow.deliver (c.view ch) F ts).1.budgetExceeded
              nextId := S.nextId + (Flow.deliver (c.view ch) F ts).2.length
              live := S.live ++ fresh S.nextId ((Flow.deliver (c.view ch) F ts).2.map (·.1)) },
            (fresh S.nextId ((Flow.deliver (c.view ch) F ts).2.map (·.1))).map fun f => .admit f.id)
  | [], S, F, hs, _, hk, hf, hb => ⟨S.log, hs, by
      cases S
      simp_all [deliver, Flow.deliver, fresh]⟩
  | t :: rest, S, F, hs, ht, hk, hf, hb => by
    simp only [List.map_cons, deliver]
    unfold Flow.deliver
    cases hn : c.nodes[t.target]? with
    | none =>
      have hv : (c.view ch).nodes[t.target]? = none := by simp [view_node, hn]
      simp only [hv, deliverOne_none (S := S) (t := lift t) (by simpa [lift] using hn),
        List.nil_append]
      exact deliver_quiet ch rest S F hs ht hk hf hb
    | some node =>
      have hv : (c.view ch).nodes[t.target]? = some
          { join := node.join, maxFirings := node.maxFirings, groups := node.groups } := by
        simp [view_node, hn]
      obtain ⟨log₁, hs₁, h₁⟩ := deliverOne_quiet (S := S) (t := t) hn hs ht
      simp only [hv, h₁, incoming_view, key_eq hk, firingsOf_eq hf, flow_firingsOf_setKey]
      by_cases hstep : (Join.arrive node.join (Flow.incoming c.flow t.target)
        (F.key (t.target, t.generation)) t.edge).2 = true
      rotate_left
      · simp only [hstep, Bool.not_false, ↓reduceIte, List.nil_append]
        exact deliver_quiet ch rest _ _ hs ht (by simp [Flow.State.setKey, hk]) hf hb
      · by_cases hroom : node.maxFirings ≤ F.firingsOf t.target
        · simp only [hstep, Bool.not_true, Bool.false_eq_true, ↓reduceIte, hroom, List.nil_append]
          exact deliver_quiet ch rest _ _ hs ht (by simp [Flow.State.setKey, hk]) hf
            (by simp [hb, Flow.State.setKey])
        · simp only [hstep, Bool.not_true, Bool.false_eq_true, ↓reduceIte, hroom,
            List.singleton_append]
          obtain ⟨log₂, hs₂, h₂⟩ := deliver_quiet ch rest
            { (S.setKey (t.target, t.generation) (Join.arrive node.join (Flow.incoming c.flow t.target)
                (F.key (t.target, t.generation)) t.edge).1).bump t.target with
              log := log₁
              live := S.live ++ fresh S.nextId [(t.target, t.generation)]
              nextId := S.nextId + 1 }
            ((F.setKey (t.target, t.generation) (Join.arrive node.join (Flow.incoming c.flow t.target)
              (F.key (t.target, t.generation)) t.edge).1).bump t.target) hs₁ ht
            (by simp [Flow.State.setKey, Flow.State.bump, hk])
            (by simp [Flow.State.bump, Flow.State.setKey, State.firingsOf,
              Flow.State.firingsOf, hf])
            (by simp [hb, Flow.State.setKey, Flow.State.bump])
          refine ⟨log₂, hs₂, ?_⟩
          rw [h₂]
          simp only [List.map_cons, List.length_cons, fresh,
            List.append_assoc, List.cons_append, List.nil_append]
          refine Prod.ext ?_ rfl
          simp only [Nat.add_assoc, Nat.add_comm 1]

/-! ## Admissions -/

/-- Fresh firings admitted in order, each fixing its number from `started`. -/
def admitList : List Nat → List Live → List Live
  | _, [] => []
  | started, f :: rest =>
    { f with ordinal := started[f.key.1]?.getD 0, admitted := true } ::
      admitList (started.set f.key.1 (started[f.key.1]?.getD 0 + 1)) rest

theorem admitList_numbers : ∀ (started : List Nat) (L : List Live),
    (admitList started L).map (fun f => (f.key, f.ordinal)) = number started (L.map (·.key))
  | _, [] => rfl
  | started, f :: rest => by
    simp only [admitList, List.map_cons, number, admitList_numbers _ rest]

/-- Looking a live firing up by its id finds it, when ids are distinct. -/
theorem find?_id : ∀ {l : List Live} {f : Live}, (l.map (·.id)).Nodup → f ∈ l →
    l.find? (·.id == f.id) = some f
  | [], _, _, h => absurd h List.not_mem_nil
  | g :: l, f, hnd, hf => by
    simp only [List.map_cons, List.nodup_cons, List.mem_map, not_exists, not_and] at hnd
    rcases List.mem_cons.mp hf with rfl | hf
    · simp
    · have hne : (g.id == f.id) = false := by simpa using fun h => hnd.1 f hf h.symm
      simp only [List.find?_cons, hne]
      exact find?_id (by simpa using hnd.2) hf

/-- Updating the firings with a given id leaves the others alone. -/
theorem map_other {l : List Live} {id : Nat} (h : ∀ g ∈ l, g.id ≠ id) (u : Live → Live) :
    (l.map fun g => if g.id == id then u g else g) = l := by
  conv => rhs; rw [← List.map_id l]
  apply List.map_congr_left
  intro g hg
  simp [h g hg]

/-- The host admits fresh firings in order, fixing each one's number. -/
theorem decideAll_admits (hq : Quiet c) : ∀ (L old : List Live) (fuel : Nat) (s : State c),
    s.live = old ++ L → ((old ++ L).map (·.id)).Nodup → L.length ≤ fuel →
    decideAll fuel s (L.map fun f => .admit f.id) =
      ({ s with
          live := old ++ admitList s.started L
          started := tally s.started (L.map (·.key))
          used := s.used + L.length }, L.map (·.key))
  | [], old, fuel, s, hl, _, _ => by
    cases fuel <;> (cases s; simp_all [decideAll, admitList, tally])
  | f :: rest, old, fuel + 1, s, hl, hnd, hfuel => by
    have hf : s.live.find? (·.id == f.id) = some f := by
      rw [hl]
      exact find?_id hnd (by simp)
    have hids := hnd
    simp only [List.map_append, List.map_cons, List.nodup_append, List.nodup_cons, List.mem_cons,
      List.mem_map] at hids
    have hold : ∀ g ∈ old, g.id ≠ f.id := fun g hg h =>
      hids.2.2 g.id ⟨g, hg, rfl⟩ f.id (Or.inl rfl) h
    have hrest : ∀ g ∈ rest, g.id ≠ f.id := fun g hg h => hids.2.1.1 ⟨g, hg, h⟩
    simp only [List.map_cons, decideAll, answer, hf, hq.admit, List.nil_append,
      List.singleton_append]
    rw [decideAll_admits hq rest (old ++ [{ f with ordinal := s.startedOf f.key.1, admitted := true }])
      fuel _ ?live ?nodup (by simpa using hfuel)]
    case live =>
      simp only [hl, List.map_append, List.map_cons, map_other hold,
        map_other hrest, beq_self_eq_true, ↓reduceIte, List.append_assoc, List.singleton_append,
        State.startedOf]
    case nodup => simpa using hnd
    simp only [admitList, tally, State.startedOf, List.append_assoc,
      List.singleton_append, List.length_cons, Prod.mk.injEq, and_true]
    congr 1
    omega

/-! ## Routings -/

/-- Routing groups the host answers with the core's pick route what the flow
model routes. -/
theorem routeTokens_quiet (hq : Quiet c) (k : Key) (status : Status) :
    ∀ (groups : List (List Flow.Arm)) (s : State c),
      routeTokens s k status groups =
        ({ s with used := s.used + groups.length },
          (groups.filterMap (Flow.emit status)).map (tokenOf k status))
  | [], s => by simp [routeTokens]
  | arms :: rest, s => by
    simp only [routeTokens, pickArm, hq.pick, routeTokens_quiet hq k status rest]
    cases h : Flow.emit status arms <;>
      simp [h, Nat.add_assoc, Nat.add_comm 1]

theorem tokens_quiet (ch : List Nat) (k : Key) {status : Status} (hst : status ≠ .cancelled) :
    (((c.nodes[k.1]?.map (·.groups)).getD []).filterMap (Flow.emit status)).map (tokenOf k status) =
      (Flow.tokensOf (c.view ch) k status).map lift := by
  have hc : (status == .cancelled) = false := by simpa using hst
  cases hn : c.nodes[k.1]? <;> simp [Flow.tokensOf, view_node, hn, tokenOf, lift, hc]

theorem fresh_keys : ∀ (id : Nat) (ks : List Key), (fresh id ks).map (·.key) = ks
  | _, [] => rfl
  | id, k :: rest => by simp [fresh, fresh_keys (id + 1) rest]

theorem fresh_ids : ∀ (id : Nat) (ks : List Key), (fresh id ks).map (·.id) = List.range' id ks.length
  | _, [] => rfl
  | id, k :: rest => by simp [fresh, fresh_ids (id + 1) rest, List.range'_succ]

theorem fresh_length (id : Nat) (ks : List Key) : (fresh id ks).length = ks.length := by
  simpa using congrArg List.length (fresh_keys id ks)

/-- Fresh ids, numbered from `nextId`, are distinct from the live ones below it. -/
theorem fresh_nodup {live : List Live} {nextId : Nat} (hnd : (live.map (·.id)).Nodup)
    (hlt : ∀ g ∈ live, g.id < nextId) (ks : List Key) :
    ((live ++ fresh nextId ks).map (·.id)).Nodup := by
  rw [List.map_append, fresh_ids, List.nodup_append]
  refine ⟨hnd, List.nodup_range', fun a ha b hb hab => ?_⟩
  obtain ⟨g, hg, rfl⟩ := List.mem_map.mp ha
  have := hlt g hg
  simp only [List.mem_range'_1] at hb
  omega

/-- A recorded status other than `cancelled`, handed to a host that answers at
once: it routes as the flow model routes it, and the host admits what that
starts, in order. -/
theorem raise_route (hq : Quiet c) (ch : List Nat) (S : State c) (F : Flow.State) (k : Key)
    (st : Status) (hst : st ≠ .cancelled) (hs : NoStop S.log.entries) (ht : S.tainted = [])
    (hk : S.keys = F.keys) (hf : S.firings = F.firings) (hb : S.budgetExceeded = F.budgetExceeded)
    (hstarted : S.started = F.firings) (hids : (S.live.map (·.id)).Nodup)
    (hlt : ∀ g ∈ S.live, g.id < S.nextId)
    (hfuel : (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).2.length + 1 ≤
      routeFuel c S) :
    ∃ (log : Log c) (used : Nat), NoStop log.entries ∧
      raise S [.route ⟨k, st⟩] =
        ({ S with
            log, used
            keys := (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).1.keys
            firings := (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).1.firings
            started := (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).1.firings
            budgetExceeded :=
              (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).1.budgetExceeded
            nextId := S.nextId + (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).2.length
            live := S.live ++ admitList F.firings (fresh S.nextId
              ((Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).2.map (·.1))) },
          (Flow.deliver (c.view ch) F (Flow.tokensOf (c.view ch) k st)).2.map (·.1)) := by
  obtain ⟨fuel, hfuel'⟩ : ∃ fuel, routeFuel c S = fuel + 1 := ⟨routeFuel c S - 1, by omega⟩
  obtain ⟨log₂, hs₂, h₂⟩ := deliver_quiet ch (Flow.tokensOf (c.view ch) k st)
    { S with
      log := S.log.push (.routed k.1) (by simpa [Valid] using hs.killedIn k.1)
      used := S.used + ((c.nodes[k.1]?.map (·.groups)).getD []).length } F
    (hs.push rfl (by simpa [Valid] using hs.killedIn k.1)) ht hk hf hb
  refine ⟨log₂, ?used, hs₂, ?eq⟩
  case eq =>
    unfold raise
    rw [hq.holds, hfuel']
    simp only [Bool.false_eq_true, ↓reduceIte]
    unfold decideAll
    simp only [answer, hs.killedIn, Bool.false_eq_true, ↓reduceDIte, routeTokens_quiet hq,
      tokens_quiet ch k hst, List.append_nil, List.nil_append]
    rw [h₂]
    simp only
    rw [decideAll_admits hq _ S.live fuel _ rfl (fresh_nodup hids hlt _)
      (by rw [fresh_length, List.length_map]; omega)]
    simp only [fresh_keys, hstarted, fresh_length, List.length_map]
    rw [← (flow_deliver_number (c.view ch) _ F).2]

/-! ## Attempts -/

theorem retryOf_of {n : Nat} {node : SpecNode} (h : c.nodes[n]? = some node) :
    retryOf c n = node.retry := by
  simp [retryOf, h]

theorem report_of {f : Live} {node : SpecNode} (h : c.nodes[f.key.1]? = some node)
    (hs : f.signalled = false) : report c f = attemptAt (node.script f.ordinal) f.attempt := by
  simp [report, h, hs]

theorem filter_map_update (f g : Live) (h : g.id = f.id) : ∀ (l : List Live),
    (l.map fun x => if x.id == g.id then g else x).filter (·.id != f.id) = l.filter (·.id != f.id)
  | [] => rfl
  | x :: xs => by
    have ih := filter_map_update f g h xs
    simp only [h, beq_iff_eq] at ih
    by_cases hx : x.id = f.id <;> simp [hx, h, ih]

theorem filter_update (s : State c) (f g : Live) (h : g.id = f.id) :
    (s.update g).live.filter (·.id != f.id) = s.live.filter (·.id != f.id) :=
  filter_map_update f g h s.live

theorem finalFrom_ge (r : Retry) (script : List Outcome) :
    ∀ (fuel n : Nat), n ≤ (finalFrom r script n fuel).1
  | 0, _ => Nat.le_refl _
  | fuel + 1, n => by
    simp only [finalFrom]
    split
    · exact Nat.le_trans (Nat.le_succ n) (finalFrom_ge r script fuel (n + 1))
    · exact Nat.le_refl _

theorem finalFrom_outcome (r : Retry) (script : List Outcome) :
    ∀ (fuel n : Nat), (finalFrom r script n fuel).2 = attemptAt script (finalFrom r script n fuel).1
  | 0, _ => rfl
  | fuel + 1, n => by
    simp only [finalFrom]
    split
    · exact finalFrom_outcome r script fuel (n + 1)
    · rfl

/-- The retries a firing of `k` schedules from attempt `a` up to its last
attempt `count`, each with its base delay. -/
def retriesFrom (k : Key) (r : Retry) (a count : Nat) : List (Nat × Nat × Nat × Nat) :=
  (List.range (count - a)).map fun i => (k.1, k.2, a + i + 1, baseDelay r (a + i))

theorem retriesFrom_succ (k : Key) (r : Retry) {a count : Nat} (h : a < count) :
    retriesFrom k r a count = (k.1, k.2, a + 1, baseDelay r a) :: retriesFrom k r (a + 1) count := by
  unfold retriesFrom
  obtain ⟨m, hm⟩ : ∃ m, count - a = m + 1 := ⟨count - a - 1, by omega⟩
  have hm' : count - (a + 1) = m := by omega
  rw [hm, hm', List.range_succ_eq_map]
  simp only [List.map_cons, List.map_map, Function.comp_def, Nat.add_zero]
  congr 2
  funext i
  rw [show a + i.succ = a + 1 + i by omega]

/-- The host runs a firing's attempts, with no stop in the log and every
admission answered with the default: as the retry model does, it retries
until an attempt is final, then records it and hands its routing on. -/
theorem runAttempts_quiet (hq : Quiet c) {node : SpecNode} :
    ∀ (fuel fuel' : Nat) (s : State c) (f : Live),
      c.nodes[f.key.1]? = some node → f.signalled = false → NoStop s.log.entries →
      node.retry.limit - f.attempt ≤ fuel' → fuel' < fuel →
      ∃ (log : Log c) (used : Nat), NoStop log.entries ∧
        runAttempts true fuel s f =
          raise { s with
              log, used
              live := s.live.filter (·.id != f.id)
              finished := s.finished ++ [f.key]
              attempts := s.attempts ++ [(f.key.1, f.key.2,
                (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').1,
                (recorded node.retry (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').1
                  (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').2).tag)]
              retries := s.retries ++ retriesFrom f.key node.retry f.attempt
                (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').1
              failed := s.failed ||
                (recorded node.retry (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').1
                  (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').2).isFailure }
            [.route ⟨f.key, recorded node.retry
              (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').1
              (finalFrom node.retry (node.script f.ordinal) f.attempt fuel').2⟩]
  | 0, _, _, _, _, _, _, _, h => absurd h (Nat.not_lt_zero _)
  | fuel + 1, fuel', s, f, hn, hsig, hs, hlim, hfuel => by
    unfold runAttempts
    simp only [retryOf_of hn, report_of hn hsig, hsig, hs.cancelledIn, Bool.false_or,
      Bool.not_false, Bool.true_and]
    split
    · rename_i hr
      have ha : admitVerdict ((c.verdicts[s.used]?).getD 0) = .admit := hq.admit s
      simp only [↓reduceIte, ha]
      have hlt : f.attempt < node.retry.limit := by
        simp only [Bool.and_eq_true, decide_eq_true_eq] at hr
        exact hr.2
      obtain ⟨fuel'', rfl⟩ : ∃ n, fuel' = n + 1 := ⟨fuel' - 1, by omega⟩
      have hfin : finalFrom node.retry (node.script f.ordinal) f.attempt (fuel'' + 1) =
          finalFrom node.retry (node.script f.ordinal) (f.attempt + 1) fuel'' := by
        simp only [finalFrom]
        simp only [hr, ↓reduceIte]
      have hcount : f.attempt <
          (finalFrom node.retry (node.script f.ordinal) (f.attempt + 1) fuel'').1 :=
        Nat.lt_of_lt_of_le (Nat.lt_succ_self _) (finalFrom_ge _ _ _ _)
      obtain ⟨log', used', hs', h'⟩ := runAttempts_quiet hq fuel fuel''
        (({ s with
            log := s.log.push (.retried f.key.1) (hs.cancelledIn f.key.1)
            retries := s.retries ++ [(f.key.1, f.key.2, f.attempt + 1, baseDelay node.retry f.attempt)]
            used := s.used + 1 } : State c).update { f with attempt := f.attempt + 1, signalled := false })
        { f with attempt := f.attempt + 1, signalled := false } hn rfl
        (hs.push (e := .retried f.key.1) rfl (hs.cancelledIn f.key.1)) (by simp only; omega)
        (by omega)
      refine ⟨log', used', hs', ?_⟩
      rw [h', hfin, retriesFrom_succ f.key node.retry hcount]
      rw [filter_update _ f { f with attempt := f.attempt + 1, signalled := false } rfl]
      simp only [State.update, List.append_assoc, List.singleton_append]
    · rename_i hr
      have hfin : finalFrom node.retry (node.script f.ordinal) f.attempt fuel' =
          (f.attempt, attemptAt (node.script f.ordinal) f.attempt) := by
        cases fuel' with
        | zero => rfl
        | succ n =>
          simp only [finalFrom]
          simp only [hr, Bool.false_eq_true, ↓reduceIte]
      refine ⟨s.log, s.used, hs, ?_⟩
      simp [hfin, hs.killedIn, retriesFrom]

/-! ## Live firings, as the flow model lists them -/

/-- A live firing as the flow model tracks it: its key and its number. -/
def Live.flow (f : Live) : Key × Nat :=
  (f.key, f.ordinal)

@[simp] theorem Live.flow_fst (f : Live) : f.flow.1 = f.key :=
  rfl

@[simp] theorem Live.flow_snd (f : Live) : f.flow.2 = f.ordinal :=
  rfl

theorem keyLe_trans (a b d : Key × Nat) : Flow.keyLe a b = true → Flow.keyLe b d = true →
    Flow.keyLe a d = true := by
  simp only [Flow.keyLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

theorem keyLe_total (a b : Key × Nat) : (Flow.keyLe a b || Flow.keyLe b a) = true := by
  simp only [Flow.keyLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  omega

theorem keyLe_antisymm {a b : Key × Nat} : Flow.keyLe a b = true → Flow.keyLe b a = true →
    a.1 = b.1 := by
  simp only [Flow.keyLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq, beq_iff_eq]
  intro h₁ h₂
  exact Prod.ext (by omega) (by omega)

/-- Two members of a list whose keys are distinct are equal when their keys
are. -/
theorem eq_of_key : ∀ {l : List (Key × Nat)}, (l.map (·.1)).Nodup → ∀ {a b : Key × Nat},
    a ∈ l → b ∈ l → a.1 = b.1 → a = b
  | [], _, _, _, ha, _, _ => absurd ha List.not_mem_nil
  | x :: xs, hnd, a, b, ha, hb, h => by
    simp only [List.map_cons, List.nodup_cons, List.mem_map, not_exists, not_and] at hnd
    rcases List.mem_cons.mp ha with rfl | ha' <;> rcases List.mem_cons.mp hb with rfl | hb'
    · rfl
    · exact absurd h.symm (hnd.1 b hb')
    · exact absurd h (hnd.1 a ha')
    · exact eq_of_key hnd.2 ha' hb' h

/-- Sorting by key a permutation of a sorted list whose keys are distinct
gives that list. -/
theorem mergeSort_eq_of_perm {l l' : List (Key × Nat)} (h : l.Perm l') (hnd : (l.map (·.1)).Nodup)
    (hs : l'.Pairwise (Flow.keyLe · ·)) : l.mergeSort Flow.keyLe = l' := by
  have hsort := List.mergeSort_perm l Flow.keyLe
  refine List.Perm.eq_of_pairwise (fun a b ha hb hab hba => ?_)
    (List.pairwise_mergeSort keyLe_trans keyLe_total l) hs (hsort.trans h)
  exact eq_of_key hnd (hsort.mem_iff.mp ha) (h.mem_iff.mpr hb) (keyLe_antisymm hab hba)

/-- Removing a live firing by id, when ids are distinct. -/
theorem perm_filter_id : ∀ {l : List Live} {f : Live}, f ∈ l → (l.map (·.id)).Nodup →
    l.Perm (f :: l.filter (·.id != f.id))
  | [], _, hf, _ => absurd hf List.not_mem_nil
  | g :: l, f, hf, hnd => by
    simp only [List.map_cons, List.nodup_cons, List.mem_map, not_exists, not_and] at hnd
    rcases List.mem_cons.mp hf with rfl | hf
    · have hall : l.filter (·.id != f.id) = l :=
        List.filter_eq_self.mpr fun x hx => by simpa using fun h => hnd.1 x hx h
      simp [hall]
    · have hne : g.id ≠ f.id := fun h => hnd.1 f hf h.symm
      have ih := perm_filter_id hf hnd.2
      simp only [List.filter_cons, bne_iff_ne, ne_eq, hne, not_false_eq_true,
        ↓reduceIte]
      exact (ih.cons g).trans (List.Perm.swap f g _)

theorem admitList_ids : ∀ (started : List Nat) (L : List Live),
    (admitList started L).map (·.id) = L.map (·.id)
  | _, [] => rfl
  | started, f :: rest => by simp [admitList, admitList_ids _ rest]

theorem admitList_keys : ∀ (started : List Nat) (L : List Live),
    (admitList started L).map (·.key) = L.map (·.key)
  | _, [] => rfl
  | started, f :: rest => by simp [admitList, admitList_keys _ rest]

/-- A fresh firing, once admitted: on its first attempt, admitted, neither
waiting nor signalled. -/
def Live.Ready (f : Live) : Prop :=
  f.attempt = 1 ∧ f.admitted = true ∧ f.waiting = false ∧ f.signalled = false

theorem admitList_fresh_ready : ∀ (started : List Nat) (id : Nat) (ks : List Key),
    ∀ f ∈ admitList started (fresh id ks), f.Ready
  | _, _, [], f, hf => absurd hf List.not_mem_nil
  | started, id, k :: rest, f, hf => by
    simp only [fresh, admitList, List.mem_cons] at hf
    rcases hf with rfl | hf
    · exact ⟨rfl, rfl, rfl, rfl⟩
    · exact admitList_fresh_ready _ _ rest f hf

/-- The firings a flow delivery starts are at nodes the case has. -/
theorem flow_deliver_nodes (v : Flow.Case) : ∀ (ts : List Flow.Token) (fs : Flow.State),
    ∀ x ∈ (Flow.deliver v fs ts).2, ∃ node, v.nodes[x.1.1]? = some node
  | [], _, x, hx => by simp [Flow.deliver] at hx
  | t :: rest, fs, x, hx => by
    unfold Flow.deliver at hx
    split at hx
    · exact flow_deliver_nodes v rest fs x hx
    · rename_i node hn
      dsimp only at hx
      split at hx
      · exact flow_deliver_nodes v rest _ x hx
      · split at hx
        · exact flow_deliver_nodes v rest _ x hx
        · rcases List.mem_cons.mp hx with rfl | hx
          · exact ⟨node, hn⟩
          · exact flow_deliver_nodes v rest _ x hx

theorem recorded_ne_cancelled (r : Retry) (n : Nat) {o : Outcome} (h : o ≠ .cancelled) :
    recorded r n o ≠ .cancelled := by
  unfold Flow.recorded
  split
  · simp
  · cases o <;> simp_all [Flow.Outcome.status]

/-! ## The two states agree -/

/-- How many attempts a firing takes, as `Spec.run` counts them. -/
def Case.count (c : Case) (node ordinal : Nat) : Nat :=
  match c.nodes[node]? with
  | none => 1
  | some n => (Flow.attempts n.retry (n.script ordinal)).1

/-- A retry's base delay, as `Spec.run` reads it. -/
def Case.delay (c : Case) (node failed : Nat) : Nat :=
  match c.nodes[node]? with
  | none => 0
  | some n => baseDelay n.retry failed

/-- What `Spec.run` lists for a finished firing: its attempts and its
recorded status. -/
def attemptOf (c : Case) (ch : List Nat) (firing : Key × Nat) : Nat × Nat × Nat × String :=
  (firing.1.1, firing.1.2, c.count firing.1.1 firing.2, ((c.view ch).record firing.1.1 firing.2).tag)

/-- The retries `Spec.run` lists for a finished firing. -/
def retriesOf (c : Case) (firing : Key × Nat) : List (Nat × Nat × Nat × Nat) :=
  (List.range (c.count firing.1.1 firing.2 - 1)).map fun i =>
    (firing.1.1, firing.1.2, i + 2, c.delay firing.1.1 (i + 1))

/-- Whether a finished firing's record fails the run. -/
def failedOf (c : Case) (ch : List Nat) (firing : Key × Nat) : Bool :=
  ((c.view ch).record firing.1.1 firing.2).isFailure

/-- A state of the control model and one of the flow model that the host
cannot tell apart, and that step alike: the same keys, counts, steps and
records; the same live firings, all admitted on their first attempt; and no
stop, taint or open decision. -/
structure Agrees (c : Case) (ch : List Nat) (fs : Flow.State) (cs : State c) : Prop where
  log : NoStop cs.log.entries
  keys : cs.keys = fs.keys
  tainted : cs.tainted = []
  firings : cs.firings = fs.firings
  started : cs.started = fs.firings
  held : cs.held = []
  live : (cs.live.map Live.flow).Perm fs.live
  sorted : fs.live.Pairwise (Flow.keyLe · ·)
  distinct : (fs.live.map (·.1)).Nodup
  fired : ∀ x ∈ fs.live, (fs.key x.1).fired = true
  ready : ∀ f ∈ cs.live, f.Ready
  nodes : ∀ f ∈ cs.live, ∃ node, c.nodes[f.key.1]? = some node
  ids : (cs.live.map (·.id)).Nodup
  below : ∀ f ∈ cs.live, f.id < cs.nextId
  steps : cs.steps = fs.steps
  finished : cs.finished = fs.finished.map (·.1)
  budgetExceeded : cs.budgetExceeded = fs.budgetExceeded
  completed : cs.completed = []
  controls : cs.controls = []
  attempts : cs.attempts = fs.finished.map (attemptOf c ch)
  retries : cs.retries = fs.finished.flatMap (retriesOf c)
  failed : cs.failed = fs.finished.any (failedOf c ch)
  budget : Flow.WithinBudget (c.view ch) fs
  length : fs.firings.length = (c.view ch).nodes.length

/-- The host lists the live firings as the flow model does: sorted by key. -/
theorem Agrees.byKey {ch : List Nat} {fs : Flow.State} {cs : State c} (h : Agrees c ch fs cs) :
    ((cs.live.filter (·.admitted)).mergeSort fun a b => keyLe a.key b.key).map Live.flow =
      fs.live := by
  have hall : cs.live.filter (·.admitted) = cs.live :=
    List.filter_eq_self.mpr fun f hf => (h.ready f hf).2.1
  rw [hall, List.map_mergeSort (r := fun a b => keyLe a.key b.key) (s := Flow.keyLe)
    (f := Live.flow) (fun _ _ _ _ => rfl)]
  exact mergeSort_eq_of_perm h.live (h.live.map (·.1) |>.nodup_iff.mpr h.distinct) h.sorted

/-! ## One host step -/

/-- What a firing records, with no stop and scripts that never report
`cancelled`, is not `cancelled`. -/
theorem record_ne_cancelled (hq : Quiet c) (ch : List Nat) {n : Nat} {node : SpecNode}
    (hn : c.nodes[n]? = some node) (ordinal : Nat) :
    (c.view ch).record n ordinal ≠ .cancelled := by
  simp only [Case.view, Spec.view, Flow.Spec.record, hn]
  apply recorded_ne_cancelled
  rw [Flow.attempts, finalFrom_outcome]
  exact hq.attemptAt (List.mem_of_getElem? hn) _ _

theorem budget_view (ch : List Nat) :
    ((c.view ch).nodes.map (·.maxFirings)).sum = budget c := by
  simp [Case.view, Spec.view, budget, Function.comp_def]

/-- The firings the budgets allow bound the firings counted so far. -/
theorem firings_sum_le {v : Flow.Case} {fs : Flow.State} (hb : Flow.WithinBudget v fs)
    (hl : fs.firings.length = v.nodes.length) :
    fs.firings.sum ≤ (v.nodes.map (·.maxFirings)).sum := by
  apply Flow.sum_le_of_le _ _ (by simp [hl])
  intro i hi hm
  have hi' : i < v.nodes.length := by simpa using hm
  have := hb i v.nodes[i] (List.getElem?_eq_getElem hi')
  simp only [Flow.State.firingsOf, List.getElem?_eq_getElem hi, Option.getD_some] at this
  simpa using this

/-- A delivery starts no more firings than the budgets allow in all. -/
theorem deliver_le_budget (ch : List Nat) {F : Flow.State} (hb : Flow.WithinBudget (c.view ch) F)
    (hl : F.firings.length = (c.view ch).nodes.length) (ts : List Flow.Token) :
    (Flow.deliver (c.view ch) F ts).2.length ≤ budget c := by
  obtain ⟨_, _, hlen, hsum⟩ := Flow.deliver_counts (c.view ch) ts F hl
  have := firings_sum_le (Flow.deliver_within_budget (c.view ch) ts F hb) hlen
  rw [budget_view] at this
  omega

/-- The flow state once `firing` finished, before its routing delivers. -/
def retire (fs : Flow.State) (firing : Key × Nat) : Flow.State :=
  { fs with live := fs.live.erase firing, finished := fs.finished ++ [firing] }

theorem finish_retire (v : Flow.Case) (fs : Flow.State) (firing : Key × Nat) :
    Flow.finish v fs firing =
      { (Flow.deliver v (retire fs firing) (Flow.tokensOf v firing.1 (v.record firing.1.1 firing.2))).1 with
        live := ((Flow.deliver v (retire fs firing)
          (Flow.tokensOf v firing.1 (v.record firing.1.1 firing.2))).1.live ++
            (Flow.deliver v (retire fs firing)
              (Flow.tokensOf v firing.1 (v.record firing.1.1 firing.2))).2).mergeSort Flow.keyLe
        steps := (Flow.deliver v (retire fs firing)
          (Flow.tokensOf v firing.1 (v.record firing.1.1 firing.2))).1.steps ++
            [((Flow.deliver v (retire fs firing)
              (Flow.tokensOf v firing.1 (v.record firing.1.1 firing.2))).2.mergeSort
                Flow.keyLe).map (·.1)] } :=
  rfl

/-- A finished firing's routing, handed to the host: the flow model's
`finish`. `S` is the control state once the firing's record is in. -/
theorem route_agrees (hq : Quiet c) {ch : List Nat} {fs : Flow.State} {cs : State c}
    (h : Agrees c ch fs cs) {f : Live} (hmem : f ∈ cs.live) {node : SpecNode}
    (hn : c.nodes[f.key.1]? = some node) (S : State c) (hS : NoStop S.log.entries)
    (hkeys : S.keys = cs.keys) (htainted : S.tainted = cs.tainted)
    (hfirings : S.firings = cs.firings) (hstarted : S.started = cs.started)
    (hheld : S.held = cs.held) (hlive : S.live = cs.live.filter (·.id != f.id))
    (hnext : S.nextId = cs.nextId) (hsteps : S.steps = cs.steps)
    (hfin : S.finished = cs.finished ++ [f.key]) (hbe : S.budgetExceeded = cs.budgetExceeded)
    (hcomp : S.completed = cs.completed) (hctl : S.controls = cs.controls)
    (hatt : S.attempts = cs.attempts ++ [attemptOf c ch f.flow])
    (hret : S.retries = cs.retries ++ retriesOf c f.flow)
    (hfail : S.failed = (cs.failed || failedOf c ch f.flow)) :
    Agrees c ch (Flow.finish (c.view ch) fs f.flow)
      { (raise S [.route ⟨f.key, (c.view ch).record f.key.1 f.ordinal⟩]).1 with
        steps := (raise S [.route ⟨f.key, (c.view ch).record f.key.1 f.ordinal⟩]).1.steps ++
          [sorted (raise S [.route ⟨f.key, (c.view ch).record f.key.1 f.ordinal⟩]).2] } := by
  have hbudget := Flow.within_budget_finish (c.view ch) h.budget f.flow
  obtain ⟨hdlive, hdfin, hdlen, _⟩ := Flow.deliver_counts (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
    (retire fs f.flow) h.length
  have hnum := flow_deliver_number (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
    (retire fs f.flow)
  obtain ⟨hnew, hnewk⟩ := Flow.deliver_started (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
    (retire fs f.flow)
  have hdsteps := Flow.deliver_steps (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
    (retire fs f.flow)
  have hdnodes := flow_deliver_nodes (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
    (retire fs f.flow)
  have hids : (S.live.map (·.id)).Nodup := by
    rw [hlive]
    exact h.ids.sublist ((List.filter_sublist).map _)
  have hbelow : ∀ g ∈ S.live, g.id < S.nextId := fun g hg => by
    rw [hlive] at hg
    rw [hnext]
    exact h.below g (List.mem_filter.mp hg).1
  obtain ⟨log₂, used₂, hs₂, h₂⟩ := raise_route hq ch S
    (retire fs f.flow) f.key
    ((c.view ch).record f.key.1 f.ordinal) (record_ne_cancelled hq ch hn _) hS
    (by rw [htainted]; exact h.tainted) (by rw [hkeys]; exact h.keys)
    (by rw [hfirings]; exact h.firings) (by rw [hbe]; exact h.budgetExceeded)
    (by rw [hstarted]; exact h.started) hids hbelow
    (by
      have := deliver_le_budget ch (F := retire fs f.flow) h.budget h.length
        (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal))
      simp only [routeFuel, budget] at this ⊢
      omega)
  have hmono := Flow.deliver_fired_mono (c.view ch)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal)) (retire fs f.flow)
  rw [h₂, finish_retire]
  rw [finish_retire] at hbudget
  simp only [Live.flow_fst, Live.flow_snd] at hbudget ⊢
  generalize Flow.deliver (c.view ch) (retire fs f.flow)
    (Flow.tokensOf (c.view ch) f.key ((c.view ch).record f.key.1 f.ordinal)) = D
    at hbudget hdlive hdfin hdlen hnum hnew hnewk hdsteps hdnodes hmono ⊢
  have hadm : (admitList (retire fs f.flow).firings (fresh S.nextId (D.2.map (·.1)))).map
      Live.flow = D.2 := by
    rw [show Live.flow = fun f => (f.key, f.ordinal) from rfl, admitList_numbers, fresh_keys]
    exact hnum.1.symm
  have hrest : (S.live.map Live.flow).Perm (fs.live.erase f.flow) := by
    have hp := ((perm_filter_id hmem h.ids).map Live.flow).symm.trans h.live
    have := hp.erase f.flow
    rw [List.map_cons, List.erase_cons_head] at this
    rw [hlive]
    exact this
  refine ⟨hs₂, rfl, ?_, rfl, rfl, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, rfl, ?_, ?_, ?_,
    ?_, ?_, hbudget, hdlen⟩
  · rw [htainted]; exact h.tainted
  · rw [hheld]; exact h.held
  · rw [hdlive, List.map_append, hadm]
    exact (hrest.append_right D.2).trans (List.mergeSort_perm _ _).symm
  · exact List.pairwise_mergeSort keyLe_trans keyLe_total _
  · rw [hdlive]
    apply ((List.mergeSort_perm _ Flow.keyLe).map (·.1)).nodup_iff.mpr
    rw [List.map_append, List.nodup_append]
    refine ⟨h.distinct.sublist ((List.erase_sublist).map _), hnew, fun a ha b hb hab => ?_⟩
    obtain ⟨x, hx, rfl⟩ := List.mem_map.mp ha
    have hfx := h.fired x (List.erase_sublist.subset hx)
    have hu := (hnewk b hb).1
    subst hab
    simp only [retire, Flow.State.key] at hu hfx
    rw [hfx] at hu
    exact absurd hu (by decide)
  · intro x hx
    rw [List.mem_mergeSort, List.mem_append, hdlive] at hx
    rcases hx with hx | hx
    · exact hmono x.1 (h.fired x (List.erase_sublist.subset hx))
    · exact (hnewk x.1 (List.mem_map_of_mem hx)).2
  · intro g hg
    rcases List.mem_append.mp hg with hg | hg
    · rw [hlive] at hg
      exact h.ready g (List.mem_filter.mp hg).1
    · exact admitList_fresh_ready _ _ _ g hg
  · intro g hg
    rcases List.mem_append.mp hg with hg | hg
    · rw [hlive] at hg
      exact h.nodes g (List.mem_filter.mp hg).1
    · have hk : g.key ∈ D.2.map (·.1) := by
        rw [← fresh_keys S.nextId (D.2.map (·.1)), ← admitList_keys (retire fs f.flow).firings]
        exact List.mem_map_of_mem hg
      obtain ⟨x, hx, hxk⟩ := List.mem_map.mp hk
      obtain ⟨n', hn'⟩ := hdnodes x hx
      rw [view_node] at hn'
      cases hc : c.nodes[x.1.1]? with
      | none => simp [hc] at hn'
      | some node' => exact ⟨node', hxk ▸ hc⟩
  · rw [List.map_append, admitList_ids, ← List.map_append]
    exact fresh_nodup hids hbelow _
  · intro g hg
    rcases List.mem_append.mp hg with hg | hg
    · exact Nat.lt_of_lt_of_le (hbelow g hg) (Nat.le_add_right _ _)
    · have hid : g.id ∈ (fresh S.nextId (D.2.map (·.1))).map (·.id) := by
        rw [← admitList_ids (retire fs f.flow).firings]
        exact List.mem_map_of_mem hg
      rw [fresh_ids, List.mem_range'_1, List.length_map] at hid
      show g.id < S.nextId + D.2.length
      omega
  · rw [hdsteps, hsteps, h.steps]
    simp only [retire, sorted]
    rw [List.map_mergeSort (r := Flow.keyLe) (s := keyLe) (f := Prod.fst) (fun _ _ _ _ => rfl)]
  · rw [hdfin, hfin, h.finished]
    simp [retire]
  · rw [hcomp, h.completed]
  · rw [hctl, h.controls]
  · rw [hdfin, hatt, h.attempts]
    simp [retire]
  · rw [hdfin, hret, h.retries]
    simp [retire, List.flatMap_append]
  · rw [hdfin, hfail, h.failed]
    simp [retire, List.any_append]

/-- A `finish` step: the host finishes the firing the flow model's host
finishes, and the two states still agree. -/
theorem finish_agrees (hq : Quiet c) {ch : List Nat} {fs : Flow.State} {cs : State c}
    (h : Agrees c ch fs cs) (choice : Nat) {firing : Key × Nat}
    (hfiring : fs.live[choice % fs.live.length]? = some firing) :
    Agrees c ch (Flow.finish (c.view ch) fs firing) (hostFinish true cs choice) := by
  have hkey := h.byKey
  have hlen : ((cs.live.filter (·.admitted)).mergeSort fun a b => keyLe a.key b.key).length =
      fs.live.length := by
    rw [← hkey, List.length_map]
  obtain ⟨f, hf, rfl⟩ : ∃ f, ((cs.live.filter (·.admitted)).mergeSort
      fun a b => keyLe a.key b.key)[choice % fs.live.length]? = some f ∧ f.flow = firing := by
    have hfiring' : (((cs.live.filter (·.admitted)).mergeSort
        fun a b => keyLe a.key b.key).map Live.flow)[choice % fs.live.length]? = some firing := by
      rw [hkey]
      exact hfiring
    rw [List.getElem?_map] at hfiring'
    cases hb : ((cs.live.filter (·.admitted)).mergeSort
        fun a b => keyLe a.key b.key)[choice % fs.live.length]? with
    | none => simp [hb] at hfiring'
    | some f => exact ⟨f, rfl, by simpa [hb] using hfiring'⟩
  obtain ⟨hmem, _⟩ := chosen_admitted (hlen ▸ hf)
  obtain ⟨hatt, _, hwait, hsig⟩ := h.ready f hmem
  obtain ⟨node, hn⟩ := h.nodes f hmem
  unfold hostFinish
  simp only [hlen, hf, hwait, h.held, List.any_nil, Bool.false_eq_true, ↓reduceIte]
  unfold hostRun
  obtain ⟨log₁, used₁, hs₁, h₁⟩ := runAttempts_quiet hq (attemptFuel c) node.retry.limit
    (cs.update f) f hn hsig h.log (by omega)
    (by rw [← retryOf_of hn]; exact limit_lt_attemptFuel c _)
  rw [h₁]
  have hrec : recorded node.retry
      (finalFrom node.retry (node.script f.ordinal) f.attempt node.retry.limit).1
      (finalFrom node.retry (node.script f.ordinal) f.attempt node.retry.limit).2 =
        (c.view ch).record f.key.1 f.ordinal := by
    rw [hatt]
    simp [Case.view, Spec.view, Flow.Spec.record, hn, Flow.attempts]
  rw [hrec]
  refine route_agrees hq h hmem hn _ hs₁ rfl rfl rfl rfl rfl (filter_update cs f f rfl) rfl rfl rfl
    rfl rfl rfl ?_ ?_ rfl
  · simp [attemptOf, Case.count, hn, Flow.attempts, hatt, State.update]
  · simp only [State.update, retriesOf, retriesFrom, Case.count, Case.delay, hn, Flow.attempts,
      hatt, Live.flow_fst, Live.flow_snd]
    congr 2
    funext i
    rw [Nat.add_comm 1 i]

/-! ## Whole runs -/

/-- The flow model's first state, before its seeds deliver. -/
def flowStart (v : Flow.Case) : Flow.State :=
  { keys := [], firings := List.replicate v.nodes.length 0, live := [], steps := [], finished := []
    budgetExceeded := [] }

/-- The seed tokens of the flow model. -/
def seedTokens (v : Flow.Case) : List Flow.Token :=
  (Flow.seeds v).map fun (node, edge) => ⟨node, 0, edge⟩

theorem start_eq (v : Flow.Case) :
    Flow.start v =
      { (Flow.deliver v (flowStart v) (seedTokens v)).1 with
        live := (Flow.deliver v (flowStart v) (seedTokens v)).2.mergeSort Flow.keyLe
        steps := [((Flow.deliver v (flowStart v) (seedTokens v)).2.mergeSort Flow.keyLe).map
          (·.1)] } :=
  rfl

theorem view_length (ch : List Nat) : (c.view ch).nodes.length = c.nodes.length := by
  simp [Case.view, Spec.view]

/-- The two models start alike. -/
theorem start_agrees (hq : Quiet c) (ch : List Nat) :
    Agrees c ch (Flow.start (c.view ch)) (start c) := by
  have hbudget := Flow.within_budget_start (c.view ch)
  have h₀ : Flow.WithinBudget (c.view ch) (flowStart (c.view ch)) := by
    intro n node _
    simp only [flowStart, Flow.State.firingsOf, List.getElem?_replicate]
    split <;> simp
  have hl₀ : (flowStart (c.view ch)).firings.length = (c.view ch).nodes.length := by
    simp [flowStart]
  obtain ⟨_, hdfin, hdlen, _⟩ := Flow.deliver_counts (c.view ch) (seedTokens (c.view ch))
    (flowStart (c.view ch)) hl₀
  have hnum := flow_deliver_number (c.view ch) (seedTokens (c.view ch)) (flowStart (c.view ch))
  obtain ⟨hnew, hnewk⟩ := Flow.deliver_started (c.view ch) (seedTokens (c.view ch))
    (flowStart (c.view ch))
  have hdnodes := flow_deliver_nodes (c.view ch) (seedTokens (c.view ch)) (flowStart (c.view ch))
  have hfuel := deliver_le_budget ch h₀ hl₀ (seedTokens (c.view ch))
  have hseeds : ((Flow.seeds c.flow).map fun (node, edge) => (⟨node, 0, edge, false⟩ : Token)) =
      (seedTokens (c.view ch)).map lift := by
    simp only [seedTokens, List.map_map, Function.comp_def, lift]
    rfl
  unfold start
  simp only [hseeds]
  obtain ⟨log₁, hs₁, h₁⟩ := deliver_quiet ch (seedTokens (c.view ch))
    ({ log := ⟨[], (by simp [LogOK] : LogOK c [])⟩, keys := [], tainted := []
       firings := List.replicate c.nodes.length 0, started := List.replicate c.nodes.length 0
       nextId := 1, live := [], held := [], steps := [], finished := [], budgetExceeded := []
       completed := [], attempts := [], retries := [], controls := [], failed := false,
       used := 0 } : State c)
    (flowStart (c.view ch)) (by simp [NoStop]) rfl rfl (by simp [flowStart, view_length]) rfl
  rw [h₁]
  rw [start_eq] at hbudget ⊢
  generalize Flow.deliver (c.view ch) (flowStart (c.view ch)) (seedTokens (c.view ch)) = D
    at hbudget hdfin hdlen hnum hnew hnewk hdnodes hfuel ⊢
  unfold raise
  rw [hq.holds]
  simp only [Bool.false_eq_true, ↓reduceIte]
  rw [decideAll_admits hq _ [] _ _ (by simp) (fresh_nodup (by simp) (by simp) _)
    (by simp only [fresh_length, List.length_map, routeFuel, budget] at hfuel ⊢; omega)]
  dsimp only
  have hadm : (admitList (List.replicate c.nodes.length 0) (fresh 1 (D.2.map (·.1)))).map
      Live.flow = D.2 := by
    rw [show Live.flow = fun f => (f.key, f.ordinal) from rfl, admitList_numbers, fresh_keys]
    conv => rhs; rw [hnum.1]
    simp [flowStart, view_length]
  refine ⟨hs₁, rfl, rfl, rfl, ?_, rfl, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, rfl, rfl, rfl,
    ?_, ?_, ?_, hbudget, hdlen⟩
  · simp only [fresh_keys]
    rw [hnum.2]
    simp [flowStart, view_length]
  · simp only [List.nil_append]
    rw [hadm]
    exact (List.mergeSort_perm _ _).symm
  · exact List.pairwise_mergeSort keyLe_trans keyLe_total _
  · exact ((List.mergeSort_perm _ Flow.keyLe).map (·.1)).nodup_iff.mpr hnew
  · intro x hx
    exact (hnewk x.1 (List.mem_map_of_mem (List.mem_mergeSort.mp hx))).2
  · intro g hg
    exact admitList_fresh_ready _ _ _ g (by simpa using hg)
  · intro g hg
    have hk : g.key ∈ D.2.map (·.1) := by
      rw [← fresh_keys 1 (D.2.map (·.1)), ← admitList_keys (List.replicate c.nodes.length 0)]
      exact List.mem_map_of_mem (by simpa using hg)
    obtain ⟨x, hx, hxk⟩ := List.mem_map.mp hk
    obtain ⟨n', hn'⟩ := hdnodes x hx
    rw [view_node] at hn'
    cases hc : c.nodes[x.1.1]? with
    | none => simp [hc] at hn'
    | some node' => exact ⟨node', hxk ▸ hc⟩
  · rw [List.nil_append, admitList_ids]
    simpa using fresh_nodup (live := []) (nextId := 1) (by simp) (by simp) (D.2.map (·.1))
  · intro g hg
    have hid : g.id ∈ (fresh 1 (D.2.map (·.1))).map (·.id) := by
      rw [← admitList_ids (List.replicate c.nodes.length 0)]
      exact List.mem_map_of_mem (by simpa using hg)
    rw [fresh_ids, List.mem_range'_1, List.length_map] at hid
    show g.id < 1 + D.2.length
    omega
  · simp only [sorted, fresh_keys]
    rw [List.map_mergeSort (r := Flow.keyLe) (s := keyLe) (f := Prod.fst) (fun _ _ _ _ => rfl)]
  · rw [hdfin]
    rfl
  · rw [hdfin]
    rfl
  · rw [hdfin]
    rfl
  · rw [hdfin]
    rfl

/-- Host steps in lockstep: the control host takes the flow host's choices
as `finish` actions, and the states keep agreeing. -/
theorem loop_agrees (hq : Quiet c) {ch : List Nat} (hch : c.schedule = ch.map .finish) :
    ∀ (fuel k : Nat) (fs : Flow.State) (cs : State c), Agrees c ch fs cs →
      Agrees c ch (Flow.loop (c.view ch) fuel k fs) (loop c fuel k cs)
  | 0, _, _, _, h => h
  | fuel + 1, k, fs, cs, h => by
    have hlen := h.live.length_eq
    rw [List.length_map] at hlen
    unfold Flow.loop Control.loop
    split
    · rename_i hnone
      have hfs : fs.live = [] := by
        rw [List.getElem?_eq_none_iff] at hnone
        cases hl : fs.live with
        | nil => rfl
        | cons x xs =>
          rw [hl] at hnone
          have := Nat.mod_lt ((c.view ch).schedule[k]?.getD 0) (show 0 < (x :: xs).length by simp)
          omega
      have hcs : cs.live = [] := List.eq_nil_of_length_eq_zero (by rw [hlen, hfs]; rfl)
      simp only [hcs, h.held, List.isEmpty_nil, Bool.and_self, ↓reduceIte]
      exact h
    · rename_i firing hfiring
      have hne : cs.live ≠ [] := fun hcs => by
        have : fs.live.length = 0 := by rw [← hlen, hcs]; rfl
        rw [List.length_eq_zero_iff] at this
        simp [this] at hfiring
      have hact : (c.schedule[k]?).getD (fallback cs) = .finish ((ch[k]?).getD 0) := by
        rw [hch, List.getElem?_map]
        cases ch[k]? with
        | none => simp [fallback, h.held]
        | some n => rfl
      have hemp : cs.live.isEmpty = false := by simpa [List.isEmpty_iff] using hne
      simp only [hemp, Bool.false_and, Bool.false_eq_true, ↓reduceIte, hact, act]
      exact loop_agrees hq hch fuel (k + 1) _ _ (finish_agrees hq h _ hfiring)

theorem loop_settled (c : Case) :
    ∀ (fuel k : Nat) (s : State c), s.live = [] → s.held = [] → loop c fuel k s = s
  | 0, _, _, _, _ => rfl
  | _ + 1, _, _, hl, hh => by simp [loop, hl, hh]

/-- Running `a` steps, then `b` more, is running `a + b` steps. -/
theorem loop_add (c : Case) : ∀ (a b k : Nat) (s : State c),
    loop c b (k + a) (loop c a k s) = loop c (a + b) k s
  | 0, b, k, s => by
    show loop c b (k + 0) s = loop c (0 + b) k s
    rw [Nat.add_zero, Nat.zero_add]
  | a + 1, b, k, s => by
    rw [Nat.add_right_comm a 1 b]
    by_cases he : (s.live.isEmpty && s.held.isEmpty) = true
    · have hs : s.live = [] ∧ s.held = [] := by simpa using he
      rw [loop_settled c _ _ s hs.1 hs.2, loop_settled c _ _ s hs.1 hs.2,
        loop_settled c _ _ s hs.1 hs.2]
    · simp only [loop, he, Bool.false_eq_true, ↓reduceIte]
      rw [show k + (a + 1) = k + 1 + a by omega, loop_add c a b (k + 1)]

theorem flow_loop_settled (v : Flow.Case) :
    ∀ (fuel k : Nat) (s : Flow.State), s.live = [] → Flow.loop v fuel k s = s
  | 0, _, _, _ => rfl
  | _ + 1, _, _, hl => by simp [Flow.loop, hl]

theorem flow_loop_add (v : Flow.Case) : ∀ (a b k : Nat) (s : Flow.State),
    Flow.loop v b (k + a) (Flow.loop v a k s) = Flow.loop v (a + b) k s
  | 0, b, k, s => by
    show Flow.loop v b (k + 0) s = Flow.loop v (0 + b) k s
    rw [Nat.add_zero, Nat.zero_add]
  | a + 1, b, k, s => by
    rw [Nat.add_right_comm a 1 b]
    cases hf : s.live[(v.schedule[k]?.getD 0) % s.live.length]? with
    | none =>
      have hl : s.live = [] := by
        rw [List.getElem?_eq_none_iff] at hf
        cases hs : s.live with
        | nil => rfl
        | cons x xs =>
          rw [hs] at hf
          have := Nat.mod_lt (v.schedule[k]?.getD 0) (show 0 < (x :: xs).length by simp)
          omega
      rw [flow_loop_settled v _ _ s hl, flow_loop_settled v _ _ s hl,
        flow_loop_settled v _ _ s hl]
    | some firing =>
      simp only [Flow.loop, hf]
      rw [show k + (a + 1) = k + 1 + a by omega, flow_loop_add v a b (k + 1)]

/-- The two models' runs end agreeing. -/
theorem runState_agrees (hq : Quiet c) {ch : List Nat} (hch : c.schedule = ch.map .finish) :
    Agrees c ch (Flow.runState (c.view ch)) (runState c) := by
  have hc : runState c =
      loop c (c.schedule.length + (owed (loop c c.schedule.length 0 (start c)) + 1)) 0 (start c) := by
    unfold runState
    dsimp only
    rw [← loop_add c _ _ 0, Nat.zero_add]
  have hsettled := run_settles c
  rw [hc] at hsettled ⊢
  have hflow := Flow.run_settles (c.view ch)
  unfold Flow.runState
  generalize c.schedule.length + (owed (loop c c.schedule.length 0 (start c)) + 1) = N at hsettled ⊢
  generalize ((c.view ch).nodes.map (·.maxFirings)).sum + 1 = M at hflow ⊢
  have hN := loop_agrees hq hch N 0 _ _ (start_agrees hq ch)
  have hM := loop_agrees hq hch M 0 _ _ (start_agrees hq ch)
  by_cases hle : M ≤ N
  · obtain ⟨d, rfl⟩ := Nat.exists_eq_add_of_le hle
    rw [← flow_loop_add, Nat.zero_add, flow_loop_settled _ _ _ _ hflow] at hN
    exact hN
  · obtain ⟨d, rfl⟩ := Nat.exists_eq_add_of_le (Nat.le_of_lt (Nat.lt_of_not_le hle))
    rw [← loop_add, Nat.zero_add, loop_settled _ _ _ _ hsettled.1 hsettled.2] at hM
    exact hM

/-- A generated case run by a host that only finishes firings, in the order of
the case's schedule, never holds a decision, and answers with `verdicts`. -/
def ofSpec (s : Spec) (verdicts : List Nat) : Case where
  nodes := s.nodes
  schedule := s.schedule.map .finish
  holds := false
  verdicts

/-- **The control model without stops is the flow model.** A host that only
finishes firings, never holds a decision and answers each one with the
default, on scripts that never report `cancelled`, observes the same run in
`Control.run` as in `Spec.run`: the same steps, records, attempts, retries,
parked tokens, budget refusals and status, with no stop signal and nothing
completed without running. -/
theorem control_free_agrees (s : Spec) (verdicts : List Nat)
    (hv : ∀ roll ∈ verdicts, DefaultRoll roll)
    (hs : ∀ node ∈ s.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script) :
    run (ofSpec s verdicts) = s.run := by
  have hq : Quiet (ofSpec s verdicts) := ⟨rfl, hv, hs⟩
  have h := runState_agrees hq (ch := s.schedule) rfl
  obtain ⟨hlive, hheld⟩ := run_settles (ofSpec s verdicts)
  have hflive := Flow.run_settles s.view
  have hview : (ofSpec s verdicts).view s.schedule = s.view := rfl
  rw [hview] at h
  have hflive : (Flow.runState s.view).live = [] := Flow.run_settles s.view
  simp only [run, Flow.Spec.run, Flow.run]
  rw [h.steps, h.finished, h.keys, h.budgetExceeded, hlive, hheld, h.log.rootStopped, h.failed,
    h.attempts, h.retries, h.controls, h.completed, hflive]
  rfl

/-! ## The flow and retry theorems, carried over -/

/-- In such a run, each `(node, generation)` key starts at most once
(`Flow.started_once`). -/
theorem control_free_started_once (s : Spec) (verdicts : List Nat)
    (hv : ∀ roll ∈ verdicts, DefaultRoll roll)
    (hs : ∀ node ∈ s.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script) :
    (run (ofSpec s verdicts)).steps.flatten.Nodup := by
  rw [control_free_agrees s verdicts hv hs]
  exact Flow.started_once s.view

/-- In such a run, a budget refusal fails the run
(`Flow.budget_exceeded_fails`). -/
theorem control_free_budget_exceeded_fails (s : Spec) (verdicts : List Nat)
    (hv : ∀ roll ∈ verdicts, DefaultRoll roll)
    (hs : ∀ node ∈ s.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script)
    (h : (run (ofSpec s verdicts)).budgetExceeded ≠ []) :
    (run (ofSpec s verdicts)).status ≠ "success" := by
  rw [control_free_agrees s verdicts hv hs] at h ⊢
  exact Flow.budget_exceeded_fails s.view h

/-- Cutting every firing to its last attempt keeps scripts free of
`cancelled`. -/
theorem finalized_uncancelled (s : Spec)
    (hs : ∀ node ∈ s.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script) :
    ∀ node ∈ s.finalized.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script := by
  intro node hnode script hscript
  simp only [Flow.Spec.finalized, List.mem_map] at hnode
  obtain ⟨n, hn, rfl⟩ := hnode
  simp only [List.mem_map, List.mem_range] at hscript
  obtain ⟨o, _, rfl⟩ := hscript
  simp only [List.mem_singleton]
  intro h
  rw [Flow.attempts, finalFrom_outcome] at h
  exact attemptAt_ne_cancelled (script_ne_cancelled (hs n hn) o) _ h.symm

/-- In such a run, retries are invisible outside the log
(`Flow.retries_invisible`): the same run with every firing cut to its last
attempt looks the same to routing and the run context. -/
theorem control_free_retries_invisible (s : Spec) (verdicts : List Nat)
    (hv : ∀ roll ∈ verdicts, DefaultRoll roll)
    (hs : ∀ node ∈ s.nodes, ∀ script ∈ node.outcomes, Outcome.cancelled ∉ script) :
    (run (ofSpec s.finalized verdicts)).routing = (run (ofSpec s verdicts)).routing := by
  rw [control_free_agrees s verdicts hv hs,
    control_free_agrees s.finalized verdicts hv (finalized_uncancelled s hs)]
  exact Flow.retries_invisible s

end PetriModel.Control
