//! Fabro's `[run.agent.mcps.<name>]` entries: the MCP servers a native agent
//! node connects to, read from the settings layers the way Fabro reads them
//! into the resolved definitions the agent step carries
//! ([`frontend_attractor::mcps`]).
//!
//! The layers, lowest first: `~/.fabro/settings.toml` (the host's
//! [`SETTINGS_HOOKS_VAR`] variable), `.fabro/project.toml`, `workflow.toml`.
//! Entries are keyed by name; a higher layer's entry replaces a lower one with
//! the same name whole (Fabro's `StickyMap`), and an entry with
//! `enabled = false` in any layer removes the name. Fabro's own parser is
//! `fabro-config/src/layers/run.rs` (`McpEntryLayer`) and
//! `resolve/run.rs` (`resolve_mcp_entry`) at the pinned revision; this
//! module keeps its shape, its field rules and its defaults.
//!
//! `{{ inputs.* }}`, `{{ vars.* }}` and `{{ goal }}` substitute at load.
//! `{{ secrets.NAME }}` may stand alone as an `env` or `headers` value and
//! becomes a [`McpValue::Secret`] the step resolves when it launches the
//! server. `{{ env.* }}` is refused, as Fabro refuses it before launch.
//!
//! A reference entry (`[run.agent.mcps.<name>] id = "<catalog id>"`) names
//! a server of the host's catalog, which a Fabro server keeps outside the
//! settings files. The host binds the catalog as the [`MCP_CATALOG_VAR`]
//! variable ([`McpCatalog`]); the entry it names is read with the inline
//! rules under `<name>`, as Fabro's `resolve_mcp_entries` resolves it. The
//! standalone runner binds no catalog and refuses the reference.

use std::collections::BTreeMap;

use frontend::{CompileInputs, Diagnostics, FileSource, Span};
pub use frontend_attractor::mcps::{
    DEFAULT_STARTUP_TIMEOUT_MS, DEFAULT_TOOL_TIMEOUT_MS, McpHttpProtocol, McpServer, McpTransport,
    McpValue,
};
use frontend_attractor::model::parse_duration;
use frontend_attractor::template::Context;
use ir::Value;

use crate::hooks::{PROJECT_FILE, SETTINGS_FILE, SETTINGS_HOOKS_VAR};
use crate::secrets::{InterpolationError, interpolate};

/// The `CompileInputs` variable a host sets to the text of its MCP catalog,
/// when it has one: a TOML table keyed by catalog id, each entry in the
/// inline `[run.agent.mcps.<name>]` shape (`type`, `command`, `url`, ...).
/// A `[run.agent.mcps.<name>] id = "<catalog id>"` entry in any layer then
/// resolves to that server under `<name>`. Without the variable, a
/// reference is `unsupported.workflow_toml.run.agent.mcps.reference`.
pub const MCP_CATALOG_VAR: &str = "fabro.mcp_catalog_toml";

/// The file name the catalog's own diagnostics carry, and the prefix of a
/// resolved server's `source` (`mcp-catalog:<id>`).
const CATALOG_SOURCE: &str = "mcp-catalog";

/// The host's MCP catalog, parsed from [`MCP_CATALOG_VAR`].
#[derive(Debug, Default)]
pub struct McpCatalog {
    entries: toml::Table,
}

impl McpCatalog {
    /// Parse the catalog text. A text that is not a TOML table, or an entry
    /// that is not a table, is `fabro.mcps.catalog`; the catalog then holds
    /// the entries that are tables, so a reference to a bad one is reported
    /// against the catalog, not as a missing standalone feature.
    pub fn parse(text: &str, diags: &mut Diagnostics) -> Self {
        let span = Span::file(CATALOG_SOURCE);
        let entries: toml::Table = match text.parse() {
            Ok(entries) => entries,
            Err(error) => {
                diags.error(
                    "fabro.mcps.catalog",
                    span,
                    format!("the host's MCP catalog is not valid TOML: {error}"),
                );
                return Self::default();
            }
        };
        let (tables, others): (toml::Table, toml::Table) =
            entries.into_iter().partition(|(_, entry)| entry.is_table());
        for id in others.keys() {
            diags.error(
                "fabro.mcps.catalog",
                span.clone(),
                format!("MCP catalog entry `{id}` must be a table of server fields"),
            );
        }
        Self { entries: tables }
    }

    fn get(&self, id: &str) -> Option<&toml::Table> {
        self.entries.get(id).and_then(toml::Value::as_table)
    }
}

/// Read and merge every layer. `workflow_toml` is the already-read
/// `(path, text)` of the workflow's own `workflow.toml`, when it exists.
/// References resolve against the catalog the host bound, when it did.
pub fn load(
    files: &dyn FileSource,
    inputs: &CompileInputs,
    workflow_toml: Option<&(String, String)>,
    template: &Context,
    diags: &mut Diagnostics,
) -> Vec<McpServer> {
    let catalog = match inputs.vars.get(MCP_CATALOG_VAR) {
        Some(Value::String(text)) => Some(McpCatalog::parse(text, diags)),
        _ => None,
    };
    let catalog = catalog.as_ref();
    let mut layers = Vec::with_capacity(3);
    if let Some(Value::String(text)) = inputs.vars.get(SETTINGS_HOOKS_VAR) {
        layers.push(read_layer(text, SETTINGS_FILE, template, catalog, diags));
    }
    if let Some(text) = files.read(PROJECT_FILE) {
        layers.push(read_layer(&text, PROJECT_FILE, template, catalog, diags));
    }
    if let Some((path, text)) = workflow_toml {
        layers.push(read_layer(text, path, template, catalog, diags));
    }
    merge(layers)
}

/// One layer's entries: the name and the server, or `None` for an entry the
/// layer disabled (which removes the name from lower layers too).
pub type LayerEntry = (String, Option<McpServer>);

/// Read one settings layer's `[run.agent.mcps]` table. `source` names the
/// file in messages. Every problem is a diagnostic on the file; a file that
/// names servers and cannot be read is an error, never a silent skip. A
/// reference entry resolves against `catalog`; without one it is refused.
pub fn read_layer(
    text: &str,
    source: &str,
    template: &Context,
    catalog: Option<&McpCatalog>,
    diags: &mut Diagnostics,
) -> Vec<LayerEntry> {
    let span = Span::file(source);
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(error) => {
            if text.contains("run.agent.mcps") {
                diags.error(
                    "fabro.mcps.toml",
                    span,
                    format!("`{source}` configures MCP servers but is not valid TOML: {error}"),
                );
            }
            return Vec::new();
        }
    };
    let Some(mcps) = table
        .get("run")
        .and_then(toml::Value::as_table)
        .and_then(|run| run.get("agent"))
        .and_then(toml::Value::as_table)
        .and_then(|agent| agent.get("mcps"))
    else {
        return Vec::new();
    };
    let Some(entries) = mcps.as_table() else {
        diags.error(
            "fabro.mcps.shape",
            span,
            format!("`run.agent.mcps` in `{source}` must be a table of servers keyed by name"),
        );
        return Vec::new();
    };
    let mut reader = EntryReader {
        source,
        span,
        template,
        catalog,
        diags,
    };
    let mut names: Vec<&String> = entries.keys().collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| match reader.entry(name, &entries[name]) {
            Read::Server(server) => Some((name.clone(), Some(server))),
            Read::Disabled => Some((name.clone(), None)),
            Read::Rejected => None,
        })
        .collect()
}

/// What one entry read as.
enum Read {
    Server(McpServer),
    /// `enabled = false`: the name is removed from every layer below.
    Disabled,
    /// Errors were reported, or the runner cannot serve the entry.
    Rejected,
}

/// Merge the layers, lowest first: a later entry replaces an earlier one with
/// the same name; a disabled entry removes the name. Sorted by name.
pub fn merge(layers: Vec<Vec<LayerEntry>>) -> Vec<McpServer> {
    let mut merged: BTreeMap<String, Option<McpServer>> = BTreeMap::new();
    for layer in layers {
        for (name, server) in layer {
            merged.insert(name, server);
        }
    }
    merged.into_values().flatten().collect()
}

struct EntryReader<'a> {
    source:   &'a str,
    span:     Span,
    template: &'a Context,
    /// The host's catalog, for reference entries; `None` inside the catalog
    /// itself, whose entries are inline by construction.
    catalog:  Option<&'a McpCatalog>,
    diags:    &'a mut Diagnostics,
}

/// What a `script` entry runs through, per transport: `sh` on the host for
/// stdio, the scope's Bash for sandbox, as Fabro resolves them.
#[derive(Clone, Copy)]
enum Interpreter {
    HostShell,
    SandboxBash,
}

impl EntryReader<'_> {
    fn error(&mut self, code: &str, message: String) {
        self.diags.error(code, self.span.clone(), message);
    }

    fn unsupported(&mut self, feature: &str, message: String, hint: &str) {
        self.diags
            .unsupported(feature, self.span.clone(), message, hint);
    }

    fn entry(&mut self, name: &str, value: &toml::Value) -> Read {
        match self.read_entry(name, value) {
            Some(Some(server)) => Read::Server(server),
            Some(None) => Read::Disabled,
            None => Read::Rejected,
        }
    }

    /// `Some(None)` is a disabled entry, `None` an entry with errors (already
    /// reported) or a reference the runner cannot serve.
    #[expect(
        clippy::option_option,
        reason = "the inner `None` is the disabled case and `?` propagates the rejected one; \
                  the public shape is `Read`"
    )]
    fn read_entry(&mut self, name: &str, value: &toml::Value) -> Option<Option<McpServer>> {
        let source = self.source;
        let Some(table) = value.as_table() else {
            self.error(
                "fabro.mcps.entry",
                format!("`run.agent.mcps.{name}` in `{source}` must be a table"),
            );
            return None;
        };
        let has_reference = table.contains_key("id");
        let has_inline = [
            "type",
            "script",
            "command",
            "url",
            "headers",
            "port",
            "env",
            "startup_timeout",
            "tool_timeout",
        ]
        .iter()
        .any(|key| table.contains_key(*key));
        if has_reference && has_inline {
            self.error(
                "fabro.mcps.entry",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` cannot mix catalog reference fields \
                     (`id`, `enabled`) with inline server fields"
                ),
            );
            return None;
        }
        let enabled = match table.get("enabled") {
            None => true,
            Some(toml::Value::Boolean(flag)) => *flag,
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.enabled` in `{source}` must be a boolean"),
                );
                return None;
            }
        };
        if has_reference {
            if let Some(key) = table
                .keys()
                .find(|key| !matches!(key.as_str(), "id" | "enabled"))
            {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` is not a field of a catalog \
                         reference (`id`, `enabled`)"
                    ),
                );
                return None;
            }
            if !enabled {
                return Some(None);
            }
            let id = table.get("id").and_then(toml::Value::as_str).unwrap_or("");
            return self.reference(name, id);
        }
        if !enabled {
            return Some(None);
        }
        let Some(kind) = table.get("type").and_then(toml::Value::as_str) else {
            self.error(
                "fabro.mcps.type",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` needs `type = \"stdio\"`, `\"http\"` \
                     or `\"sandbox\"`"
                ),
            );
            return None;
        };
        let allowed: &[&str] = match kind {
            "http" => &[
                "enabled",
                "type",
                "protocol",
                "url",
                "headers",
                "startup_timeout",
                "tool_timeout",
            ],
            "stdio" => &[
                "enabled",
                "type",
                "script",
                "command",
                "env",
                "startup_timeout",
                "tool_timeout",
            ],
            "sandbox" => &[
                "enabled",
                "type",
                "protocol",
                "script",
                "command",
                "port",
                "env",
                "startup_timeout",
                "tool_timeout",
            ],
            other => {
                self.error(
                    "fabro.mcps.type",
                    format!(
                        "`run.agent.mcps.{name}.type = \"{other}\"` in `{source}` is not a \
                         transport Fabro accepts (`stdio`, `http`, `sandbox`)"
                    ),
                );
                return None;
            }
        };
        let mut ok = true;
        for key in table.keys() {
            if !allowed.contains(&key.as_str()) {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` is not a field of a \
                         `{kind}` server"
                    ),
                );
                ok = false;
            }
        }
        if !ok {
            return None;
        }
        let protocol = self.protocol(name, table)?;
        let startup_timeout_ms =
            self.duration(name, table, "startup_timeout", DEFAULT_STARTUP_TIMEOUT_MS)?;
        let tool_timeout_ms =
            self.duration(name, table, "tool_timeout", DEFAULT_TOOL_TIMEOUT_MS)?;
        let transport = match kind {
            "http" => {
                let url = self.text(name, table, "url", true)?;
                let headers = self.values(name, table, "headers")?;
                McpTransport::Http {
                    protocol,
                    url,
                    headers,
                }
            }
            "stdio" => McpTransport::Stdio {
                command: self.command(name, table, Interpreter::HostShell)?,
                env:     self.values(name, table, "env")?,
            },
            _ => {
                let port = match table.get("port") {
                    Some(toml::Value::Integer(port)) if (1..=65_535).contains(port) => {
                        u16::try_from(*port).unwrap_or(u16::MAX)
                    }
                    _ => {
                        self.error(
                            "fabro.mcps.entry",
                            format!(
                                "`run.agent.mcps.{name}.port` in `{source}` must be an integer \
                                 from 1 to 65535"
                            ),
                        );
                        return None;
                    }
                };
                McpTransport::Sandbox {
                    protocol,
                    command: self.command(name, table, Interpreter::SandboxBash)?,
                    port,
                    env: self.values(name, table, "env")?,
                }
            }
        };
        Some(Some(McpServer {
            name: name.to_owned(),
            transport,
            startup_timeout_ms,
            tool_timeout_ms,
            source: source.to_owned(),
        }))
    }

    /// A reference to the host's catalog: the entry it names, read with the
    /// inline rules under the reference's `name`, its `source` naming the
    /// catalog entry. The standalone runner has no catalog and refuses the
    /// reference; a bound catalog without the id is `fabro.mcps.reference`.
    #[expect(
        clippy::option_option,
        reason = "the same shape as `read_entry`, whose reference branch this is"
    )]
    fn reference(&mut self, name: &str, id: &str) -> Option<Option<McpServer>> {
        let source = self.source;
        let Some(catalog) = self.catalog else {
            self.unsupported(
                "workflow_toml.run.agent.mcps.reference",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` references the server-managed MCP \
                     catalog entry `{id}`; the standalone runner has no Fabro server catalog"
                ),
                "write the server inline with `type = \"stdio\"`, `\"http\"` or `\"sandbox\"`",
            );
            return None;
        };
        let Some(entry) = catalog.get(id) else {
            self.error(
                "fabro.mcps.reference",
                format!(
                    "`run.agent.mcps.{name}` in `{source}` references the MCP catalog entry \
                     `{id}`, which the host's catalog does not have"
                ),
            );
            return None;
        };
        let catalog_source = format!("{CATALOG_SOURCE}:{id}");
        if entry.contains_key("id") {
            self.error(
                "fabro.mcps.catalog",
                format!(
                    "`{catalog_source}` is itself a reference; a catalog entry must be an inline \
                     server"
                ),
            );
            return None;
        }
        let mut reader = EntryReader {
            source:   &catalog_source,
            span:     Span::file(&catalog_source),
            template: self.template,
            catalog:  None,
            diags:    &mut *self.diags,
        };
        reader.read_entry(name, &toml::Value::Table(entry.clone()))
    }

    /// `protocol`: `streamable_http` (the default) or `sse`, Fabro's two
    /// values. A `stdio` entry never reaches here with one: the field is not
    /// in its allowed list.
    fn protocol(&mut self, name: &str, table: &toml::Table) -> Option<McpHttpProtocol> {
        let source = self.source;
        match table.get("protocol") {
            None => Some(McpHttpProtocol::default()),
            Some(toml::Value::String(text)) => match text.as_str() {
                "streamable_http" => Some(McpHttpProtocol::StreamableHttp),
                "sse" => Some(McpHttpProtocol::Sse),
                other => {
                    self.error(
                        "fabro.mcps.entry",
                        format!(
                            "`run.agent.mcps.{name}.protocol = \"{other}\"` in `{source}` must \
                             be `streamable_http` or `sse`"
                        ),
                    );
                    None
                }
            },
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.protocol` in `{source}` must be a string, \
                         `streamable_http` or `sse`"
                    ),
                );
                None
            }
        }
    }

    fn duration(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
        default: u64,
    ) -> Option<u64> {
        let source = self.source;
        match table.get(key) {
            None => Some(default),
            Some(toml::Value::String(text)) => {
                if let Some(duration) = parse_duration(text) {
                    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                } else {
                    self.error(
                        "fabro.mcps.entry",
                        format!(
                            "`run.agent.mcps.{name}.{key} = \"{text}\"` in `{source}` is not a \
                             duration (`10s`, `500ms`, `2m`)"
                        ),
                    );
                    None
                }
            }
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.{key}` in `{source}` must be a duration string \
                         (`10s`, `500ms`, `2m`)"
                    ),
                );
                None
            }
        }
    }

    /// A required or optional string field, interpolated. A secret token is
    /// refused: only an `env` or `headers` value may carry one.
    fn text(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
        required: bool,
    ) -> Option<String> {
        let source = self.source;
        match table.get(key) {
            None if required => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}` in `{source}` needs `{key}`"),
                );
                None
            }
            None => Some(String::new()),
            Some(toml::Value::String(text)) => self
                .interpolate(&format!("run.agent.mcps.{name}.{key}"), text)
                .map(|value| match value {
                    McpValue::Literal(text) => text,
                    McpValue::Secret { .. } => String::new(),
                }),
            Some(_) => {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.{key}` in `{source}` must be a string"),
                );
                None
            }
        }
    }

    /// Exactly one of `script` and `command`, as the argv the transport runs.
    fn command(
        &mut self,
        name: &str,
        table: &toml::Table,
        interpreter: Interpreter,
    ) -> Option<Vec<String>> {
        let source = self.source;
        match (table.get("script"), table.get("command")) {
            (Some(_), Some(_)) | (None, None) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}` in `{source}` needs exactly one of `script` and \
                         `command`"
                    ),
                );
                None
            }
            (Some(_), None) => {
                let script = self.text(name, table, "script", true)?;
                let shell = match interpreter {
                    Interpreter::HostShell => "sh",
                    Interpreter::SandboxBash => "bash",
                };
                Some(vec![shell.to_owned(), "-c".to_owned(), script])
            }
            (None, Some(toml::Value::Array(items))) => {
                let mut command = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    let Some(text) = item.as_str() else {
                        self.error(
                            "fabro.mcps.entry",
                            format!(
                                "`run.agent.mcps.{name}.command[{index}]` in `{source}` must be \
                                 a string"
                            ),
                        );
                        return None;
                    };
                    match self
                        .interpolate(&format!("run.agent.mcps.{name}.command[{index}]"), text)?
                    {
                        McpValue::Literal(text) => command.push(text),
                        McpValue::Secret { .. } => return None,
                    }
                }
                if command.is_empty() {
                    self.error(
                        "fabro.mcps.entry",
                        format!("`run.agent.mcps.{name}.command` in `{source}` must not be empty"),
                    );
                    return None;
                }
                Some(command)
            }
            (None, Some(_)) => {
                self.error(
                    "fabro.mcps.entry",
                    format!(
                        "`run.agent.mcps.{name}.command` in `{source}` must be an array of \
                         strings"
                    ),
                );
                None
            }
        }
    }

    /// An `env` or `headers` table: each value literal text or exactly one
    /// `{{ secrets.NAME }}` token.
    fn values(
        &mut self,
        name: &str,
        table: &toml::Table,
        key: &str,
    ) -> Option<BTreeMap<String, McpValue>> {
        let source = self.source;
        let mut out = BTreeMap::new();
        let Some(values) = table.get(key) else {
            return Some(out);
        };
        let Some(values) = values.as_table() else {
            self.error(
                "fabro.mcps.entry",
                format!("`run.agent.mcps.{name}.{key}` in `{source}` must be a table of strings"),
            );
            return None;
        };
        for (field, value) in values {
            let Some(text) = value.as_str() else {
                self.error(
                    "fabro.mcps.entry",
                    format!("`run.agent.mcps.{name}.{key}.{field}` in `{source}` must be a string"),
                );
                return None;
            };
            let what = format!("run.agent.mcps.{name}.{key}.{field}");
            out.insert(field.clone(), self.interpolate_value(&what, text)?);
        }
        Some(out)
    }

    /// Interpolate a transport string where a secret may not appear.
    fn interpolate(&mut self, what: &str, text: &str) -> Option<McpValue> {
        match interpolate(text, self.template, false) {
            Ok(value) => Some(McpValue::Literal(value.text)),
            Err(problem) => {
                self.report(what, problem);
                None
            }
        }
    }

    /// Interpolate a value that may be exactly one secret token.
    fn interpolate_value(&mut self, what: &str, text: &str) -> Option<McpValue> {
        match interpolate(text, self.template, true) {
            Ok(value) => Some(match value.secret {
                Some(name) => McpValue::Secret { name },
                None => McpValue::Literal(value.text),
            }),
            Err(problem) => {
                self.report(what, problem);
                None
            }
        }
    }

    fn report(&mut self, what: &str, problem: InterpolationError) {
        let source = self.source;
        match problem {
            InterpolationError::SecretNotAllowed { name } => self.unsupported(
                "workflow_toml.run.agent.mcps.secret",
                format!(
                    "`{what}` in `{source}` reads `{{{{ secrets.{name} }}}}`; the standalone \
                     runner resolves a secret only as a whole `env` or `headers` value"
                ),
                "write `KEY = \"{{ secrets.NAME }}\"` under `env` or `headers` on its own",
            ),
            InterpolationError::Env { name } => self.error(
                "fabro.mcps.env_token",
                format!(
                    "`{what}` in `{source}` reads `{{{{ env.{name} }}}}`, which Fabro refuses \
                     before launching an MCP server"
                ),
            ),
            InterpolationError::Unbound { name } => self.error(
                "fabro.mcps.unbound",
                format!("`{what}` in `{source}` reads `{{{{ {name} }}}}`, which no input binds"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use frontend::CompileInputs;

    use super::*;

    fn context() -> Context {
        Context::new(&CompileInputs::new().with_input("root", "/srv"))
    }

    fn layer(text: &str) -> (Vec<LayerEntry>, Vec<String>) {
        layer_with(text, None)
    }

    fn layer_with(text: &str, catalog: Option<&McpCatalog>) -> (Vec<LayerEntry>, Vec<String>) {
        let mut diags = Diagnostics::new();
        let entries = read_layer(text, "workflow.toml", &context(), catalog, &mut diags);
        let codes = diags.iter().map(|d| d.code.to_string()).collect();
        (entries, codes)
    }

    /// A reference resolves against the host's catalog: the entry is read
    /// with the inline rules, under the reference's name, its source naming
    /// the catalog entry; a disabled reference removes the name; an id the
    /// catalog lacks, or a catalog entry that is not an inline server, is an
    /// error against the catalog, never the standalone refusal.
    #[test]
    fn references_resolve_against_the_hosts_catalog() {
        let mut diags = Diagnostics::new();
        let catalog = McpCatalog::parse(
            "[files-prod]\ntype = \"stdio\"\ncommand = [\"srv\", \"{{ inputs.root }}\"]\n\
             env = { TOKEN = \"{{ secrets.FILES }}\" }\ntool_timeout = \"2m\"\n\
             [remote]\ntype = \"http\"\nurl = \"https://mcp.example\"\n\
             [loop]\nid = \"remote\"\n[broken]\ntype = \"stdio\"\n",
            &mut diags,
        );
        assert!(diags.iter().next().is_none(), "the catalog parses");
        let (entries, codes) = layer_with(
            "[run.agent.mcps.notes]\nid = \"files-prod\"\n[run.agent.mcps.off]\nid = \"remote\"\n\
             enabled = false\n",
            Some(&catalog),
        );
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(entries.len(), 2);
        let (name, server) = &entries[0];
        assert_eq!(name, "notes");
        let server = server.as_ref().expect("resolved");
        assert_eq!(server.name, "notes", "the reference's name, not the id");
        assert_eq!(server.source, "mcp-catalog:files-prod");
        assert_eq!(server.tool_timeout_ms, 120_000);
        assert_eq!(server.transport, McpTransport::Stdio {
            command: vec!["srv".into(), "/srv".into()],
            env:     BTreeMap::from([("TOKEN".to_owned(), McpValue::Secret {
                name: "FILES".into(),
            })]),
        });
        assert_eq!(entries[1], ("off".to_owned(), None));
        for (text, code) in [
            (
                "[run.agent.mcps.a]\nid = \"nope\"\n",
                "fabro.mcps.reference",
            ),
            ("[run.agent.mcps.a]\nid = \"loop\"\n", "fabro.mcps.catalog"),
            ("[run.agent.mcps.a]\nid = \"broken\"\n", "fabro.mcps.entry"),
        ] {
            let (entries, codes) = layer_with(text, Some(&catalog));
            assert_eq!(codes, [code], "{text}");
            assert!(entries.is_empty(), "{text}");
        }
        // The catalog's own shape problems are reported once, at parse.
        let mut diags = Diagnostics::new();
        let catalog =
            McpCatalog::parse("bad = 3\n[ok]\ntype = \"http\"\nurl = \"u\"\n", &mut diags);
        let codes: Vec<String> = diags.iter().map(|d| d.code.to_string()).collect();
        assert_eq!(codes, ["fabro.mcps.catalog"]);
        assert!(catalog.get("ok").is_some() && catalog.get("bad").is_none());
        let mut diags = Diagnostics::new();
        McpCatalog::parse("[ok\n", &mut diags);
        let codes: Vec<String> = diags.iter().map(|d| d.code.to_string()).collect();
        assert_eq!(codes, ["fabro.mcps.catalog"]);
    }

    #[test]
    fn every_transport_reads_with_fabros_fields_and_defaults() {
        let (entries, codes) = layer(
            r#"
[run.agent.mcps.files]
type = "stdio"
command = ["npx", "server", "{{ inputs.root }}"]
env = { TOKEN = "{{ secrets.FILES_TOKEN }}", MODE = "prod" }
startup_timeout = "15s"
tool_timeout = "2m"

[run.agent.mcps.shell]
type = "stdio"
script = "exec ./server --root {{ inputs.root }}"

[run.agent.mcps.remote]
type = "http"
url = "https://mcp.example/{{ inputs.root }}"
headers = { Authorization = "{{ secrets.REMOTE }}" }

[run.agent.mcps.browser]
type = "sandbox"
command = ["npx", "playwright-mcp", "--port", "3100"]
port = 3100
"#,
        );
        assert!(codes.is_empty(), "{codes:?}");
        let servers = merge(vec![entries]);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["browser", "files", "remote", "shell"]);
        let files = &servers[1];
        assert_eq!(files.startup_timeout_ms, 15_000);
        assert_eq!(files.tool_timeout_ms, 120_000);
        assert_eq!(files.source, "workflow.toml");
        assert_eq!(files.transport, McpTransport::Stdio {
            command: vec!["npx".into(), "server".into(), "/srv".into()],
            env:     BTreeMap::from([
                ("MODE".to_owned(), McpValue::Literal("prod".into())),
                ("TOKEN".to_owned(), McpValue::Secret {
                    name: "FILES_TOKEN".into(),
                }),
            ]),
        });
        assert_eq!(servers[3].transport, McpTransport::Stdio {
            command: vec!["sh".into(), "-c".into(), "exec ./server --root /srv".into()],
            env:     BTreeMap::new(),
        });
        assert_eq!(servers[2].transport, McpTransport::Http {
            protocol: McpHttpProtocol::StreamableHttp,
            url:      "https://mcp.example//srv".into(),
            headers:  BTreeMap::from([("Authorization".to_owned(), McpValue::Secret {
                name: "REMOTE".into(),
            })]),
        });
        assert_eq!(servers[2].startup_timeout_ms, DEFAULT_STARTUP_TIMEOUT_MS);
        assert_eq!(servers[2].tool_timeout_ms, DEFAULT_TOOL_TIMEOUT_MS);
        assert!(matches!(
            &servers[0].transport,
            McpTransport::Sandbox { command, port: 3100, .. }
                if command[0] == "npx"
        ));
        // The step config round-trips through JSON.
        let json = serde_json::to_value(&servers).expect("json");
        assert_eq!(
            json[1]["transport"]["env"]["TOKEN"]["$secret"],
            "FILES_TOKEN"
        );
        let back: Vec<McpServer> = serde_json::from_value(json).expect("back");
        assert_eq!(back, servers);
    }

    /// `protocol = "sse"` is accepted on `http` and `sandbox` servers, as
    /// Fabro accepts it; `streamable_http` is the default and may be named.
    /// A transport persisted before the field existed reads as the default.
    #[test]
    fn sse_and_streamable_http_are_fabros_two_protocols() {
        let (entries, codes) = layer(
            r#"
[run.agent.mcps.legacy]
type = "http"
url = "http://127.0.0.1:1/sse"
protocol = "sse"

[run.agent.mcps.current]
type = "http"
url = "http://127.0.0.1:1/mcp"
protocol = "streamable_http"

[run.agent.mcps.browser]
type = "sandbox"
command = ["npx", "@playwright/mcp", "--port", "3100"]
port = 3100
protocol = "sse"

[run.agent.mcps.plain]
type = "sandbox"
script = "exec ./server"
port = 3200
"#,
        );
        assert!(codes.is_empty(), "{codes:?}");
        let servers = merge(vec![entries]);
        let protocols: Vec<(&str, McpHttpProtocol)> = servers
            .iter()
            .map(|server| {
                (server.name.as_str(), match &server.transport {
                    McpTransport::Http { protocol, .. }
                    | McpTransport::Sandbox { protocol, .. } => *protocol,
                    McpTransport::Stdio { .. } => unreachable!("no stdio entry"),
                })
            })
            .collect();
        assert_eq!(protocols, [
            ("browser", McpHttpProtocol::Sse),
            ("current", McpHttpProtocol::StreamableHttp),
            ("legacy", McpHttpProtocol::Sse),
            ("plain", McpHttpProtocol::StreamableHttp),
        ]);
        assert_eq!(McpHttpProtocol::Sse.kind(), "sse");
        assert_eq!(McpHttpProtocol::default().kind(), "streamable_http");
        // The step config carries the protocol by Fabro's name...
        let json = serde_json::to_value(&servers).expect("json");
        assert_eq!(json[0]["transport"]["protocol"], "sse");
        assert_eq!(json[3]["transport"]["protocol"], "streamable_http");
        // ...and a graph written before the field existed reads as the
        // default, so an existing run directory still resumes.
        let older: McpTransport =
            serde_json::from_value(serde_json::json!({ "type": "http", "url": "u" }))
                .expect("an older transport");
        assert_eq!(older, McpTransport::Http {
            protocol: McpHttpProtocol::StreamableHttp,
            url:      "u".into(),
            headers:  BTreeMap::new(),
        });
    }

    #[test]
    fn fabros_shape_rules_are_errors_and_disabled_entries_vanish() {
        let cases = [
            ("[run.agent.mcps.a]\ncommand = [\"x\"]\n", "fabro.mcps.type"),
            (
                "[run.agent.mcps.a]\ntype = \"grpc\"\ncommand = [\"x\"]\n",
                "fabro.mcps.type",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nurl = \"u\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\nscript = \"s\"\ncommand = [\"x\"]\n",
                "fabro.mcps.entry",
            ),
            ("[run.agent.mcps.a]\ntype = \"stdio\"\n", "fabro.mcps.entry"),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = []\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = \"x\"\n",
                "fabro.mcps.entry",
            ),
            ("[run.agent.mcps.a]\ntype = \"http\"\n", "fabro.mcps.entry"),
            (
                "[run.agent.mcps.a]\ntype = \"sandbox\"\ncommand = [\"x\"]\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"sandbox\"\ncommand = [\"x\"]\nport = 70000\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\ntool_timeout = 5\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\ntool_timeout = \"soon\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\ntype = \"stdio\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\nname = \"x\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\nid = \"cat\"\n",
                "unsupported.workflow_toml.run.agent.mcps.reference",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"http\"\nurl = \"u\"\nprotocol = \"ws\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"http\"\nurl = \"u\"\nprotocol = 3\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nprotocol = \"sse\"\n",
                "fabro.mcps.entry",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\", \"{{ secrets.T }}\"]\n",
                "unsupported.workflow_toml.run.agent.mcps.secret",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nenv = { A = \"{{ env.HOME }}\" }\n",
                "fabro.mcps.env_token",
            ),
            (
                "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"{{ inputs.nope }}\"]\n",
                "fabro.mcps.unbound",
            ),
            ("[run.agent]\nmcps = 3\n", "fabro.mcps.shape"),
            ("[run.agent.mcps]\na = 3\n", "fabro.mcps.entry"),
            ("[run.agent.mcps.a\ntype = \"stdio\"\n", "fabro.mcps.toml"),
        ];
        for (text, code) in cases {
            let (entries, codes) = layer(text);
            assert_eq!(codes, [code], "{text}");
            assert!(entries.is_empty(), "{text}");
        }
        let (entries, codes) = layer(
            "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"x\"]\nenabled = false\n[run.agent.mcps.b]\nid = \"cat\"\nenabled = false\n",
        );
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(entries, vec![
            ("a".to_owned(), None),
            ("b".to_owned(), None)
        ]);
        assert!(layer("[run]\ngoal = \"x\"\n").0.is_empty());
        assert!(layer("[run.agent.mcps]\n").0.is_empty());
    }

    #[test]
    fn later_layers_replace_by_name_and_can_disable() {
        let (settings, _) = layer(
            "[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"low\"]\n[run.agent.mcps.b]\ntype = \"stdio\"\ncommand = [\"b\"]\n",
        );
        let (project, _) = layer("[run.agent.mcps.a]\ntype = \"stdio\"\ncommand = [\"high\"]\n");
        let (workflow, _) = layer(
            "[run.agent.mcps.b]\ntype = \"stdio\"\ncommand = [\"b\"]\nenabled = false\n[run.agent.mcps.c]\ntype = \"http\"\nurl = \"u\"\n",
        );
        let merged = merge(vec![settings, project, workflow]);
        let summary: Vec<(String, String)> = merged
            .iter()
            .map(|server| {
                (server.name.clone(), match &server.transport {
                    McpTransport::Stdio { command, .. } => command[0].clone(),
                    McpTransport::Http { url, .. } => url.clone(),
                    McpTransport::Sandbox { .. } => "sandbox".into(),
                })
            })
            .collect();
        assert_eq!(summary, [
            ("a".to_owned(), "high".to_owned()),
            ("c".to_owned(), "u".to_owned()),
        ]);
    }
}
