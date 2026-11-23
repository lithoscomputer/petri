//! The run's `[run.agent.mcps]` servers on a native session, through Pebble's
//! `mcp` feature.
//!
//! Pebble owns the servers. The session names them to the builder
//! (`CodingAgentBuilder::mcp_servers`); Pebble starts each one while it builds
//! the agent, registers every tool it advertises as a `RegisteredTool` whose
//! source is `ToolSource::Mcp` under `mcp__<server>__<tool>` (the name Fabro
//! gives it, and the name `frontend_attractor::mcps::qualified_tool_name`
//! computes), runs the calls through its normal tool path (so Pebble's
//! middleware, history, output policy, cancellation and agent events apply to
//! an MCP tool as to any other), and closes the servers when the agent shuts
//! down. What stays Petri's is the mapping from Fabro's entries to Pebble's
//! servers ([`pebble_servers`]) and the event contract ([`Mirror`]).
//!
//! Placement follows Fabro: a `stdio` server is a child process of Petri's
//! host (Fabro's run worker), never of the sandbox; its working directory is
//! the scope's workspace when the scope shares the host filesystem, else
//! Petri's own. An `http` server is reached from the host, over streamable
//! HTTP or, with `protocol = "sse"`, the older SSE transport. A `sandbox`
//! server is launched in the scope's execution environment and reached over
//! the same two protocols through the environment's route to its port
//! (`ExecEnv::preview_url`, handed to Pebble as its own `PortRoutes` contract
//! by [`super::environment::ScopePortRoutes`]): the host's own loopback, the
//! Docker plugin's forward into the container, or Daytona's preview link with
//! its token header. A streamable HTTP server is reached
//! at the route itself; an SSE server serves its event stream at `/sse`
//! under it ([`SSE_PATH`]), where Fabro has always reached one. Pebble
//! releases the route when the server stops. Secrets in `env` and `headers`
//! are resolved here, when the servers are named, and never written down.
//! An `http` server's requests also carry the stage's identity headers
//! ([`super::identity`]) beneath its configured `headers`: a configured
//! header of the same name, in any case, is sent instead of the stage's.
//!
//! Failure behavior follows Fabro: a server that does not start (a spawn
//! error, a handshake timeout, a protocol error) is reported by Pebble
//! (`McpServerFailed`) and skipped, and the session proceeds with the tools
//! of the servers that started; the session puts the reason on the node's
//! stderr. A tool result the server marks `isError` reaches the model as the
//! tool's error text; a transport failure, a timeout or a cancellation
//! reaches it as a failed call with a reason. A server whose connection
//! closes during the session is reported by Pebble (`McpServerDisconnected`,
//! once, by the call that first found it closed), and every later call to
//! it fails at once. When Pebble's build fails after the servers started,
//! Pebble shuts them down before it reports the failure, so nothing leaks.
//!
//! Pebble's events are the record of the servers and their calls:
//! `McpServerReady`, `McpServerFailed` and `McpServerDisconnected` for the
//! servers, `ToolCallStarted` and `ToolCallCompleted` under
//! `mcp__<server>__<tool>` for the calls, all in the session's
//! `agent_activity`. Petri restates none of them. The one fact Pebble cannot
//! know is a server Petri never named to it because a secret the entry needs
//! is unavailable: that is [`UNAVAILABLE_EVENT`]. A retained session's
//! successor node names the same servers again, so Pebble starts its own and
//! registers the same tool names, and the conversation's earlier tool calls
//! stay valid (`crate::sessions`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use executor::{ExecEnv, SecretProvider};
use frontend_attractor::mcps::{McpHttpProtocol, McpServer, McpTransport, McpValue};
use pebble_coding_agent::mcp::{
    McpHttpProtocol as PebbleProtocol, McpPlacement, McpServer as PebbleServer,
};

/// Where a sandbox-hosted SSE server serves its event stream, under the
/// environment's route to its port: the path Fabro reaches one at.
pub const SSE_PATH: &str = "/sse";

/// The `kind` of the `StepEvent::Custom` payload for a server Petri never
/// named to Pebble because a secret its `env` or `headers` needs is
/// unavailable: `{ kind, node, firing, attempt, server, error }`. Emitted
/// once per such server, before the agent is built. Every other fact about
/// the servers is Pebble's own event.
pub const UNAVAILABLE_EVENT: &str = "attractor.mcp.unavailable";

/// Fabro's entries as Pebble's servers, in configuration order, their
/// secrets resolved.
pub(super) struct Prepared {
    /// The servers Pebble starts.
    pub(super) servers:     Vec<PebbleServer>,
    /// The servers whose secret is unavailable, with the reason: reported
    /// `failed` and never named to Pebble.
    pub(super) unavailable: Vec<(String, String)>,
}

/// Map every configured server onto Pebble's, resolving `{"$secret": name}`
/// values through the run's secret provider now and nowhere else; the
/// resolved values go into the process or the request and are never written
/// down. `identity` is the stage's identity headers, added to every `http`
/// server's headers that do not already name them.
pub(super) fn pebble_servers(
    servers: &[McpServer],
    env: &dyn ExecEnv,
    secrets: &dyn SecretProvider,
    identity: &[(&str, String)],
) -> Prepared {
    let mut prepared = Prepared {
        servers:     Vec::with_capacity(servers.len()),
        unavailable: Vec::new(),
    };
    for server in servers {
        match pebble_server(server, env, secrets, identity) {
            Ok(server) => prepared.servers.push(server),
            Err(error) => prepared
                .unavailable
                .push((server.name.clone(), error.to_string())),
        }
    }
    prepared
}

/// A secret a transport names that the run cannot supply.
#[derive(Debug, thiserror::Error)]
#[error("secret `{name}` for `{field}` is unavailable: {reason}")]
struct SecretUnavailable {
    name:   String,
    field:  String,
    reason: String,
}

fn pebble_server(
    server: &McpServer,
    env: &dyn ExecEnv,
    secrets: &dyn SecretProvider,
    identity: &[(&str, String)],
) -> Result<PebbleServer, SecretUnavailable> {
    let placement = match &server.transport {
        McpTransport::Stdio { command, env: vars } => McpPlacement::Stdio {
            command:     command.clone(),
            env:         resolve_values(vars, secrets, "env")?,
            current_dir: env
                .shares_host_filesystem()
                .then(|| PathBuf::from(env.workspace_path())),
            clear_env:   false,
        },
        McpTransport::Http {
            protocol,
            url,
            headers,
        } => {
            let mut headers = resolve_values(headers, secrets, "headers")?;
            for (name, value) in identity {
                if !headers.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                    headers.insert((*name).to_owned(), value.clone());
                }
            }
            McpPlacement::Http {
                url: url.clone(),
                headers,
                protocol: pebble_protocol(*protocol),
            }
        }
        McpTransport::Sandbox {
            protocol,
            command,
            port,
            env: vars,
        } => McpPlacement::Environment {
            command:  command.clone(),
            port:     *port,
            env:      resolve_values(vars, secrets, "env")?,
            protocol: pebble_protocol(*protocol),
            path:     matches!(protocol, McpHttpProtocol::Sse).then(|| SSE_PATH.to_owned()),
        },
    };
    Ok(PebbleServer::new(server.name.clone(), placement)
        .with_startup_timeout(Duration::from_millis(server.startup_timeout_ms))
        .with_tool_timeout(Duration::from_millis(server.tool_timeout_ms)))
}

fn pebble_protocol(protocol: McpHttpProtocol) -> PebbleProtocol {
    match protocol {
        McpHttpProtocol::StreamableHttp => PebbleProtocol::StreamableHttp,
        McpHttpProtocol::Sse => PebbleProtocol::Sse,
    }
}

fn resolve_values(
    values: &BTreeMap<String, McpValue>,
    secrets: &dyn SecretProvider,
    field: &str,
) -> Result<BTreeMap<String, String>, SecretUnavailable> {
    let mut out = BTreeMap::new();
    for (key, value) in values {
        let text = match value {
            McpValue::Literal(text) => text.clone(),
            McpValue::Secret { name } => secrets
                .resolve(name)
                .map_err(|error| SecretUnavailable {
                    name:   name.clone(),
                    field:  format!("{field}.{key}"),
                    reason: error.to_string(),
                })?
                .expose()
                .to_string(),
        };
        out.insert(key.clone(), text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::slice;

    use executor::{EnvError, MapSecrets, ProcessHandle, ProcessSpec};

    use super::*;

    /// An environment that runs nothing and shares no filesystem: the
    /// mapping needs only its workspace answers.
    #[derive(Debug)]
    struct NoEnv;

    #[async_trait::async_trait]
    impl ExecEnv for NoEnv {
        async fn spawn(&self, _spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
            Err(EnvError::backend("none", "spawn", "runs nothing"))
        }

        fn workspace_path(&self) -> &'static str {
            "/work"
        }

        async fn read_file(&self, _relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
            Ok(None)
        }

        async fn write_file(&self, _relative: &Path, _: &[u8]) -> Result<(), EnvError> {
            Ok(())
        }

        fn grace(&self) -> Duration {
            Duration::from_secs(1)
        }
    }

    fn server(name: &str, transport: McpTransport) -> McpServer {
        McpServer {
            name: name.to_owned(),
            transport,
            startup_timeout_ms: 1_000,
            tool_timeout_ms: 2_000,
            source: "workflow.toml".to_owned(),
        }
    }

    fn placement_of(server: &McpServer) -> McpPlacement {
        let prepared = pebble_servers(slice::from_ref(server), &NoEnv, &MapSecrets::empty(), &[]);
        assert!(
            prepared.unavailable.is_empty(),
            "{:?}",
            prepared.unavailable
        );
        assert_eq!(prepared.servers.len(), 1);
        assert_eq!(prepared.servers[0].name(), server.name);
        assert_eq!(
            prepared.servers[0].startup_timeout(),
            Duration::from_secs(1)
        );
        assert_eq!(prepared.servers[0].tool_timeout(), Duration::from_secs(2));
        prepared.servers[0].placement().clone()
    }

    /// An `http` server keeps its protocol: SSE is Pebble's `Sse` at the
    /// configured URL, with no path of Petri's added.
    #[test]
    fn an_http_server_maps_to_pebbles_protocol_at_its_url() {
        let sse = server("legacy", McpTransport::Http {
            protocol: McpHttpProtocol::Sse,
            url:      "http://127.0.0.1:1/sse".into(),
            headers:  BTreeMap::from([("X-Case".to_owned(), McpValue::Literal("sse".into()))]),
        });
        match placement_of(&sse) {
            McpPlacement::Http {
                url,
                headers,
                protocol,
            } => {
                assert_eq!(url, "http://127.0.0.1:1/sse");
                assert_eq!(
                    headers,
                    BTreeMap::from([("X-Case".to_owned(), "sse".to_owned())])
                );
                assert_eq!(protocol, PebbleProtocol::Sse);
            }
            other => panic!("expected an http placement, got {other:?}"),
        }
        let current = server("current", McpTransport::Http {
            protocol: McpHttpProtocol::StreamableHttp,
            url:      "http://127.0.0.1:1/mcp".into(),
            headers:  BTreeMap::new(),
        });
        match placement_of(&current) {
            McpPlacement::Http { protocol, .. } => {
                assert_eq!(protocol, PebbleProtocol::StreamableHttp);
            }
            other => panic!("expected an http placement, got {other:?}"),
        }
    }

    /// An `http` server gets the stage's identity headers beside its own,
    /// and a configured header of the same name, in any case, is sent
    /// instead of the stage's.
    #[test]
    fn an_http_server_gets_the_identity_headers_its_configuration_does_not_name() {
        let remote = server("remote", McpTransport::Http {
            protocol: McpHttpProtocol::StreamableHttp,
            url:      "http://127.0.0.1:1/mcp".into(),
            headers:  BTreeMap::from([
                (
                    "X-Petri-Node".to_owned(),
                    McpValue::Literal("configured".into()),
                ),
                ("X-Case".to_owned(), McpValue::Literal("kept".into())),
            ]),
        });
        let identity = [
            ("x-petri-node", "build#2".to_owned()),
            ("x-petri-firing", "7".to_owned()),
        ];
        let prepared = pebble_servers(
            slice::from_ref(&remote),
            &NoEnv,
            &MapSecrets::empty(),
            &identity,
        );
        match prepared.servers[0].placement() {
            McpPlacement::Http { headers, .. } => assert_eq!(
                headers,
                &BTreeMap::from([
                    ("X-Case".to_owned(), "kept".to_owned()),
                    ("X-Petri-Node".to_owned(), "configured".to_owned()),
                    ("x-petri-firing".to_owned(), "7".to_owned()),
                ])
            ),
            other => panic!("expected an http placement, got {other:?}"),
        }
    }

    /// A `sandbox` server is an environment placement; an SSE one serves its
    /// stream at `/sse` under the route to its port, as Fabro reaches it, and
    /// a streamable HTTP one is reached at the route itself.
    #[test]
    fn a_sandbox_server_maps_to_an_environment_placement_with_fabros_sse_path() {
        let sse = server("browser", McpTransport::Sandbox {
            protocol: McpHttpProtocol::Sse,
            command:  vec!["npx".into(), "@playwright/mcp".into()],
            port:     3100,
            env:      BTreeMap::from([("MODE".to_owned(), McpValue::Literal("test".into()))]),
        });
        match placement_of(&sse) {
            McpPlacement::Environment {
                command,
                port,
                env,
                protocol,
                path,
            } => {
                assert_eq!(command, ["npx", "@playwright/mcp"]);
                assert_eq!(port, 3100);
                assert_eq!(
                    env,
                    BTreeMap::from([("MODE".to_owned(), "test".to_owned())])
                );
                assert_eq!(protocol, PebbleProtocol::Sse);
                assert_eq!(path.as_deref(), Some(SSE_PATH));
            }
            other => panic!("expected an environment placement, got {other:?}"),
        }
        let current = server("plain", McpTransport::Sandbox {
            protocol: McpHttpProtocol::StreamableHttp,
            command:  vec!["server".into()],
            port:     3200,
            env:      BTreeMap::new(),
        });
        match placement_of(&current) {
            McpPlacement::Environment { protocol, path, .. } => {
                assert_eq!(protocol, PebbleProtocol::StreamableHttp);
                assert!(path.is_none(), "reached at the route itself: {path:?}");
            }
            other => panic!("expected an environment placement, got {other:?}"),
        }
    }
}
