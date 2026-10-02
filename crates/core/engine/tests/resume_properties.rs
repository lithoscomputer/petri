//! §6 resume over random flows, loops, retries, stops and decisions the host
//! holds open included. A run's log cut at any record, between two host events
//! or inside one before the core's records for it were flushed, resumes to the
//! state the live run had after its last whole host event. It owes exactly
//! what the live run had outstanding there, and fed the rest of the host's
//! events it writes the same log byte for byte: resume is invisible in the
//! log. A log that is no prefix of its own replay is refused.

mod flow;
mod support;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::mem;
use std::rc::Rc;

use engine::{
    Command, DecisionId, EngineState, Event, EventLog, EventOrigin, ResumePoint, apply, resume,
};
use flow::FlowCase;
use ir::FiringId;
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use serde::Serialize;

/// What the live run still owed after a host event: the last `StartStep` and
/// `ScheduleRetry` it issued for each firing, and the admissions and routing
/// decisions it had not answered yet.
#[derive(Clone, Default)]
struct Owed {
    starts:    BTreeMap<FiringId, Command>,
    retries:   BTreeMap<FiringId, Command>,
    decisions: BTreeMap<DecisionId, Command>,
}

/// The live run as it stood after one host event.
struct Boundary {
    state: EngineState,
    owed:  Owed,
}

/// Byte encoding, the comparison `resume` itself makes: `PartialEq` on JSON
/// maps ignores key order.
fn bytes<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("logs and commands always encode")
}

/// Two states are the same when they print the same. JSON cannot encode the
/// state's maps keyed by tuples, and `PartialEq` finds a NaN backoff factor
/// unequal to itself; every map in the state is ordered, so the printout is
/// stable.
fn same_state(a: &EngineState, b: &EngineState) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

/// Rerun a case and keep the live run's state and debts after each of the
/// wanted host events, counted from 1.
fn boundaries(case: &FlowCase, wanted: BTreeSet<usize>) -> BTreeMap<usize, Boundary> {
    let kept: Rc<RefCell<BTreeMap<usize, Boundary>>> = Rc::default();
    let sink = Rc::clone(&kept);
    let mut owed = Owed::default();
    let mut applied = 0;
    let mut logged = 0;
    flow::run_observed(case, move |state, commands| {
        applied += 1;
        // The host event this `apply` took is the first record it appended.
        if let Event::AdmissionDecided { decision_id, .. }
        | Event::RoutingResolved { decision_id, .. } = &state.log.records()[logged].event
        {
            owed.decisions.remove(decision_id);
        }
        logged = state.log.len();
        for command in commands {
            match command {
                Command::StartStep(resolved) => {
                    owed.starts.insert(resolved.id(), command.clone());
                }
                Command::ScheduleRetry { firing, .. } => {
                    owed.retries.insert(*firing, command.clone());
                }
                Command::Admit { decision_id } | Command::ResolveRouting { decision_id, .. } => {
                    owed.decisions.insert(*decision_id, command.clone());
                }
                _ => {}
            }
        }
        // A stop withdraws the admissions of the firings it settles, and a
        // kill the routings of what it reached: the driver drops their late
        // answers, and a resumed one does not ask again.
        owed.decisions
            .retain(|id, _| state.has_pending_admission(*id) || state.has_pending_routing(*id));
        if wanted.contains(&applied) {
            sink.borrow_mut().insert(applied, Boundary {
                state: state.clone(),
                owed:  owed.clone(),
            });
        }
    });
    mem::take(&mut *kept.borrow_mut())
}

/// §6: the effects a resumed driver owes, in dispatch order: `AcquireScope`
/// per held scope, the unanswered admissions and routing decisions, then per
/// live firing not awaiting admission its last `StartStep`, or its last
/// `ScheduleRetry` when it waits on a backoff. It re-dispatches the live
/// firings that neither wait on a backoff nor are cancelling. Read off the
/// live run, not replayed.
fn check_owed(point: &ResumePoint, live: &EngineState, owed: &Owed) -> Result<(), TestCaseError> {
    let scopes: Vec<Command> = live
        .held_scopes()
        .map(|scope| Command::AcquireScope { scope })
        .collect();
    let mut firings = Vec::new();
    let mut redispatched = Vec::new();
    for firing in live.live_firings() {
        if live.is_awaiting_admission(firing.id) {
            continue;
        }
        let last = if firing.awaiting_retry {
            owed.retries.get(&firing.id)
        } else {
            owed.starts.get(&firing.id)
        };
        prop_assert!(
            last.is_some(),
            "{} is live, but the live run never started it",
            firing.id
        );
        firings.extend(last.cloned());
        if !firing.awaiting_retry && !firing.cancelling {
            redispatched.push(firing.id);
        }
    }

    prop_assert_eq!(
        point.pending.len(),
        scopes.len() + owed.decisions.len() + firings.len(),
        "pending: {:?}",
        point.pending
    );
    let (held, rest) = point.pending.split_at(scopes.len());
    let (decisions, started) = rest.split_at(owed.decisions.len());
    prop_assert!(bytes(held) == bytes(&scopes), "held scopes: {held:?}");
    let decisions: BTreeSet<Vec<u8>> = decisions.iter().map(bytes).collect();
    let expected: BTreeSet<Vec<u8>> = owed.decisions.values().map(bytes).collect();
    prop_assert!(decisions == expected, "decisions: {rest:?}");
    prop_assert!(bytes(started) == bytes(&firings), "firings: {started:?}");
    prop_assert_eq!(&point.redispatched, &redispatched);
    Ok(())
}

fn check_resume(case: &FlowCase, cuts: &[Index], tamper: Index) -> Result<(), TestCaseError> {
    let run = flow::run(case);
    let graph = &run.harness.original_graph;
    let log = &run.harness.state.log;
    let records = log.records();
    // Where each host event's records begin: resume replays whole host
    // events.
    let events: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.origin == EventOrigin::External)
        .map(|(at, _)| at)
        .collect();
    let cuts: Vec<usize> = cuts
        .iter()
        .map(|cut| cut.index(records.len() + 1))
        .collect();
    let taken = |cut: usize| events.iter().filter(|at| **at < cut).count();
    let live = boundaries(case, cuts.iter().map(|cut| taken(*cut)).collect());

    for cut in cuts {
        let k = taken(cut);
        let point = resume(graph.clone(), &log.prefix(cut)).map_err(|error| {
            TestCaseError::fail(format!(
                "a log cut after {cut} of {} records was refused: {error}",
                records.len()
            ))
        })?;

        // Replay regenerates the rest of the last host event's records.
        let end = events.get(k).copied().unwrap_or(records.len());
        prop_assert!(
            bytes(&point.state.log) == bytes(&log.prefix(end)),
            "resumed after {} records, the log has {} records, not {}",
            cut,
            point.state.log.len(),
            end
        );
        // The state is the live run's after that event.
        let fresh = Boundary {
            state: EngineState::new(graph.clone()),
            owed:  Owed::default(),
        };
        let boundary = if k == 0 { &fresh } else { &live[&k] };
        prop_assert!(
            same_state(&point.state, &boundary.state),
            "resumed after {} records, the state differs from the live run's",
            cut
        );
        check_owed(&point, &boundary.state, &boundary.owed)?;

        // Resume is invisible: the rest of the host's events write the same
        // log.
        let mut state = point.state;
        for event in log.external_events().skip(k) {
            state = apply(state, event.clone()).0;
        }
        prop_assert!(
            bytes(&state.log) == bytes(log),
            "resumed after {} records, the run went on to a different log",
            cut
        );
        prop_assert_eq!(state.exit(), run.harness.state.exit());
    }

    // A log that is no prefix of its own replay is refused: two core records
    // in the wrong order, or a core record the replay does not produce.
    let swaps: Vec<usize> = (1..records.len())
        .filter(|at| {
            records[at - 1].origin == EventOrigin::Core
                && records[*at].origin == EventOrigin::Core
                && bytes(&records[at - 1].event) != bytes(&records[*at].event)
        })
        .collect();
    if !swaps.is_empty() {
        let at = swaps[tamper.index(swaps.len())];
        let mut swapped = records.to_vec();
        let (earlier, later) = (swapped[at - 1].event.clone(), swapped[at].event.clone());
        swapped[at - 1].event = later;
        swapped[at].event = earlier;
        let swapped = EventLog::try_from_records(log.version(), swapped)
            .expect("the records stay contiguous");
        let refused = resume(graph.clone(), &swapped).err();
        prop_assert_eq!(
            refused.and_then(|mismatch| mismatch.first_divergence),
            Some(records[at - 1].seq),
            "two core records swapped at seq {}",
            records[at - 1].seq
        );
    }
    // A host event past the end is only more input; a core record is not.
    if let Some(core) = records
        .iter()
        .rev()
        .find(|record| record.origin == EventOrigin::Core)
    {
        let mut longer = records.to_vec();
        longer.push(engine::EventRecord {
            seq: records.len() as u64,
            ..core.clone()
        });
        let longer =
            EventLog::try_from_records(log.version(), longer).expect("the records stay contiguous");
        prop_assert!(
            resume(graph.clone(), &longer).is_err(),
            "a record past the end of the replay was accepted"
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// A log cut anywhere resumes to the live run's state, owes what the live
    /// run owed, and goes on to the same log; a log that is no prefix of its
    /// replay is refused — for every generated graph, host schedule and cut.
    #[test]
    fn random_flows_resume_from_any_cut_of_their_log(
        case in flow::flow_case(),
        cuts in prop::collection::vec(any::<Index>(), 4),
        tamper in any::<Index>(),
    ) {
        check_resume(&case, &cuts, tamper)?;
    }
}
