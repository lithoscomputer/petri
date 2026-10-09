//! The node's own session's usage, by the route that spent it.
//!
//! `pebble.usage` is one total for the node's session. When Pebble moves the
//! session to a fallback route, a host that prices or reports usage per
//! model cannot tell from that total what each route spent. The node's sink
//! folds the session's own events through [`Accounting`] as they arrive:
//! `SessionStarted` (a fresh session or a resumed export) and
//! `RouteFailover` set the route in effect, and each `AssistantMessage` and
//! `CompactionCompleted` adds its usage, already priced at the model that
//! answered, to that route. Those are the events Pebble bills a prompt for,
//! so the routes' usage sums to `pebble.usage`: a breakdown of it, not an
//! addition. A child's usage is the child's own; the sub-agent ledger
//! reports it.

use std::sync::{Mutex, PoisonError};

use ir::Value;
use lithos_llm::types::Usage;
use pebble_coding_agent::events::{CodingAgentEvent, CodingEvent};
use serde::Serialize;
use serde_json::json;

/// The per-stage metric under `Metrics::custom`: an array of
/// `{ provider, model, usage }`, one entry per route the node's session spent
/// usage on, in the order the routes were first used. `provider` and `model`
/// are named as a `pebble.subagents` session account names them, the provider
/// ID and the model ID apart; `provider` is null when the stream never named
/// it. `usage` is lithos-llm's `Usage`, as `pebble.usage` is. The entries
/// sum to `pebble.usage`; a node whose session used nothing reports an empty
/// array.
pub const METRIC: &str = "pebble.usage_by_model";

/// What one node's own session spent on each route, folded from Pebble's
/// stream as the events arrive. Shared between the node's sink, which folds,
/// and the session, which reports the metric.
#[derive(Debug, Default)]
pub struct Accounting {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The route in effect, once the stream named one.
    route:  Option<Route>,
    /// Each route's usage, in the order the routes were first used.
    routes: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Route {
    provider: Option<String>,
    model:    String,
}

#[derive(Debug, Serialize)]
struct Entry {
    #[serde(flatten)]
    route: Route,
    usage: Usage,
}

impl Route {
    /// A `provider/model` selector, as `RouteFailover` names a route. The
    /// provider ID ends at the first `/`; the model ID may hold more.
    fn from_selector(selector: &str) -> Self {
        match selector.split_once('/') {
            Some((provider, model)) => Self {
                provider: Some(provider.to_owned()),
                model:    model.to_owned(),
            },
            None => Self {
                provider: None,
                model:    selector.to_owned(),
            },
        }
    }
}

impl Accounting {
    /// Fold one event of the session's tree; only the node's own session's
    /// events count.
    pub fn observe(&self, event: &CodingAgentEvent) {
        if event.parent_session_id.is_some() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &event.event {
            CodingEvent::SessionStarted {
                provider,
                model: Some(model),
            } => {
                state.route = Some(Route {
                    provider: provider.clone(),
                    model:    model.clone(),
                });
            }
            CodingEvent::RouteFailover { to, .. } => {
                state.route = Some(Route::from_selector(to));
            }
            // An answer seen before any start still says which model it was
            // priced at.
            CodingEvent::AssistantMessage { model, usage, .. } => {
                let route = state.route.clone().unwrap_or_else(|| Route {
                    provider: None,
                    model:    model.clone(),
                });
                state.add(route, *usage);
            }
            // The summary call ran on the route in effect when the session
            // compacted.
            CodingEvent::CompactionCompleted { usage, .. } => {
                if let Some(route) = state.route.clone() {
                    state.add(route, *usage);
                }
            }
            _ => {}
        }
    }

    /// The [`METRIC`] value.
    #[must_use]
    pub fn metrics(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        json!(state.routes)
    }
}

impl State {
    /// Add `usage` to `route`'s entry. Usage of nothing opens no entry.
    fn add(&mut self, route: Route, usage: Usage) {
        if usage == Usage::default() {
            return;
        }
        if let Some(entry) = self.routes.iter_mut().find(|entry| entry.route == route) {
            entry.usage = entry.usage.saturating_add(usage);
        } else {
            self.routes.push(Entry { route, usage });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use lithos_llm::types::{Cost, CostSource, TokenCounts};
    use pebble_coding_agent::events::{
        CompactionReason, ErrorData, ErrorKind, FailoverContinuation,
    };

    use super::*;

    fn usage(input: u64, usd_micros: Option<u64>) -> Usage {
        Usage {
            tokens: TokenCounts {
                input,
                output: input / 2,
                ..TokenCounts::default()
            },
            cost:   usd_micros.map(|usd_micros| Cost {
                usd_micros,
                source: CostSource::Catalog,
            }),
        }
    }

    fn started(provider: &str, model: &str) -> CodingEvent {
        CodingEvent::SessionStarted {
            provider: Some(provider.into()),
            model:    Some(model.into()),
        }
    }

    fn answer(model: &str, usage: Usage) -> CodingEvent {
        CodingEvent::AssistantMessage {
            text: "ok".into(),
            model: model.into(),
            usage,
            tool_call_count: 0,
            context_window: None,
            reasoning: None,
        }
    }

    fn failover(from: &str, to: &str, usage: Usage) -> CodingEvent {
        CodingEvent::RouteFailover {
            from: from.into(),
            to: to.into(),
            attempt: 2,
            error: ErrorData::new(ErrorKind::Llm, "primary down"),
            usage,
            inference_ms: 0,
            tool_ms: 0,
            continuation: FailoverContinuation::ContinueTurn,
        }
    }

    fn compacted(usage: Usage) -> CodingEvent {
        CodingEvent::CompactionCompleted {
            original_turn_count: 10,
            preserved_turn_count: 6,
            summary_token_estimate: 12,
            tracked_file_count: 0,
            reason: CompactionReason::Threshold,
            usage,
        }
    }

    fn root(event: CodingEvent) -> CodingAgentEvent {
        CodingAgentEvent::new("root", event, SystemTime::UNIX_EPOCH)
    }

    fn child(event: CodingEvent) -> CodingAgentEvent {
        let mut envelope = CodingAgentEvent::new("child", event, SystemTime::UNIX_EPOCH);
        envelope.parent_session_id = Some("root".into());
        envelope
    }

    fn fold(events: &[CodingAgentEvent]) -> Value {
        let accounting = Accounting::default();
        for event in events {
            accounting.observe(event);
        }
        accounting.metrics()
    }

    /// The entries' usage, summed as `pebble.usage` sums the prompts'.
    fn total(metric: &Value) -> Usage {
        metric
            .as_array()
            .expect("an array")
            .iter()
            .map(|entry| serde_json::from_value::<Usage>(entry["usage"].clone()).expect("usage"))
            .fold(Usage::default(), Usage::saturating_add)
    }

    #[test]
    fn the_metric_is_one_entry_per_route_by_provider_and_model() {
        let metric = fold(&[
            root(started("anthropic", "claude-sonnet-5")),
            root(answer("claude-sonnet-5", usage(100, Some(30)))),
        ]);
        assert_eq!(
            metric,
            json!([{
                "provider": "anthropic",
                "model": "claude-sonnet-5",
                "usage": {
                    "tokens": {
                        "input": 100, "output": 50, "reasoning": 0,
                        "cache_read": 0, "cache_write": 0
                    },
                    "cost": { "usd_micros": 30, "source": "catalog" }
                }
            }])
        );
    }

    #[test]
    fn a_failover_splits_the_usage_between_the_routes_in_first_use_order() {
        let metric = fold(&[
            root(started("anthropic", "claude-sonnet-5")),
            root(answer("claude-sonnet-5", usage(100, Some(30)))),
            root(compacted(usage(20, Some(4)))),
            root(failover(
                "anthropic/claude-sonnet-5",
                "openai/gpt-5.4",
                usage(120, Some(34)),
            )),
            root(answer("gpt-5.4", usage(40, Some(9)))),
            root(answer("gpt-5.4", usage(10, Some(2)))),
        ]);
        let entries = metric.as_array().expect("an array");
        assert_eq!(entries.len(), 2, "{metric}");
        assert_eq!(entries[0]["provider"], "anthropic");
        assert_eq!(entries[0]["model"], "claude-sonnet-5");
        assert_eq!(entries[0]["usage"]["tokens"]["input"], 120);
        assert_eq!(entries[0]["usage"]["cost"]["usd_micros"], 34);
        assert_eq!(entries[1]["provider"], "openai");
        assert_eq!(entries[1]["model"], "gpt-5.4");
        assert_eq!(entries[1]["usage"]["tokens"]["input"], 50);
        assert_eq!(entries[1]["usage"]["cost"]["usd_micros"], 11);
        // The failover's own usage is a breakdown of the answers already
        // counted, never an addition.
        assert_eq!(total(&metric), usage(170, Some(45)));
    }

    #[test]
    fn a_resumed_session_counts_on_the_route_it_started_on() {
        let metric = fold(&[
            root(started("openai", "gpt-5.4")),
            root(answer("gpt-5.4", usage(10, None))),
        ]);
        assert_eq!(metric[0]["provider"], "openai");
        assert_eq!(metric[0]["model"], "gpt-5.4");
        assert!(metric[0]["usage"].get("cost").is_none(), "{metric}");
    }

    #[test]
    fn a_model_id_keeps_its_slashes_after_the_provider() {
        let metric = fold(&[
            root(started("openrouter", "anthropic/claude-sonnet-5")),
            root(failover(
                "openrouter/anthropic/claude-sonnet-5",
                "openrouter/meta-llama/llama-5",
                Usage::default(),
            )),
            root(answer("meta-llama/llama-5", usage(10, Some(1)))),
        ]);
        assert_eq!(
            metric,
            json!([{
                "provider": "openrouter",
                "model": "meta-llama/llama-5",
                "usage": usage(10, Some(1)),
            }])
        );
    }

    #[test]
    fn a_route_that_spent_nothing_has_no_entry() {
        assert_eq!(fold(&[root(started("test", "model"))]), json!([]));
        let metric = fold(&[
            root(started("test", "model")),
            root(answer("model", Usage::default())),
            root(failover("test/model", "test/small", Usage::default())),
            root(answer("small", usage(10, None))),
        ]);
        assert_eq!(metric.as_array().map(Vec::len), Some(1), "{metric}");
        assert_eq!(metric[0]["model"], "small");
    }

    #[test]
    fn a_childs_usage_and_route_are_the_childs_own() {
        let metric = fold(&[
            root(started("anthropic", "claude-sonnet-5")),
            child(started("openai", "gpt-5.4")),
            child(answer("gpt-5.4", usage(500, Some(90)))),
            child(compacted(usage(50, Some(9)))),
            root(answer("claude-sonnet-5", usage(10, Some(3)))),
        ]);
        assert_eq!(
            metric,
            json!([{
                "provider": "anthropic",
                "model": "claude-sonnet-5",
                "usage": usage(10, Some(3)),
            }])
        );
    }

    #[test]
    fn an_answer_before_any_start_is_counted_under_its_own_model() {
        let metric = fold(&[root(answer("model", usage(10, None)))]);
        assert_eq!(metric[0]["provider"], Value::Null);
        assert_eq!(metric[0]["model"], "model");
    }
}
