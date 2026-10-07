//! Run-creation rendering: `{{ inputs.* }}`, `{{ vars.* }}` and `{{ goal }}`
//! are rendered once, at lowering, so the graph the engine sees is literal.
//!
//! Prompts and the goal go through MiniJinja, the template language Fabro
//! uses, in strict mode: an unbound name is a diagnostic, never an empty
//! string. Command scripts get Fabro's simpler token interpolation — a
//! `{{ inputs.NAME }}` token becomes one shell word — so a script never runs
//! through a template engine that would eat its braces.

use std::borrow::Cow;
use std::collections::BTreeMap;

use frontend::{CompileInputs, FileSource};
use minijinja::{Environment, UndefinedBehavior};
use serde_json::Value;

/// Why a text did not render.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// A name the context does not bind. `name` is the dotted path as written.
    #[error("`{name}` is not bound")]
    Unbound { name: String },
    #[error("template syntax: {0}")]
    Syntax(String),
    #[error("template render: {0}")]
    Render(String),
}

impl TemplateError {
    /// Whether the failure is a missing input, which a host can fix by
    /// supplying one.
    pub fn is_unbound(&self) -> bool {
        matches!(self, Self::Unbound { .. })
    }
}

/// What templates may read.
pub struct Context {
    inputs: BTreeMap<String, Value>,
    vars:   BTreeMap<String, Value>,
    goal:   Option<String>,
}

impl Context {
    pub fn new(inputs: &CompileInputs) -> Self {
        Self {
            inputs: inputs
                .inputs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            vars:   inputs
                .vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            goal:   None,
        }
    }

    /// Fill in an input the host did not supply — a `workflow.toml` default.
    pub fn default_input(&mut self, name: &str, value: Value) {
        self.inputs.entry(name.to_string()).or_insert(value);
    }

    pub fn set_goal(&mut self, goal: String) {
        self.goal = Some(goal);
    }

    pub fn goal(&self) -> Option<&str> {
        self.goal.as_deref()
    }

    pub fn inputs(&self) -> &BTreeMap<String, Value> {
        &self.inputs
    }

    pub fn vars(&self) -> &BTreeMap<String, Value> {
        &self.vars
    }

    /// Whether a dotted name a template reads is bound: `inputs.X` when the
    /// input exists, `vars.X` likewise, `goal` when set, and the bare
    /// namespaces themselves.
    fn binds(&self, name: &str) -> bool {
        match name.split_once('.') {
            None => matches!(name, "inputs" | "vars") || (name == "goal" && self.goal.is_some()),
            Some(("inputs", rest)) => self
                .inputs
                .contains_key(rest.split('.').next().unwrap_or(rest)),
            Some(("vars", rest)) => self
                .vars
                .contains_key(rest.split('.').next().unwrap_or(rest)),
            Some(("goal", _)) => self.goal.is_some(),
            Some(_) => false,
        }
    }

    fn value(&self) -> minijinja::Value {
        let mut map = BTreeMap::new();
        map.insert("inputs", minijinja::Value::from_serialize(&self.inputs));
        map.insert("vars", minijinja::Value::from_serialize(&self.vars));
        if let Some(goal) = &self.goal {
            map.insert("goal", minijinja::Value::from(goal.as_str()));
        }
        minijinja::Value::from_serialize(map)
    }

    /// The text a scalar renders to — the same spelling in a prompt and a
    /// script, as Fabro guarantees.
    fn scalar_text(value: &Value) -> String {
        match value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    /// The text of one `inputs.*`, `vars.*` or `goal` token: the same
    /// spelling in a prompt, a script and a settings string.
    pub fn token_text(&self, token: &str) -> Result<String, TemplateError> {
        if token == "goal" {
            return self.goal.clone().ok_or_else(|| TemplateError::Unbound {
                name: "goal".into(),
            });
        }
        let (namespace, name) = token
            .split_once('.')
            .ok_or_else(|| TemplateError::Unbound {
                name: token.to_string(),
            })?;
        let map = match namespace {
            "inputs" => &self.inputs,
            "vars" => &self.vars,
            _ => {
                return Err(TemplateError::Unbound {
                    name: token.to_string(),
                });
            }
        };
        map.get(name)
            .map(Self::scalar_text)
            .ok_or_else(|| TemplateError::Unbound {
                name: token.to_string(),
            })
    }
}

/// Where `{% include %}` reads from: a repository and the directory the
/// including template lives in, so `partials/x.md.j2` beside a prompt file
/// resolves beside it.
pub struct Includes<'a> {
    pub files:    &'a dyn FileSource,
    pub base_dir: String,
}

/// The sentinel pair a masked `{{ context.NAME }}` token travels as
/// through the MiniJinja render (fabro-e71b): private-use characters
/// MiniJinja treats as plain text. Stage dispatch (`attractor/steps`
/// `fork_context_tokens`) restores and resolves them.
const CONTEXT_OPEN: char = '\u{E00B}';
const CONTEXT_CLOSE: char = '\u{E00C}';

/// Mask every `{{ context.NAME }}` token in `text` behind the sentinels, so
/// the MiniJinja pass leaves it verbatim instead of refusing it as an
/// unbound name (fabro-e71b: the token resolves at stage dispatch, against
/// the run context — lowering has none).
fn mask_context_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(rest);
            return out;
        };
        let token = after[..end].trim();
        if let Some(name) = token.strip_prefix("context.") {
            let first = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            let rest_ok = name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
            if first && rest_ok {
                out.push_str(&rest[..start]);
                out.push(CONTEXT_OPEN);
                out.push_str(name);
                out.push(CONTEXT_CLOSE);
                rest = &after[end + 2..];
                continue;
            }
        }
        out.push_str(&rest[..start + 2]);
        rest = &rest[start + 2..];
    }
    out.push_str(rest);
    out
}

/// Restore what [`mask_context_tokens`] masked. A sentinel without its
/// partner — a literal private-use character the author wrote — stays as
/// it is.
fn restore_context_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != CONTEXT_OPEN {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        loop {
            match chars.next() {
                Some(CONTEXT_CLOSE) => {
                    out.push_str("{{ context.");
                    out.push_str(name.as_str());
                    out.push_str(" }}");
                    break;
                }
                Some(c) => name.push(c),
                None => {
                    out.push(CONTEXT_OPEN);
                    out.push_str(&name);
                    return out;
                }
            }
        }
    }
    out
}

/// Render a prompt-like text with MiniJinja, strict about unbound names.
pub fn render(text: &str, ctx: &Context) -> Result<String, TemplateError> {
    render_with(text, ctx, None)
}

/// [`render`], with `{% include %}` resolved through `includes`.
pub fn render_with(
    text: &str,
    ctx: &Context,
    includes: Option<Includes<'_>>,
) -> Result<String, TemplateError> {
    // fabro-e71b: `{{ context.NAME }}` survives lowering verbatim, masked
    // for the MiniJinja pass and restored on the way out.
    let text = &mask_context_tokens(text);
    if !text.contains("{{") && !text.contains("{%") && !text.contains("{#") {
        return Ok(restore_context_tokens(text));
    }
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let root_name = includes.as_ref().map_or_else(
        || "__petri_root__".to_string(),
        |includes| join(&includes.base_dir, "__petri_root__"),
    );
    if let Some(includes) = includes {
        // Includes are read from the repository before the render, so the
        // loader owns a snapshot and needs no lifetime on the source.
        let files: &dyn FileSource = includes.files;
        let mut cache: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut pending = Vec::new();
        let mut names = Vec::new();
        collect_includes(text, &mut names);
        pending.extend(names.into_iter().map(|name| (name, root_name.clone())));
        while let Some((name, parent)) = pending.pop() {
            let path = join_template_path(&name, &parent);
            if cache.contains_key(&path) {
                continue;
            }
            let content = files.read(&path);
            if let Some(content) = &content {
                let mut children = Vec::new();
                collect_includes(content, &mut children);
                pending.extend(children.into_iter().map(|name| (name, path.clone())));
            }
            cache.insert(path, content);
        }
        env.set_path_join_callback(|name, parent| Cow::Owned(join_template_path(name, parent)));
        env.set_loader(move |name| Ok(cache.get(name).cloned().flatten()));
    }
    env.add_template_owned(root_name.clone(), text.clone())
        .map_err(|e| TemplateError::Syntax(e.to_string()))?;
    let template = env
        .get_template(&root_name)
        .map_err(|e| TemplateError::Syntax(e.to_string()))?;
    // Render first: what renders is bound, whatever a static reading says (a
    // `{% set %}` inside an `{% if %}` reads as undeclared to MiniJinja's
    // analysis, and renders fine). The analysis only names the missing input
    // afterwards, since MiniJinja's own undefined error does not say which
    // name it was; an input or var comes before a template-local name.
    template
        .render(ctx.value())
        .map(|rendered| restore_context_tokens(&rendered))
        .map_err(|e| match e.kind() {
            minijinja::ErrorKind::UndefinedError => {
                let mut unbound: Vec<String> = template
                    .undeclared_variables(true)
                    .into_iter()
                    .filter(|name| !ctx.binds(name))
                    .collect();
                unbound.sort_by_key(|name| {
                    let input =
                        name.starts_with("inputs.") || name.starts_with("vars.") || name == "goal";
                    (!input, name.clone())
                });
                TemplateError::Unbound {
                    name: unbound
                        .into_iter()
                        .next()
                        .or_else(|| undefined_name(&e))
                        .unwrap_or_else(|| "?".into()),
                }
            }
            _ => TemplateError::Render(e.to_string()),
        })
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

fn join_template_path(name: &str, parent: &str) -> String {
    let mut parts: Vec<&str> = parent.split('/').filter(|part| !part.is_empty()).collect();
    parts.pop();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// The names `{% include "..." %}` tags in `text` refer to, so a loader can
/// read them ahead of the render.
fn collect_includes(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(start) = rest.find("{%") {
        let Some(end) = rest[start..].find("%}") else {
            return;
        };
        let tag = rest[start + 2..start + end]
            .trim()
            .trim_start_matches(['-', '+'])
            .trim();
        if let Some(arg) = tag
            .strip_prefix("include")
            .or_else(|| tag.strip_prefix("import"))
            .or_else(|| tag.strip_prefix("extends"))
        {
            let arg = arg.trim().trim_start_matches("ignore missing").trim();
            let quoted = arg.trim_start_matches(['"', '\'']);
            let name: String = quoted
                .chars()
                .take_while(|c| *c != '"' && *c != '\'')
                .collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
        rest = &rest[start + end + 2..];
    }
}

/// The name a MiniJinja undefined error is about. MiniJinja's detail names
/// it in backticks in some sites and after a colon in others; failing both,
/// the whole detail is better than nothing.
fn undefined_name(error: &minijinja::Error) -> Option<String> {
    let detail = error.detail()?;
    if let Some(start) = detail.find('`')
        && let Some(end) = detail[start + 1..].find('`')
    {
        return Some(detail[start + 1..start + 1 + end].to_string());
    }
    Some(detail.trim().to_string())
}

/// Render a command script: every `{{ inputs.NAME }}`, `{{ vars.NAME }}` and
/// `{{ goal }}` token becomes one shell-quoted word (or, for Python, one
/// string literal). Anything else between braces stays literal.
pub fn render_script(script: &str, language: &str, ctx: &Context) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(script.len());
    let mut rest = script;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else {
            out.push_str(rest);
            return Ok(out);
        };
        let token = rest[start + 2..start + end].trim();
        let is_token = token
            .strip_prefix("inputs.")
            .or_else(|| token.strip_prefix("vars."))
            .is_some_and(|name| {
                !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
            })
            || token == "goal";
        out.push_str(&rest[..start]);
        if is_token {
            let text = ctx.token_text(token)?;
            out.push_str(&quote(&text, language)?);
        } else {
            out.push_str(&rest[start..start + end + 2]);
        }
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn quote(text: &str, language: &str) -> Result<String, TemplateError> {
    if language == "python" {
        return serde_json::to_string(text)
            .map_err(|error| TemplateError::Render(error.to_string()));
    }
    shlex::try_quote(text)
        .map(Cow::into_owned)
        .map_err(|error| TemplateError::Render(format!("cannot quote a shell value: {error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx() -> Context {
        let mut ctx = Context::new(
            &CompileInputs::new()
                .with_input("mode", "fast")
                .with_input("n", 3),
        );
        ctx.set_goal("Ship it".into());
        ctx
    }

    #[test]
    fn prompts_render_inputs_vars_and_goal() {
        assert_eq!(
            render("{{ goal }} in {{ inputs.mode }} x{{ inputs.n }}", &ctx()).expect("renders"),
            "Ship it in fast x3"
        );
        assert_eq!(
            render("plain {text}", &ctx()).expect("renders"),
            "plain {text}"
        );
    }

    #[test]
    fn unbound_names_are_errors_with_the_name() {
        let error = render("{{ inputs.missing }}", &ctx()).expect_err("unbound");
        assert_eq!(error, TemplateError::Unbound {
            name: "inputs.missing".into(),
        });
        assert!(
            render("{% if", &ctx())
                .expect_err("syntax")
                .to_string()
                .contains("syntax")
        );
    }

    /// fabro-e71b: a `{{ context.NAME }}` token survives the prompt render
    /// verbatim, masked behind the sentinels for the MiniJinja pass —
    /// it resolves at stage dispatch, against the run context.
    #[test]
    fn a_context_token_survives_the_prompt_render_verbatim() {
        assert_eq!(
            render(
                "goal {{ goal }} then {{ context.seed_id }} and {{ inputs.mode }}",
                &ctx()
            )
            .expect("renders"),
            "goal Ship it then {{ context.seed_id }} and fast"
        );
    }

    /// The mask survives template constructs: a token inside a control
    /// block still comes back verbatim, and an unbound name beside a
    /// masked token still fails strictly.
    #[test]
    fn a_context_token_survives_inside_control_structures() {
        assert_eq!(
            render(
                "{% if inputs.mode == \"fast\" %}{{ context.k }}{% endif %}",
                &ctx()
            )
            .expect("renders"),
            "{{ context.k }}"
        );
        let error = render("{{ inputs.nope }} {{ context.k }}", &ctx()).expect_err("unbound");
        assert_eq!(error, TemplateError::Unbound {
            name: "inputs.nope".into(),
        });
    }

    /// fabro-e71b: a script's context token stays verbatim at lowering —
    /// `render_script` only interpolates `inputs.`/`vars.`/`goal`; the
    /// stage dispatch pass resolves the context namespace.
    #[test]
    fn a_context_token_stays_verbatim_in_scripts() {
        assert_eq!(
            render_script(
                "run --seed {{ context.seed_id }} --mode {{ inputs.mode }}",
                "shell",
                &ctx()
            )
            .expect("renders"),
            "run --seed {{ context.seed_id }} --mode fast"
        );
    }

    #[test]
    fn scripts_interpolate_tokens_as_shell_words() {
        let mut ctx = ctx();
        ctx.default_input("msg", json!("hello world"));
        assert_eq!(
            render_script(
                "run --mode {{ inputs.mode }} --msg {{ inputs.msg }} ${X} {{ not.a.token }}",
                "shell",
                &ctx
            )
            .expect("renders"),
            "run --mode fast --msg 'hello world' ${X} {{ not.a.token }}"
        );
        assert_eq!(
            render_script("print({{ inputs.msg }})", "python", &ctx).expect("renders"),
            "print(\"hello world\")"
        );
        assert!(
            render_script("{{ inputs.nope }}", "shell", &ctx)
                .expect_err("unbound")
                .is_unbound()
        );
    }
}
