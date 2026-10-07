//! Fork-owned stage-time context tokens (fabro-e71b): a second, narrow
//! render pass over a stage's assembled text.
//!
//! At lowering, `{{ inputs.* }}`, `{{ vars.* }}` and `{{ goal }}` render
//! once and the graph the engine sees is literal (frontend `template.rs`);
//! a `{{ context.NAME }}` token is masked there behind PUA sentinels and
//! survives verbatim to stage dispatch. This module resolves it, at
//! dispatch, against the node's visible context projection — exactly the
//! keys the `## Context` section would show: public
//! (`fidelity::is_hidden_key`), non-blank, and allowed by the node's
//! [`PreamblePolicy`].
//!
//! Resolution is STRICT: an unresolved token fails the stage (class
//! [`CLASS`]) naming the token and the visible keys — a typo must not
//! silently render empty. The pass is single: substituted values are never
//! re-scanned, so a context value that itself contains `{{ ... }}` lands
//! as data, not as a template.
//!
//! Commands deviate by design: a script resolves against the run context
//! WITHOUT the rendered-dedup the `## Context` rows apply — a key some
//! stage's output already rendered is still readable, because a command
//! consumes data, not prose. Values render exactly as `## Context` renders
//! them (`fidelity::render_value`, bounded previews for oversized values).
//!
//! Injection discipline: context values are agent-written strings. Place
//! tokens in data slots (command arguments, data lines), not inside
//! instruction sentences.
//!
//! This file is fork-owned (new file, `fork_` prefix): an upstream merge
//! cannot silently absorb it. The upstream touch points are the
//! `Result`-typed `assemble` seams in `prompt.rs`/`agent.rs`, the script
//! pass in `command.rs`, the accessors on `fidelity::Preamble`, and the
//! module declaration — all pinned by `tests/fork_context_tokens.rs`.

use std::error::Error;
use std::fmt;

use serde_json::Value;

use crate::fidelity;
use crate::fork_preamble_policy::PreamblePolicy;

/// The failure class of an unresolved context token.
pub const CLASS: &str = "context_token";

/// An unresolved `{{ context.NAME }}` token: the token as written, and the
/// node's visible keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unresolved {
    pub token:   String,
    pub visible: Vec<String>,
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not visible at this node; visible context keys: {}",
            self.token,
            if self.visible.is_empty() {
                "(none)".to_owned()
            } else {
                self.visible.join(", ")
            }
        )
    }
}

impl Error for Unresolved {}

/// Resolve every `{{ context.NAME }}` in `text` against `pairs`.
///
/// The grammar is exactly `{{ context.NAME }}`: `{{`, optional whitespace,
/// `context.`, a key of `[A-Za-z0-9_][A-Za-z0-9_.-]*`, optional whitespace,
/// `}}`. Any other spelling stays literal (for prompts, lowering's strict
/// MiniJinja already refused an unmasked `{{ context... }}` expression).
pub fn resolve(text: &str, pairs: &[(String, Value)]) -> Result<String, Unresolved> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(rest);
            return Ok(out);
        };
        let token = after[..end].trim();
        if let Some(name) = token.strip_prefix("context.").filter(|name| is_key(name)) {
            if let Some((_, value)) = pairs.iter().find(|(key, _)| key == name) {
                out.push_str(&rest[..start]);
                out.push_str(&fidelity::render_value(value));
                rest = &after[end + 2..];
                continue;
            }
            let mut visible: Vec<String> = pairs.iter().map(|(key, _)| key.clone()).collect();
            visible.sort();
            return Err(Unresolved {
                token: format!("{{{{ context.{name} }}}}"),
                visible,
            });
        }
        // Not a context token: copy the opening braces and continue after
        // them, so anything else between braces stays literal.
        out.push_str(&rest[..start + 2]);
        rest = &rest[start + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A context key: alphanumeric or `_`, then any of those plus `.` and `-`.
fn is_key(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The pairs a command script resolves against: the run context's public,
/// non-blank keys under the node's policy, WITHOUT the rendered-dedup — a
/// command consumes data, so a key a stage's output already rendered is
/// still readable (the documented fabro-e71b deviation).
#[must_use]
pub fn command_pairs(kv: &Value, policy: &PreamblePolicy) -> Vec<(String, Value)> {
    let Value::Object(map) = kv else {
        return Vec::new();
    };
    map.iter()
        .filter(|(key, value)| {
            !fidelity::is_hidden_key(key)
                && !fidelity::is_blank(value)
                && policy.context_visible(key)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs() -> Vec<(String, Value)> {
        vec![
            ("seed_id".to_owned(), serde_json::json!("fabro-1234")),
            ("mode".to_owned(), serde_json::json!("fast")),
        ]
    }

    /// Presence pin (fork feature, fabro-e71b): a visible token resolves to
    /// the value, exactly as `## Context` would render it.
    #[test]
    fn a_visible_token_resolves_to_the_value() {
        assert_eq!(
            resolve("Work {{ context.seed_id }} now", &pairs()).expect("resolves"),
            "Work fabro-1234 now"
        );
        assert_eq!(
            resolve("{{  context.mode  }}", &pairs()).expect("resolves"),
            "fast",
            "optional whitespace inside the braces"
        );
    }

    /// Strict: an unresolved token names the token and the visible keys.
    #[test]
    fn an_unresolved_token_names_the_token_and_the_visible_keys() {
        let err = resolve("{{ context.typo }}", &pairs()).expect_err("strict");
        assert_eq!(err.token, "{{ context.typo }}");
        assert_eq!(err.visible, ["mode", "seed_id"], "sorted by the object");
        let text = err.to_string();
        assert!(text.contains("`{{ context.typo }}`"), "{text}");
        assert!(text.contains("mode, seed_id"), "{text}");
    }

    /// Other namespaces and near-miss spellings stay literal: the pass is
    /// only the context namespace.
    #[test]
    fn other_namespaces_and_near_misses_stay_literal() {
        // `9x` IS a key by the grammar (digits allowed); `-x`, empty and
        // dotted-off spellings are not.
        let text = "{{ inputs.x }} {{ goal }} {{ context. }} {{ context.-x }} \
                    {{ contextit }} { context.a } {{context.a";
        assert_eq!(
            resolve(text, &pairs()).expect("resolves"),
            text,
            "nothing in this text is a context token"
        );
    }

    /// Single pass: a substituted value containing a token spelling lands
    /// as data, never re-scanned.
    #[test]
    fn a_substituted_value_is_never_rescanned() {
        let pairs = vec![(
            "mode".to_owned(),
            serde_json::json!("{{ context.seed_id }}"),
        )];
        assert_eq!(
            resolve("x{{ context.mode }}y", &pairs).expect("resolves"),
            "x{{ context.seed_id }}y"
        );
    }

    /// Oversized values render the bounded preview, exactly as `## Context`.
    #[test]
    fn oversized_values_render_the_bounded_preview() {
        let big = "z".repeat(100_000);
        let pairs = vec![("big".to_owned(), serde_json::json!(big))];
        let out = resolve("v={{ context.big }}", &pairs).expect("resolves");
        assert!(
            out.contains("bytes; too large to inline) Preview:"),
            "{out}"
        );
        assert!(out.len() < 1000, "{out}");
    }

    /// The empty projection refuses any token and names no keys.
    #[test]
    fn an_empty_projection_refuses_any_token() {
        let err = resolve("{{ context.any }}", &[]).expect_err("strict");
        assert!(err.to_string().contains("(none)"), "{}", err);
    }
}
