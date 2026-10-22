//! Fabro's local hook configuration: the `[[run.hooks]]` entries of the
//! settings layers, merged the way Fabro merges them, into the resolved list
//! the run carries ([`frontend_attractor::hooks`]).
//!
//! The layers, lowest first: `~/.fabro/settings.toml`, `.fabro/project.toml`,
//! `workflow.toml`. A higher layer's entry replaces a lower one with the same
//! `id` in place; entries without an `id` append. Every entry names one
//! transport: `script` or `command` (a command hook), `url` (HTTP), `prompt`
//! (a one-turn model call), or `agent = "enabled"` with a `prompt` (an
//! agent). Fabro's own parser is `fabro-config/src/layers/run.rs`
//! (`HookEntry`) and `resolve/run.rs` (`resolve_hook`) at the pinned
//! revision; this module keeps its shape and its one validation rule.

use std::collections::BTreeMap;

use frontend::{CompileInputs, Diagnostics, FileSource, Span};
pub use frontend_attractor::hooks::{
    DEFAULT_MAX_TOOL_ROUNDS, DEFAULT_MODEL, DEFAULT_TIMEOUT_MS, HookDefinition, HookEvent,
    HookKind, PROMPT_TIMEOUT_MS, TlsMode,
};
use ir::Value;
use serde::Deserialize;

/// The `CompileInputs` variable a host sets to the text of its
/// `~/.fabro/settings.toml`, when the run has one. The file is outside the
/// repository, so lowering never reads it by itself.
pub const SETTINGS_HOOKS_VAR: &str = "fabro.settings_toml";

/// The project layer's repository-relative path.
pub const PROJECT_FILE: &str = ".fabro/project.toml";

/// The name diagnostics give the host's settings layer, whose text arrives
/// as [`SETTINGS_HOOKS_VAR`] rather than from a path lowering can read.
pub const SETTINGS_FILE: &str = "settings.toml";

/// Read and merge every hook layer. `workflow_toml` is the already-read
/// `(path, text)` of the workflow's own `workflow.toml`, when it exists.
pub fn load(
    files: &dyn FileSource,
    inputs: &CompileInputs,
    workflow_toml: Option<&(String, String)>,
    diags: &mut Diagnostics,
) -> Vec<HookDefinition> {
    let mut layers = Vec::with_capacity(3);
    if let Some(Value::String(text)) = inputs.vars.get(SETTINGS_HOOKS_VAR) {
        layers.push(read_layer(text, SETTINGS_FILE, diags));
    }
    if let Some(text) = files.read(PROJECT_FILE) {
        layers.push(read_layer(&text, PROJECT_FILE, diags));
    }
    if let Some((path, text)) = workflow_toml {
        layers.push(read_layer(text, path, diags));
    }
    let merged = merge(layers);
    if merged
        .iter()
        .any(|hook| hook.event == HookEvent::CheckpointSaved)
    {
        // The per-entry warning names the file; this one states the policy
        // once for the run.
        diags.warning(
            "fabro.hooks.checkpoint_saved",
            Span::file("hooks"),
            "a `checkpoint_saved` hook is configured; Petri does not write checkpoints, so it \
             is recorded as unsupported and never runs",
        );
    }
    merged
}

/// The TOML shape of one `[[run.hooks]]` entry: Fabro's `HookEntry`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id:              Option<String>,
    name:            Option<String>,
    event:           Option<String>,
    matcher:         Option<String>,
    blocking:        Option<bool>,
    timeout:         Option<String>,
    sandbox:         Option<bool>,
    script:          Option<String>,
    command:         Option<Vec<String>>,
    url:             Option<String>,
    #[serde(default)]
    headers:         BTreeMap<String, String>,
    tls:             Option<TlsMode>,
    prompt:          Option<String>,
    model:           Option<String>,
    max_tool_rounds: Option<u32>,
    agent:           Option<String>,
}

/// Read the `[[run.hooks]]` entries of one settings-layer file. `source`
/// names the file in diagnostics and in the resolved definitions. Returns
/// the entries that resolved; a bad entry is an error and is left out.
pub fn read_layer(text: &str, source: &str, diags: &mut Diagnostics) -> Vec<HookDefinition> {
    let span = Span::file(source);
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(error) => {
            // A file that names hooks and cannot be read would skip them
            // silently. The workflow reader fails admission for its own file
            // whatever it configures; a lower layer that names no hooks is
            // left to the readers of the sections it does name.
            if text.contains("run.hooks") {
                diags.error(
                    "fabro.hooks.toml",
                    span,
                    format!("`{source}` configures hooks but is not valid TOML: {error}"),
                );
            }
            return Vec::new();
        }
    };
    let Some(hooks) = table
        .get("run")
        .and_then(toml::Value::as_table)
        .and_then(|run| run.get("hooks"))
    else {
        return Vec::new();
    };
    let Some(entries) = hooks.as_array() else {
        diags.error(
            "fabro.hooks.shape",
            span,
            format!("`run.hooks` in `{source}` must be an array of tables (`[[run.hooks]]`)"),
        );
        return Vec::new();
    };
    let mut out = Vec::with_capacity(entries.len());
    for (index, value) in entries.iter().enumerate() {
        let path = format!("run.hooks[{index}]");
        let entry: Entry = match value.clone().try_into() {
            Ok(entry) => entry,
            Err(error) => {
                diags.error(
                    "fabro.hooks.entry",
                    span.clone(),
                    format!("`{path}` in `{source}`: {error}"),
                );
                continue;
            }
        };
        if let Some(definition) = resolve(&entry, &path, source, &span, diags) {
            out.push(definition);
        }
    }
    out
}

/// Fabro's `resolve_hook`: exactly one transport, the event parsed, the
/// timeout as milliseconds, the name from `name` then `id`.
fn resolve(
    entry: &Entry,
    path: &str,
    source: &str,
    span: &Span,
    diags: &mut Diagnostics,
) -> Option<HookDefinition> {
    let mut ok = true;
    let event = match entry.event.as_deref().map(HookEvent::parse) {
        Some(Some(event)) => Some(event),
        Some(None) => {
            diags.error(
                "fabro.hooks.event",
                span.clone(),
                format!(
                    "`{path}` in `{source}`: `{}` is not a hook event; the events are {}",
                    entry.event.as_deref().unwrap_or_default(),
                    HookEvent::ALL
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            None
        }
        None => {
            diags.error(
                "fabro.hooks.event",
                span.clone(),
                format!("`{path}` in `{source}` names no `event`"),
            );
            None
        }
    };
    let agent = match entry.agent.as_deref() {
        None => false,
        Some("enabled") => true,
        Some(other) => {
            diags.error(
                "fabro.hooks.entry",
                span.clone(),
                format!("`{path}` in `{source}`: `agent` must be `\"enabled\"`, not `{other}`"),
            );
            ok = false;
            false
        }
    };
    let transports = usize::from(entry.script.is_some() || entry.command.is_some())
        + usize::from(entry.url.is_some())
        + usize::from(entry.prompt.is_some() && !agent)
        + usize::from(agent);
    if transports != 1 {
        diags.error(
            "fabro.hooks.transport",
            span.clone(),
            format!(
                "`{path}` in `{source}`: exactly one hook transport must be configured (`script` or \
                 `command`, `url`, `prompt`, or `agent = \"enabled\"` with a `prompt`)"
            ),
        );
        ok = false;
    }
    let timeout_ms = match entry.timeout.as_deref().map(parse_duration_ms) {
        Some(Some(ms)) => Some(ms),
        Some(None) => {
            diags.error(
                "fabro.hooks.timeout",
                span.clone(),
                format!(
                    "`{path}` in `{source}`: `timeout` must be a duration with one unit, such as \
                     `30s` or `500ms`, not `{}`",
                    entry.timeout.as_deref().unwrap_or_default()
                ),
            );
            ok = false;
            None
        }
        None => None,
    };
    if let Some(matcher) = &entry.matcher
        && let Err(error) = regex::Regex::new(matcher)
    {
        diags.error(
            "fabro.hooks.matcher",
            span.clone(),
            format!("`{path}` in `{source}`: `matcher` is not a valid regex: {error}"),
        );
        ok = false;
    }
    let kind = if let Some(script) = &entry.script {
        HookKind::Command {
            command: script.clone(),
        }
    } else if let Some(command) = &entry.command {
        HookKind::Command {
            command: command.join(" "),
        }
    } else if let Some(url) = &entry.url {
        HookKind::Http {
            url:     url.clone(),
            headers: entry.headers.clone(),
            tls:     entry.tls.unwrap_or_default(),
        }
    } else if agent {
        HookKind::Agent {
            prompt:          entry.prompt.clone().unwrap_or_default(),
            model:           entry.model.clone(),
            max_tool_rounds: entry.max_tool_rounds,
        }
    } else if let Some(prompt) = &entry.prompt {
        HookKind::Prompt {
            prompt: prompt.clone(),
            model:  entry.model.clone(),
        }
    } else {
        return None;
    };
    let event = event?;
    if !ok {
        return None;
    }
    if event == HookEvent::CheckpointSaved {
        diags.warning(
            "fabro.hooks.checkpoint_saved",
            span.clone(),
            format!(
                "`{path}` in `{source}` listens for `checkpoint_saved`, a Fabro platform event; \
                 Petri does not write checkpoints, so this hook never runs"
            ),
        );
    }
    let name = entry
        .name
        .clone()
        .or_else(|| entry.id.clone())
        .unwrap_or_else(|| generated_name(event, &kind));
    Some(HookDefinition {
        name,
        id: entry.id.clone(),
        event,
        kind,
        matcher: entry.matcher.clone(),
        blocking: entry.blocking,
        timeout_ms,
        sandbox: entry.sandbox,
        source: Some(source.to_owned()),
    })
}

/// Fabro's `effective_name`: the event, then the first twenty characters of
/// the command or prompt, or the URL.
fn generated_name(event: HookEvent, kind: &HookKind) -> String {
    match kind {
        HookKind::Command { command } => {
            format!("{event}:{}", command.chars().take(20).collect::<String>())
        }
        HookKind::Http { url, .. } => format!("{event}:{url}"),
        HookKind::Prompt { prompt, .. } | HookKind::Agent { prompt, .. } => {
            format!("{event}:{}", prompt.chars().take(20).collect::<String>())
        }
    }
}

/// Fabro's settings duration: an integer with one unit, `ms`, `s`, `m`, `h`
/// or `d`. Composed values such as `1h30m` are refused.
pub fn parse_duration_ms(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let factor = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    number.checked_mul(factor)
}

/// Fabro's `combine_hooks`, applied lowest layer first: a higher layer's
/// entry replaces a lower one with the same `id` in place; the rest append.
pub fn merge(layers: Vec<Vec<HookDefinition>>) -> Vec<HookDefinition> {
    let mut merged: Vec<HookDefinition> = Vec::new();
    for layer in layers {
        let mut current = layer;
        for existing in &mut merged {
            if let Some(id) = &existing.id
                && let Some(index) = current
                    .iter()
                    .position(|entry| entry.id.as_deref() == Some(id))
            {
                *existing = current.remove(index);
            }
        }
        merged.extend(current);
    }
    merged
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn layer(text: &str) -> (Vec<HookDefinition>, Vec<String>) {
        let mut diags = Diagnostics::new();
        let hooks = read_layer(text, "workflow.toml", &mut diags);
        let codes = diags.iter().map(|d| d.code.to_string()).collect();
        (hooks, codes)
    }

    #[test]
    fn shorthand_command_and_every_transport_resolve() {
        let (hooks, codes) = layer(
            r#"
[[run.hooks]]
event = "stage_start"
script = "./check.sh"
matcher = "^agent$"
timeout = "30s"
sandbox = false

[[run.hooks]]
name = "notify"
event = "run_complete"
url = "https://hooks.example/done"
tls = "no_verify"
[run.hooks.headers]
X-Env = "prod"

[[run.hooks]]
event = "stage_start"
command = ["cargo", "fmt"]

[[run.hooks]]
event = "pre_tool_use"
prompt = "Is this safe?"
model = "haiku"

[[run.hooks]]
id = "verify"
event = "run_complete"
agent = "enabled"
prompt = "Run the tests."
max_tool_rounds = 3
"#,
        );
        assert_eq!(codes, Vec::<String>::new());
        assert_eq!(hooks.len(), 5);
        assert_eq!(hooks[0].kind, HookKind::Command {
            command: "./check.sh".into(),
        });
        assert_eq!(hooks[0].timeout_ms, Some(30_000));
        assert!(!hooks[0].runs_in_sandbox());
        assert_eq!(hooks[0].name, "stage_start:./check.sh");
        assert!(hooks[0].is_blocking());
        assert_eq!(hooks[1].name, "notify");
        assert!(!hooks[1].is_blocking());
        assert!(
            matches!(&hooks[1].kind, HookKind::Http { tls: TlsMode::NoVerify, headers, .. } if headers["X-Env"] == "prod")
        );
        assert_eq!(hooks[2].kind, HookKind::Command {
            command: "cargo fmt".into(),
        });
        assert_eq!(hooks[3].timeout(), Duration::from_secs(30));
        assert_eq!(hooks[4].timeout(), Duration::from_secs(60));
        assert_eq!(hooks[4].name, "verify");
        assert!(matches!(&hooks[4].kind, HookKind::Agent {
            max_tool_rounds: Some(3),
            ..
        }));
    }

    #[test]
    fn bad_entries_are_specific_errors() {
        let (hooks, codes) = layer(
            r#"
[[run.hooks]]
event = "stage_start"
script = "a"
url = "https://x"

[[run.hooks]]
event = "no_such_event"
script = "a"

[[run.hooks]]
event = "stage_start"
script = "a"
timeout = "1h30m"

[[run.hooks]]
event = "stage_start"
script = "a"
matcher = "("

[[run.hooks]]
event = "stage_start"
type = "command"
command = ["a"]

[[run.hooks]]
event = "checkpoint_saved"
script = "a"
"#,
        );
        assert_eq!(hooks.len(), 1, "{hooks:?}");
        assert_eq!(hooks[0].event, HookEvent::CheckpointSaved);
        for code in [
            "fabro.hooks.transport",
            "fabro.hooks.event",
            "fabro.hooks.timeout",
            "fabro.hooks.matcher",
            "fabro.hooks.entry",
            "fabro.hooks.checkpoint_saved",
        ] {
            assert!(codes.contains(&code.to_string()), "{code} in {codes:?}");
        }
    }

    #[test]
    fn layers_replace_by_id_in_place_and_append_the_rest() {
        let hook = |id: Option<&str>, command: &str| HookDefinition {
            name:       id.unwrap_or(command).into(),
            id:         id.map(Into::into),
            event:      HookEvent::StageStart,
            kind:       HookKind::Command {
                command: command.into(),
            },
            matcher:    None,
            blocking:   None,
            timeout_ms: None,
            sandbox:    None,
            source:     None,
        };
        let merged = merge(vec![
            vec![hook(Some("a"), "user-a"), hook(None, "user-anon")],
            vec![hook(None, "project-anon"), hook(Some("a"), "project-a")],
            vec![hook(Some("b"), "workflow-b")],
        ]);
        let commands: Vec<&str> = merged
            .iter()
            .map(|h| match &h.kind {
                HookKind::Command { command } => command.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(commands, [
            "project-a",
            "user-anon",
            "project-anon",
            "workflow-b"
        ]);
    }

    #[test]
    fn durations_take_one_unit() {
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("2m"), Some(120_000));
        assert_eq!(parse_duration_ms("1h30m"), None);
        assert_eq!(parse_duration_ms("30"), None);
    }
}
