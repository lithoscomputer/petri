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

use std::fmt::Write as _;

use execution::ExecutionIdentity;
use lithos_llm::Client;
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

/// `text` as a header value: visible ASCII stays, every other byte and `%`
/// become `%XX`.
fn header_value(text: &str) -> String {
    let mut value = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_graphic() && byte != b'%' {
            value.push(char::from(byte));
        } else {
            let _ = write!(value, "%{byte:02X}");
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::header_value;

    #[test]
    fn a_node_name_outside_visible_ascii_is_percent_encoded() {
        assert_eq!(header_value("build#2"), "build#2");
        assert_eq!(header_value("a b%é"), "a%20b%25%C3%A9");
    }
}
