//! The stage's identity on its outbound requests: every model call a native
//! stage makes and every request to an `http` MCP server carries the run and
//! the step that made it as HTTP headers, so a proxy between Petri and a
//! provider or a server can attribute the exchange and match it to the
//! run's public events.
//!
//! The values are the ones the public events carry, so a request joins to
//! the stage's events on them: [`RUN_HEADER`] is the run key (the host's run
//! id; Fabro's run ULID), [`INVOCATION_HEADER`] and [`EXECUTION_HEADER`] are
//! the event's `context.invocation` and `context.execution`, and
//! [`NODE_HEADER`], [`FIRING_HEADER`] and [`ATTEMPT_HEADER`] are its
//! `subject.node.name`, `subject.firing` and `subject.attempt`. The run,
//! invocation and execution come from the coordinator's
//! [`ExecutionIdentity`]; a driver built outside the coordinator has none and
//! sends the three step headers alone. A node name outside visible ASCII is
//! percent-encoded (as is `%`), since a header value cannot carry it.
//!
//! A sub-agent shares its stage's model client, so its calls carry its
//! stage's headers. A header the configuration names wins: a provider's
//! catalog `default_headers` take precedence over a client's defaults in
//! `lithos-llm`, and an MCP server's configured `headers` entry of the same
//! name (compared case-insensitively) replaces the stage's.
//!
//! The stage also carries W3C trace context: the step attempt's span in its
//! run's trace ([`execution::StepTrace`]) and the baggage entries
//! [`RUN_ID_BAGGAGE`] (the run key) and [`RUN_STEP_BAGGAGE`] (the node's
//! instance name). An agent node hands both to its Pebble session
//! ([`Propagation::pebble`]), which sends a `traceparent` naming its own span
//! per model request and per tool call beneath the step's, and adds its
//! `session_id` and `tool_call_id` to the baggage. A prompt node's one call
//! goes through no session, so its client sends the step's own
//! `traceparent` and `baggage` ([`prompt_client`]). Without the
//! coordinator's identity there is no run, so no trace context is sent.

use std::fmt::Write as _;

use execution::{ExecutionIdentity, StepTrace};
use lithos_llm::Client;
use pebble_coding_agent::{TraceContext, TraceContextError};
use steps::StepCtx;

/// The run key the run is stored and labelled under.
pub const RUN_HEADER: &str = "x-petri-run-id";
/// The invocation the stage runs in: `context.invocation`.
pub const INVOCATION_HEADER: &str = "x-petri-invocation";
/// The execution the stage runs in: `context.execution`.
pub const EXECUTION_HEADER: &str = "x-petri-execution";
/// The node's instance name: `subject.node.name`.
pub const NODE_HEADER: &str = "x-petri-node";
/// The firing: `subject.firing`.
pub const FIRING_HEADER: &str = "x-petri-firing";
/// The attempt, 1-based: `subject.attempt`.
pub const ATTEMPT_HEADER: &str = "x-petri-attempt";

/// The W3C trace context header.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// The W3C baggage header.
pub const BAGGAGE_HEADER: &str = "baggage";
/// The baggage entry holding the run key.
pub const RUN_ID_BAGGAGE: &str = "run_id";
/// The baggage entry holding the node's instance name.
pub const RUN_STEP_BAGGAGE: &str = "run_step";

/// The stage's identity headers, in a fixed order.
pub(crate) fn headers(ctx: &StepCtx) -> Vec<(&'static str, String)> {
    let mut headers = Vec::with_capacity(6);
    if let Some(identity) = ctx.capability::<ExecutionIdentity>() {
        headers.push((RUN_HEADER, header_value(identity.run.as_str())));
        headers.push((INVOCATION_HEADER, identity.invocation.to_string()));
        headers.push((EXECUTION_HEADER, identity.execution.to_string()));
    }
    headers.push((NODE_HEADER, header_value(&ctx.node)));
    headers.push((FIRING_HEADER, ctx.firing.0.to_string()));
    headers.push((ATTEMPT_HEADER, ctx.attempt.0.to_string()));
    headers
}

/// The application's client with the stage's headers on every request.
pub(crate) fn client(client: &Client, ctx: &StepCtx) -> Result<Client, lithos_llm::Error> {
    client.with_default_headers(headers(ctx))
}

/// The trace context a stage's requests carry: the `traceparent` of the
/// span they go beneath, and the run and the step for the baggage.
pub struct Propagation {
    traceparent: String,
    run:         String,
    step:        String,
}

impl Propagation {
    /// Beneath `traceparent`, for the step `step` of the run `run`.
    pub fn new(traceparent: String, run: &str, step: &str) -> Self {
        Self {
            traceparent,
            run: run.to_owned(),
            step: step.to_owned(),
        }
    }

    /// Beneath the step attempt's own span. `None` outside the
    /// coordinator, which names the run.
    pub(crate) fn of_step(ctx: &StepCtx) -> Option<Self> {
        let identity = ctx.capability::<ExecutionIdentity>()?;
        let trace = StepTrace::of(&identity, ctx.firing, ctx.attempt);
        Some(Self::new(
            trace.traceparent(),
            identity.run.as_str(),
            &ctx.node,
        ))
    }

    fn baggage(&self) -> [(&'static str, &str); 2] {
        [(RUN_ID_BAGGAGE, &self.run), (RUN_STEP_BAGGAGE, &self.step)]
    }

    /// `traceparent` and `baggage`, for a call made outside any Pebble
    /// session.
    pub(crate) fn headers(&self) -> [(&'static str, String); 2] {
        [
            (TRACEPARENT_HEADER, self.traceparent.clone()),
            (BAGGAGE_HEADER, baggage_header(&self.baggage())),
        ]
    }

    /// The parent a Pebble session opens its spans under, with the run and
    /// the step in its baggage.
    pub(crate) fn pebble(&self) -> Result<TraceContext, TraceContextError> {
        self.baggage().into_iter().try_fold(
            TraceContext::from_traceparent(&self.traceparent)?,
            |context, (key, value)| context.with_baggage(key, value),
        )
    }
}

/// A prompt node's client: the stage's identity headers, and the step's own
/// `traceparent` and `baggage`, since no Pebble session stands between the
/// node and its model call.
pub(crate) fn prompt_client(client: &Client, ctx: &StepCtx) -> Result<Client, lithos_llm::Error> {
    let mut headers = headers(ctx);
    if let Some(propagation) = Propagation::of_step(ctx) {
        headers.extend(propagation.headers());
    }
    client.with_default_headers(headers)
}

/// A W3C `baggage` value: `key=value` entries joined by `,`, each value
/// percent-encoded outside the baggage octets (and `%` itself).
fn baggage_header(entries: &[(&str, &str)]) -> String {
    let mut header = String::new();
    for (key, value) in entries {
        if !header.is_empty() {
            header.push(',');
        }
        header.push_str(key);
        header.push('=');
        header.push_str(&percent_encoded(value, |byte| {
            !matches!(byte, b'"' | b',' | b';' | b'\\')
        }));
    }
    header
}

/// `text` as a header value: visible ASCII stays, every other byte and `%`
/// become `%XX`.
fn header_value(text: &str) -> String {
    percent_encoded(text, |_| true)
}

/// `text` with every byte that is not visible ASCII, is `%`, or fails
/// `keep` as `%XX`.
fn percent_encoded(text: &str, keep: impl Fn(u8) -> bool) -> String {
    let mut value = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_graphic() && byte != b'%' && keep(byte) {
            value.push(char::from(byte));
        } else {
            let _ = write!(value, "%{byte:02X}");
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::{baggage_header, header_value};

    #[test]
    fn a_node_name_outside_visible_ascii_is_percent_encoded() {
        assert_eq!(header_value("build#2"), "build#2");
        assert_eq!(header_value("a b%é"), "a%20b%25%C3%A9");
    }

    /// A baggage value also encodes the octets W3C baggage reserves for
    /// its own syntax, so a value cannot end its entry or start another.
    #[test]
    fn a_baggage_value_encodes_the_baggage_delimiters() {
        assert_eq!(
            baggage_header(&[
                ("run_id", "01JDQ7ZK3V8Y2M4N6P8R0T2W4X"),
                ("run_step", "a,b;c=\"d\\ é%#2"),
            ]),
            "run_id=01JDQ7ZK3V8Y2M4N6P8R0T2W4X,run_step=a%2Cb%3Bc=%22d%5C%20%C3%A9%25#2"
        );
    }
}
