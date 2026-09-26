import PetriModel.Join

/-!
# Join theorems

For one `(node, generation)` key:

* `fires_at_most_once`: whatever tokens arrive, the node fires at most once.
* `fired_iff`: the key has fired after a sequence of arrivals exactly when
  the distinct edges of that sequence satisfy the join.
* `fired_depends_only_on_edges`: so whether it fires depends only on which
  edges delivered a token, never on arrival order or duplicates. The driver's
  completion order is not deterministic; this is why that does not matter to
  a join.
* `all_fires_iff`, `any_fires_iff`, `quorum_fires_iff`: the policies, stated
  over the arrivals.
* `quorum_never_fires`: a `Quorum n` whose tokens come only from fewer than
  `max n 1` incoming edges never fires. Load-time validation accepts such a
  node today.
-/

namespace PetriModel.Join

/-! ## Storing tokens -/

theorem mem_store {tokens : List Nat} {edge x : Nat} :
    x ∈ store tokens edge ↔ x ∈ tokens ∨ x = edge := by
  unfold store
  split <;> rename_i h
  · have : edge ∈ tokens := by simpa [List.contains_iff_mem] using h
    constructor
    · exact Or.inl
    · rintro (hx | rfl) <;> assumption
  · simp

theorem store_nodup {tokens : List Nat} {edge : Nat} (h : tokens.Nodup) :
    (store tokens edge).Nodup := by
  unfold store
  split <;> rename_i hc
  · exact h
  · have : edge ∉ tokens := by simpa [List.contains_iff_mem] using hc
    rw [List.nodup_append]
    refine ⟨h, List.nodup_cons.mpr ⟨List.not_mem_nil, List.nodup_nil⟩, ?_⟩
    intro a ha b hb
    simp only [List.mem_singleton] at hb
    subst hb
    rintro rfl
    exact this ha

theorem store_prefix (tokens : List Nat) (edge : Nat) : ∃ t, store tokens edge = tokens ++ t := by
  unfold store
  split
  · exact ⟨[], by simp⟩
  · exact ⟨[edge], rfl⟩

theorem mem_collect {acc es : List Nat} {x : Nat} : x ∈ collect acc es ↔ x ∈ acc ∨ x ∈ es := by
  induction es generalizing acc with
  | nil => simp [collect]
  | cons e es ih =>
    simp only [collect, ih, mem_store, List.mem_cons]
    constructor
    · rintro ((h | rfl) | h)
      · exact Or.inl h
      · exact Or.inr (Or.inl rfl)
      · exact Or.inr (Or.inr h)
    · rintro (h | rfl | h)
      · exact Or.inl (Or.inl h)
      · exact Or.inl (Or.inr rfl)
      · exact Or.inr h

theorem collect_nodup {acc : List Nat} (es : List Nat) (h : acc.Nodup) : (collect acc es).Nodup := by
  induction es generalizing acc with
  | nil => exact h
  | cons e es ih => exact ih (store_nodup h)

theorem collect_prefix (acc es : List Nat) : ∃ t, collect acc es = acc ++ t := by
  induction es generalizing acc with
  | nil => exact ⟨[], by simp [collect]⟩
  | cons e es ih =>
    obtain ⟨t₁, h₁⟩ := store_prefix acc e
    obtain ⟨t₂, h₂⟩ := ih (store acc e)
    exact ⟨t₁ ++ t₂, by rw [collect, h₂, h₁, List.append_assoc]⟩

/-! ## The join rule -/

/-- More tokens never undo a satisfied join. -/
theorem satisfied_append {policy : Policy} {incoming a : List Nat} (b : List Nat)
    (h : satisfied policy incoming a = true) : satisfied policy incoming (a ++ b) = true := by
  cases a with
  | nil => simp [satisfied] at h
  | cons x xs =>
    cases policy with
    | all =>
      simp only [satisfied, List.cons_append, List.all_eq_true, List.contains_iff_mem] at h ⊢
      intro e he
      have := h e he
      simp only [List.mem_cons, List.mem_append] at this ⊢
      rcases this with h | h
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
    | any => rfl
    | quorum n =>
      simp only [satisfied, List.cons_append, List.length_cons, List.length_append,
        decide_eq_true_eq] at h ⊢
      omega

/-- Two token sets with the same edges satisfy a join equally. -/
theorem satisfied_ext {policy : Policy} {incoming a b : List Nat} (ha : a.Nodup) (hb : b.Nodup)
    (hab : ∀ x, x ∈ a ↔ x ∈ b) : satisfied policy incoming a = satisfied policy incoming b := by
  cases a with
  | nil =>
    cases b with
    | nil => rfl
    | cons y ys => exact absurd ((hab y).mpr (List.mem_cons_self)) List.not_mem_nil
  | cons x xs =>
    cases b with
    | nil => exact absurd ((hab x).mp (List.mem_cons_self)) List.not_mem_nil
    | cons y ys =>
      cases policy with
      | all =>
        simp only [satisfied]
        apply Bool.eq_iff_iff.mpr
        simp only [List.all_eq_true, List.contains_iff_mem]
        exact ⟨fun h e he => (hab e).mp (h e he), fun h e he => (hab e).mpr (h e he)⟩
      | any => rfl
      | quorum n =>
        simp only [satisfied]
        rw [((List.perm_ext_iff_of_nodup ha hb).mpr hab).length_eq]

/-! ## Arrivals -/

/-- A fired key ignores every later token. -/
theorem arriveAll_of_fired {policy : Policy} {incoming : List Nat} {key : Key}
    (h : key.fired = true) (es : List Nat) : arriveAll policy incoming key es = key := by
  induction es with
  | nil => rfl
  | cons e es ih => simp [arriveAll, arrive, h, ih]

theorem fireCount_of_fired {policy : Policy} {incoming : List Nat} {key : Key}
    (h : key.fired = true) (es : List Nat) : fireCount policy incoming key es = 0 := by
  induction es with
  | nil => rfl
  | cons e es ih => simp [fireCount, arrive, h, ih]

/-- Whatever tokens arrive, a key fires at most once. -/
theorem fires_at_most_once (policy : Policy) (incoming : List Nat) (key : Key) (es : List Nat) :
    fireCount policy incoming key es ≤ 1 := by
  induction es generalizing key with
  | nil => simp [fireCount]
  | cons e es ih =>
    simp only [fireCount, arrive]
    split
    · simpa using ih key
    · split
      · simp [fireCount_of_fired]
      · simpa using ih _

/-- From an unfired key whose parked tokens do not yet satisfy the join, the
key ends up fired exactly when every distinct edge seen satisfies it. -/
theorem fired_after (policy : Policy) (incoming : List Nat) :
    ∀ (es acc : List Nat), satisfied policy incoming acc = false →
      (arriveAll policy incoming { tokens := acc, fired := false } es).fired =
        satisfied policy incoming (collect acc es)
  | [], acc, h => by simp [arriveAll, collect, h]
  | e :: es, acc, h => by
    simp only [arriveAll, collect, arrive]
    by_cases hs : satisfied policy incoming (store acc e) = true
    · simp only [hs, Bool.false_eq_true, ↓reduceIte]
      rw [arriveAll_of_fired rfl]
      obtain ⟨t, ht⟩ := collect_prefix (store acc e) es
      rw [ht, satisfied_append t hs]
    · have hs' : satisfied policy incoming (store acc e) = false := by simpa using hs
      simp only [hs', Bool.false_eq_true, ↓reduceIte]
      exact fired_after policy incoming es (store acc e) hs'

theorem fired_iff (policy : Policy) (incoming es : List Nat) :
    (arriveAll policy incoming {} es).fired = satisfied policy incoming (collect [] es) :=
  fired_after policy incoming es [] (by simp [satisfied])

/-- Whether a key fires depends only on which edges delivered a token: not
on their order, and not on duplicates. -/
theorem fired_depends_only_on_edges (policy : Policy) (incoming : List Nat) {es₁ es₂ : List Nat}
    (h : ∀ e, e ∈ es₁ ↔ e ∈ es₂) :
    (arriveAll policy incoming {} es₁).fired = (arriveAll policy incoming {} es₂).fired := by
  rw [fired_iff, fired_iff]
  apply satisfied_ext (collect_nodup _ List.nodup_nil) (collect_nodup _ List.nodup_nil)
  intro x
  simp only [mem_collect, List.not_mem_nil, false_or, h]

/-! ## The three policies -/

theorem collect_eq_nil {es : List Nat} : collect [] es = [] ↔ es = [] := by
  constructor
  · intro h
    cases es with
    | nil => rfl
    | cons e es =>
      have : e ∈ collect [] (e :: es) := mem_collect.mpr (Or.inr List.mem_cons_self)
      rw [h] at this
      exact absurd this List.not_mem_nil
  · rintro rfl
    rfl

/-- `All` fires once every incoming edge has delivered a token. -/
theorem all_fires_iff (incoming : List Nat) {es : List Nat} (hne : es ≠ []) :
    (arriveAll .all incoming {} es).fired = true ↔ ∀ e ∈ incoming, e ∈ es := by
  rw [fired_iff]
  cases hc : collect [] es with
  | nil => exact absurd (collect_eq_nil.mp hc) hne
  | cons x xs =>
    simp only [satisfied, List.all_eq_true, List.contains_iff_mem, ← hc, mem_collect,
      List.not_mem_nil, false_or]

/-- `Any` fires on the first token. -/
theorem any_fires_iff (incoming es : List Nat) :
    (arriveAll .any incoming {} es).fired = true ↔ es ≠ [] := by
  rw [fired_iff]
  cases hc : collect [] es with
  | nil => simp [satisfied, collect_eq_nil.mp hc]
  | cons x xs =>
    simp only [satisfied, true_iff]
    rintro rfl
    simp [collect] at hc

/-- `Quorum n` fires once `max n 1` distinct edges have delivered a token. -/
theorem quorum_fires_iff (n : Nat) (incoming es : List Nat) :
    (arriveAll (.quorum n) incoming {} es).fired = true ↔ max n 1 ≤ (collect [] es).length := by
  rw [fired_iff]
  cases hc : collect [] es with
  | nil => simp [satisfied]
  | cons x xs => simp [satisfied]

/-- A `Quorum n` fed only through fewer than `max n 1` incoming edges never
fires, whatever arrives. -/
theorem quorum_never_fires (n : Nat) {incoming es : List Nat} (hin : ∀ e ∈ es, e ∈ incoming)
    (hlt : incoming.length < max n 1) :
    (arriveAll (.quorum n) incoming {} es).fired = false := by
  have hsub : collect [] es ⊆ incoming := by
    intro x hx
    simp only [mem_collect, List.not_mem_nil, false_or] at hx
    exact hin x hx
  have hle := List.Nodup.length_le_of_subset (collect_nodup es List.nodup_nil) hsub
  have : ¬ max n 1 ≤ (collect [] es).length := by omega
  cases h : (arriveAll (.quorum n) incoming {} es).fired
  · rfl
  · exact absurd ((quorum_fires_iff n incoming es).mp h) this

end PetriModel.Join
