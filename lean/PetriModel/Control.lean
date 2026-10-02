import PetriModel.Retry

/-!
# Stops

Cancel and kill (`engine-spec.md` §5) over the generated flows, with a host
that can also run a single attempt and leave a firing waiting on its retry
backoff. It mirrors `stop_scope`, the cancel admission in `try_fire`, the
retry decision in `on_step_finished` and the kill checks in `on_token` and
`on_step_finished` (`crates/core/engine/src/apply.rs`):

* a stop marks its closure: the root covers every node, a group its members;
* a node reached by a cancel, or fed by a firing that recorded `cancelled`,
  completes `cancelled` without running unless it is `run_on_cancel`; the
  completion counts against the budget, and the budget refuses it quietly;
* a kill swallows tokens aimed into its closure, drops the ones parked there,
  and routes nothing out of it;
* each live firing the stop reaches gets a signal, a cancel skipping one
  already signalled; a firing waiting on its backoff is settled at once,
  recorded `cancelled` and routed unless killed;
* a firing is never retried once a stop reached it.

The core hands the host two kinds of decision, an admission for each firing
it starts and a routing for each outcome it records, and the harness answers
them depth first: a routing's tokens arrive in order, and the decisions they
cause are answered before anything queued after it. A firing's number among
its node's started firings, which picks its script, is fixed when its
admission is answered. Firing ids order the signals and the settles, as
`live_firings` does.

The run keeps a log of stops, starts, retries and routings, newest first.
Each entry carries the proof of the rule it must satisfy against the entries
below it (`Valid`), so the model cannot build a run that breaks one; the
theorems in `Thm/Control.lean` read the rules back out of the log.
-/

namespace PetriModel.Control

open Flow (Key Status Outcome SpecNode Observed Retry emit incoming seeds parkedLe attemptAt
  recorded baseDelay)

/-- The cancellation scope a stop names. -/
inductive Target where
  | root
  /-- A declared cancellation group, by its anchor. -/
  | group (anchor : Nat)
  deriving Repr, DecidableEq

/-- A stop's tier. -/
inductive Tier where
  | cancel
  | kill
  deriving Repr, DecidableEq

def Tier.tag : Tier → String
  | .cancel => "cancel"
  | .kill => "kill"

/-- One host step (`Action` in the Rust test). -/
inductive Action where
  /-- Finish the chosen live firing, running every attempt its script
  reaches. -/
  | finish (choice : Nat)
  /-- Run only the chosen firing's next attempt. -/
  | attempt (choice : Nat)
  | stop (tier : Tier) (target : Target)
  deriving Repr

/-- A generated case with its host actions. -/
structure Case where
  nodes : List SpecNode
  schedule : List Action
  deriving Repr

/-- The case as routing sees its graph, to share `Flow`'s edges and seeds. -/
def Case.flow (c : Case) : Flow.Case where
  nodes := c.nodes.map fun n => { join := n.join, maxFirings := n.maxFirings, groups := n.groups }
  schedule := []
  record := fun _ _ => .success

/-- Whether a stop of `target` reaches `node`. -/
def Case.covers (c : Case) (target : Target) (node : Nat) : Bool :=
  match target with
  | .root => true
  | .group anchor => (c.nodes[node]?.bind (·.group)) == some anchor

def Case.runOnCancel (c : Case) (node : Nat) : Bool :=
  (c.nodes[node]?).any (·.runOnCancel)

/-! ## The log -/

inductive Entry where
  | stop (tier : Tier) (target : Target)
  /-- A firing of `node` started; `tainted` when a token it took came from a
  firing that recorded `cancelled`. -/
  | started (node : Nat) (tainted : Bool)
  /-- A firing of `node` scheduled a retry. -/
  | retried (node : Nat)
  /-- A firing of `node` routed its outcome. -/
  | routed (node : Nat)
  deriving Repr

/-- Whether a stop in `log` reaches `node`. -/
def cancelledIn (c : Case) (log : List Entry) (node : Nat) : Bool :=
  log.any fun e => match e with
    | .stop _ target => c.covers target node
    | _ => false

/-- Whether a kill in `log` reaches `node`. -/
def killedIn (c : Case) (log : List Entry) (node : Nat) : Bool :=
  log.any fun e => match e with
    | .stop .kill target => c.covers target node
    | _ => false

/-- Whether a stop of the root is in `log`. -/
def rootStopped (log : List Entry) : Bool :=
  log.any fun e => match e with
    | .stop _ .root => true
    | _ => false

/-- The rule an entry must satisfy against the entries below it (§5). -/
def Valid (c : Case) : Entry → List Entry → Prop
  | .stop _ _, _ => True
  | .started node tainted, log =>
    killedIn c log node = false ∧
      ((cancelledIn c log node || tainted) = true → c.runOnCancel node = true)
  | .retried node, log => cancelledIn c log node = false
  | .routed node, log => killedIn c log node = false

def LogOK (c : Case) : List Entry → Prop
  | [] => True
  | e :: rest => Valid c e rest ∧ LogOK c rest

/-- A log whose every entry keeps its rule, newest first. -/
structure Log (c : Case) where
  entries : List Entry
  ok : LogOK c entries

def Log.push {c : Case} (log : Log c) (e : Entry) (h : Valid c e log.entries) : Log c :=
  ⟨e :: log.entries, ⟨h, log.ok⟩⟩

/-! ## The run -/

/-- A token on its way to a key; `cancelled` when its source recorded
`cancelled`. -/
structure Token where
  target : Nat
  generation : Nat
  edge : Nat
  cancelled : Bool
  deriving Repr

/-- A running firing. -/
structure Live where
  id : Nat
  key : Key
  /-- Which of its node's started firings it is. -/
  ordinal : Nat
  attempt : Nat
  waiting : Bool
  signalled : Bool
  deriving Repr

/-- A recorded firing whose outcome has yet to route. -/
structure Routing where
  key : Key
  status : Status
  deriving Repr

/-- A decision the host answers: admit a started firing, or route a recorded
outcome. -/
inductive Decision where
  | admit (id : Nat)
  | route (r : Routing)
  deriving Repr

structure State (c : Case) where
  log : Log c
  keys : List (Key × Join.Key)
  /-- Keys holding a token from a firing that recorded `cancelled`. -/
  tainted : List Key
  /-- Firings per node, as the budget counts them. -/
  firings : List Nat
  /-- Started firings per node. -/
  started : List Nat
  nextId : Nat
  /-- Running firings, by id. -/
  live : List Live
  steps : List (List Key)
  finished : List Key
  budgetExceeded : List Nat
  completed : List Key
  attempts : List (Nat × Nat × Nat × String)
  retries : List (Nat × Nat × Nat × Nat)
  controls : List (Nat × Nat × String)
  failed : Bool

variable {c : Case}

def State.key (s : State c) (k : Key) : Join.Key :=
  ((s.keys.find? (·.1 == k)).map (·.2)).getD {}

@[reducible] def State.setKey (s : State c) (k : Key) (v : Join.Key) : State c :=
  { s with keys := (k, v) :: s.keys.filter (·.1 != k) }

def State.firingsOf (s : State c) (node : Nat) : Nat :=
  s.firings[node]?.getD 0

def State.startedOf (s : State c) (node : Nat) : Nat :=
  s.started[node]?.getD 0

/-- Note that a key holds a token from a firing that recorded `cancelled`. -/
@[reducible] def State.markTainted (s : State c) (k : Key) (taint : Bool) : State c :=
  { s with tainted := if taint then k :: s.tainted.filter (· != k) else s.tainted }

/-- A budget refusal: an error only for a key that would run. -/
@[reducible] def State.refuse (s : State c) (loud : Bool) (node : Nat) : State c :=
  { s with budgetExceeded := if loud then s.budgetExceeded ++ [node] else s.budgetExceeded }

/-- Count one more firing of `node` against its budget. -/
@[reducible] def State.bump (s : State c) (node : Nat) : State c :=
  { s with firings := s.firings.set node (s.firingsOf node + 1) }

def keyLe (a b : Key) : Bool :=
  a.1 < b.1 || (a.1 == b.1 && a.2 ≤ b.2)

def sorted (keys : List Key) : List Key :=
  keys.mergeSort keyLe

/-- What happens to one token (`on_token`, `try_fire`): the new state and the
decision it raises, if any. -/
def deliverOne (s : State c) (t : Token) : State c × List Decision :=
  match hn : c.nodes[t.target]? with
  | none => (s, [])
  | some node =>
    -- A killed scope swallows tokens aimed inside it.
    let log := s.log.entries
    if hk : killedIn c log t.target = true then (s, [])
    else
      let k : Key := (t.target, t.generation)
      let before := s.key k
      let step := Join.arrive node.join (incoming c.flow t.target) before t.edge
      let taint := s.tainted.contains k || (t.cancelled && !before.fired)
      let s := (s.setKey k step.1).markTainted k taint
      if !step.2 then (s, [])
      else
        let cancelled := cancelledIn c log t.target || taint
        let admitted := !cancelled || node.runOnCancel
        -- Only a key that would run is an error (§4, "Budget refusal").
        if node.maxFirings ≤ s.firingsOf t.target then (s.refuse admitted t.target, [])
        else
          let s := s.bump t.target
          if ha : admitted = true then
            let valid : Valid c (.started t.target taint) log := by
              refine ⟨by simpa using hk, fun hc => ?_⟩
              simp only [Case.runOnCancel, hn, Option.any_some]
              simp only [admitted, cancelled, hc, Bool.not_true, Bool.false_or] at ha
              exact ha
            let f : Live := {
              id := s.nextId, key := k, ordinal := 0
              attempt := 1, waiting := false, signalled := false }
            let s := { s with
              log := s.log.push (.started t.target taint) valid
              live := s.live ++ [f]
              nextId := s.nextId + 1 }
            (s, [.admit f.id])
          else
            let s := { s with nextId := s.nextId + 1, completed := s.completed ++ [k] }
            (s, [.route ⟨k, .cancelled⟩])

/-- Deliver tokens in order. -/
def deliver : State c → List Token → State c × List Decision
  | s, [] => (s, [])
  | s, t :: rest =>
    let one := deliverOne s t
    let r := deliver one.1 rest
    (r.1, one.2 ++ r.2)

/-- The tokens a recorded outcome routes, in group order. -/
def tokensOf (c : Case) (k : Key) (status : Status) : List Token :=
  let groups := (c.nodes[k.1]?.map (·.groups)).getD []
  (groups.filterMap (emit status)).map fun arm =>
    ⟨arm.to, if arm.back then k.2 + 1 else k.2, arm.edge, status == .cancelled⟩

/-- Answer decisions depth first: those one routing raises go before the
rest. An admission fixes the firing's number among its node's started
firings; the result lists the keys admitted. A killed firing routes nothing. -/
def decideAll : Nat → State c → List Decision → State c × List Key
  | 0, s, _ => (s, [])
  | _ + 1, s, [] => (s, [])
  | fuel + 1, s, .admit id :: rest =>
    match s.live.find? (·.id == id) with
    | none => decideAll fuel s rest
    | some f =>
      let node := f.key.1
      let s := { s with
        live := s.live.map fun g => if g.id == id then { g with ordinal := s.startedOf node } else g
        started := s.started.set node (s.startedOf node + 1) }
      let r := decideAll fuel s rest
      (r.1, f.key :: r.2)
  | fuel + 1, s, .route r :: rest =>
    if hk : killedIn c s.log.entries r.key.1 = true then decideAll fuel s rest
    else
      let s := { s with log := s.log.push (.routed r.key.1) (by simpa [Valid] using hk) }
      let d := deliver s (tokensOf c r.key r.status)
      decideAll fuel d.1 (d.2 ++ rest)

/-- Enough fuel to answer everything one host step raises: every admission
and every completion counts against a budget, and every firing a stop settles
is live. -/
def routeFuel (c : Case) (s : State c) : Nat :=
  2 * (c.nodes.map (·.maxFirings)).sum + s.live.length + 1

def State.update (s : State c) (f : Live) : State c :=
  { s with live := s.live.map fun g => if g.id == f.id then f else g }

/-- A node's retry policy; one attempt for a node the case lacks. -/
def retryOf (c : Case) (node : Nat) : Retry :=
  ((c.nodes[node]?).map (·.retry)).getD
    { maxAttempts := 1, retryOn := .default, acceptPartial := false,
      initialNanos := 0, factorBits := 0, maxNanos := 0 }

/-- The most attempts any node allows: the fuel of a host step's attempts. -/
def attemptFuel (c : Case) : Nat :=
  (c.nodes.map (·.retry.maxAttempts)).foldl max 1 + 1

/-- What the host reports for a firing's running attempt: `cancelled` when it
honors a stop signal, the scripted outcome otherwise. -/
def report (c : Case) (f : Live) : Outcome :=
  match c.nodes[f.key.1]? with
  | none => .success
  | some node =>
    if f.signalled && node.honor then .cancelled else attemptAt (node.script f.ordinal) f.attempt

/-- The host reports attempts of `f` until one is final, or, for a single
attempt, until a retry leaves it waiting. -/
def runAttempts (every : Bool) : Nat → State c → Live → State c × List Key
  | 0, s, _ => (s, [])
  | fuel + 1, s, f =>
    let retry := retryOf c f.key.1
    let o := report c f
    let stopped := f.signalled || cancelledIn c s.log.entries f.key.1
    if hr : (!stopped && retry.retries o && decide (f.attempt < retry.limit)) = true then
      -- A firing a stop reached is never retried.
      let valid : Valid c (.retried f.key.1) s.log.entries := by
        simp only [Valid]
        simp only [stopped, Bool.and_eq_true, Bool.not_eq_true', Bool.or_eq_false_iff] at hr
        exact hr.1.1.2
      let s := { s with
        log := s.log.push (.retried f.key.1) valid
        retries := s.retries ++ [(f.key.1, f.key.2, f.attempt + 1, baseDelay retry f.attempt)] }
      if every then
        let f := { f with attempt := f.attempt + 1 }
        runAttempts every fuel (s.update f) f
      else (s.update { f with waiting := true }, [])
    else
      let status := recorded retry f.attempt o
      let s := { s with
        live := s.live.filter (·.id != f.id)
        finished := s.finished ++ [f.key]
        attempts := s.attempts ++ [(f.key.1, f.key.2, f.attempt, status.tag)]
        failed := s.failed || status.isFailure }
      decideAll (routeFuel c s) s [.route ⟨f.key, status⟩]

/-- `finish` and `attempt`: the chosen firing in ascending key order. A
firing waiting on its backoff first gets its `RetryElapsed`. -/
def hostFinish (every : Bool) (s : State c) (choice : Nat) : State c :=
  let byKey := s.live.mergeSort fun a b => keyLe a.key b.key
  match byKey[choice % byKey.length]? with
  | none => s
  | some f =>
    let f := if f.waiting then { f with attempt := f.attempt + 1, waiting := false } else f
    let r := runAttempts every (attemptFuel c) (s.update f) f
    { r.1 with steps := r.1.steps ++ [sorted r.2] }

/-- A kill drops the tokens parked at the keys of its closure. -/
@[reducible] def State.dropTokens (s : State c) (target : Target) (kill : Bool) : State c :=
  { s with keys := if kill then s.keys.map (fun (k, key) =>
      if c.covers target k.1 then (k, { key with tokens := [] }) else (k, key)) else s.keys }

/-- Cancel or kill (`stop_scope`): mark the closure, then settle every live
firing it reaches that waits on its backoff and signal the others, in id
order. A cancel skips a firing already signalled. -/
def hostStop (s : State c) (tier : Tier) (target : Target) : State c :=
  let s := { s with log := s.log.push (.stop tier target) trivial }
  let s := s.dropTokens target (tier == .kill)
  let parts := s.live.partition fun f => c.covers target f.key.1 && f.waiting
  let settled := parts.1
  let signals := fun f : Live => c.covers target f.key.1 && (tier == .kill || !f.signalled)
  let s := { s with
    live := parts.2.map fun f => if signals f then { f with signalled := true } else f
    controls := s.controls ++ (parts.2.filter signals).map fun f => (f.key.1, f.key.2, tier.tag)
    finished := s.finished ++ sorted (settled.map (·.key))
    attempts := s.attempts ++ (settled.mergeSort fun a b => keyLe a.key b.key).map fun f =>
      (f.key.1, f.key.2, f.attempt, Status.cancelled.tag) }
  let r := decideAll (routeFuel c s + settled.length) s
    (settled.map fun f => .route ⟨f.key, .cancelled⟩)
  { r.1 with steps := r.1.steps ++ [sorted r.2] }

def act (s : State c) : Action → State c
  | .finish choice => hostFinish true s choice
  | .attempt choice => hostFinish false s choice
  | .stop tier target => hostStop s tier target

def start (c : Case) : State c :=
  let s₀ : State c := {
    log := ⟨[], trivial⟩, keys := [], tainted := []
    firings := List.replicate c.nodes.length 0, started := List.replicate c.nodes.length 0
    nextId := 1, live := [], steps := [], finished := [], budgetExceeded := []
    completed := [], attempts := [], retries := [], controls := [], failed := false }
  let d := deliver s₀ ((seeds c.flow).map fun (node, edge) => ⟨node, 0, edge, false⟩)
  let r := decideAll (routeFuel c d.1) d.1 d.2
  { r.1 with steps := [sorted r.2] }

/-- Host steps until nothing runs: the schedule, then the first live firing
each step. -/
def loop (c : Case) : Nat → Nat → State c → State c
  | 0, _, s => s
  | fuel + 1, k, s =>
    if s.live.isEmpty then s
    else loop c fuel (k + 1) (act s ((c.schedule[k]?).getD (.finish 0)))

/-- The final state: the schedule's steps plus one per firing the budgets
allow. -/
def runState (c : Case) : State c :=
  loop c (c.schedule.length + (c.nodes.map (·.maxFirings)).sum + 1) 0 (start c)

def run (c : Case) : Observed :=
  let s := runState c
  let parked := (s.keys.flatMap fun ((node, generation), key) =>
      if key.fired then [] else key.tokens.map fun edge => (node, generation, edge)).mergeSort parkedLe
  let status :=
    if !s.live.isEmpty then "unsettled"
    else if rootStopped s.log.entries then "cancelled"
    else if s.failed || !s.budgetExceeded.isEmpty then "failed"
    else "success"
  { steps := s.steps
    finished := s.finished
    parked
    budgetExceeded := s.budgetExceeded
    status
    attempts := s.attempts
    retries := s.retries
    controls := s.controls
    completed := s.completed }

end PetriModel.Control
