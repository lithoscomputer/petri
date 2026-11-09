//! The versioned black box scenario format: the loader for
//! `crates/fabro/acceptance/scenarios/<family>/<name>.scenario.json`
//! (`SCHEMA.md` there is the contract), the required-cell matrix, the
//! value matchers `expect` blocks use, placeholder substitution, and the
//! per-cell coverage record the report reads.
//!
//! Loading is strict: every unknown key is an error, and every row of
//! `expect` is required. A scenario that does not load never runs, so a typo
//! cannot silently drop an assertion.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, io};

use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The scenario format this loader reads.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Where the tracked scenarios live, relative to this crate's manifest.
pub(crate) const SCENARIOS_DIR: &str = "../../fabro/acceptance/scenarios";

pub(crate) const FAMILIES: &[&str] = &[
    "acp",
    "backend",
    "implement",
    "interview",
    "provider-faults",
    "routing",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scenario {
    pub(crate) schema_version: u32,
    pub(crate) id:             String,
    pub(crate) family:         String,
    pub(crate) title:          String,
    #[serde(default)]
    pub(crate) obligation:     Option<String>,
    pub(crate) bundle:         Bundle,
    #[serde(default)]
    pub(crate) inputs:         BTreeMap<String, Value>,
    #[serde(default)]
    pub(crate) fixture:        Fixture,
    pub(crate) modes:          Modes,
    #[serde(default)]
    pub(crate) services:       Services,
    #[serde(default)]
    pub(crate) interviews:     Vec<Value>,
    #[serde(default)]
    pub(crate) controls:       Controls,
    #[serde(default)]
    pub(crate) bounds:         Bounds,
    pub(crate) expect:         Expect,
    /// The scenario's own directory, for `file` references.
    #[serde(skip)]
    pub(crate) dir:            PathBuf,
}

/// The graph a scenario runs: a `.fabro` file beside the scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    pub(crate) inline: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fixture {
    #[serde(default)]
    pub(crate) commits:        Vec<Commit>,
    #[serde(default)]
    pub(crate) remotes:        BTreeMap<String, Remote>,
    #[serde(default)]
    pub(crate) bin:            BTreeMap<String, FileSpec>,
    #[serde(default)]
    pub(crate) env:            BTreeMap<String, String>,
    /// The host's user settings layer (`$FABRO_HOME/settings.toml`).
    #[serde(default)]
    pub(crate) settings:       Option<String>,
    /// Python modules the bundle's helpers import: the first `python3` on
    /// the launcher's PATH that imports them all is the run's `python3`.
    #[serde(default)]
    pub(crate) python_modules: Vec<String>,
    #[serde(default = "yes")]
    pub(crate) seed:           bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Commit {
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) write:   BTreeMap<String, FileSpec>,
    #[serde(default)]
    pub(crate) remove:  Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Remote {
    #[serde(default = "yes")]
    pub(crate) bare:     bool,
    #[serde(default = "main_branch")]
    pub(crate) branches: Vec<String>,
}

fn main_branch() -> Vec<String> {
    vec!["main".to_owned()]
}

/// A file's contents: inline text or a file beside the scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileSpec {
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) file: Option<String>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
}

impl FileSpec {
    /// The bytes, read beside `dir` when the spec names a file.
    pub(crate) fn bytes(&self, dir: &Path) -> Result<Vec<u8>, String> {
        match (&self.text, &self.file) {
            (Some(text), None) => Ok(text.clone().into_bytes()),
            (None, Some(file)) => fs::read(dir.join(file)).map_err(|e| format!("read {file}: {e}")),
            _ => Err("a file spec has exactly one of `text` and `file`".to_owned()),
        }
    }

    pub(crate) fn executable(&self) -> bool {
        self.mode.as_deref() == Some("0755")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Modes {
    pub(crate) backend: Backend,
    pub(crate) agent:   Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Backend {
    Host,
    Docker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum Agent {
    #[serde(rename = "api:openai")]
    OpenAi,
    #[serde(rename = "api:anthropic")]
    Anthropic,
    #[serde(rename = "api:openrouter")]
    OpenRouter,
    #[serde(rename = "acp")]
    Acp,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Services {
    #[serde(default)]
    pub(crate) openai:     Vec<Value>,
    #[serde(default)]
    pub(crate) anthropic:  Vec<Value>,
    #[serde(default)]
    pub(crate) openrouter: Vec<Value>,
    #[serde(default)]
    pub(crate) http:       Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Controls {
    #[serde(default)]
    pub(crate) interrupt_when:     Option<InterruptWhen>,
    #[serde(default)]
    pub(crate) control_lines:      Vec<ControlLine>,
    #[serde(default)]
    pub(crate) stdin:              Option<Stdin>,
    #[serde(default)]
    pub(crate) path_without_fabro: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InterruptWhen {
    #[serde(default)]
    pub(crate) file:           Option<String>,
    #[serde(default)]
    pub(crate) container_file: Option<String>,
    #[serde(default)]
    pub(crate) request:        Option<String>,
    /// A stderr line of the run containing this text.
    #[serde(default)]
    pub(crate) stderr:         Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlLine {
    pub(crate) after_file: String,
    pub(crate) line:       String,
    #[serde(default)]
    pub(crate) delay_ms:   u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Stdin {
    pub(crate) text:  String,
    #[serde(default)]
    pub(crate) close: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bounds {
    #[serde(default = "default_deadline")]
    pub(crate) deadline_ms: u64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            deadline_ms: default_deadline(),
        }
    }
}

fn default_deadline() -> u64 {
    120_000
}

/// Every row of the phase 3 observation table. All rows are required.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expect {
    pub(crate) process:      ProcessExpect,
    pub(crate) context:      ContextExpect,
    pub(crate) files:        Vec<FileExpect>,
    pub(crate) side_effects: SideEffects,
    pub(crate) providers:    BTreeMap<String, ProviderExpect>,
    pub(crate) interviews:   InterviewExpect,
    pub(crate) lifecycle:    LifecycleExpect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessExpect {
    pub(crate) exit_code:       i32,
    pub(crate) status:          String,
    #[serde(default)]
    pub(crate) required_nodes:  Vec<String>,
    #[serde(default)]
    pub(crate) forbidden_nodes: Vec<String>,
    #[serde(default)]
    pub(crate) visits:          BTreeMap<String, u64>,
    /// How many attempts a node's final firing took, from the node records:
    /// how a retry within one firing is observed.
    #[serde(default)]
    pub(crate) attempts:        BTreeMap<String, u64>,
    /// Texts the run's terminal output (stderr) must contain: the echoed
    /// step lines and the run's own notices.
    #[serde(default)]
    pub(crate) stderr_contains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextExpect {
    #[serde(default)]
    pub(crate) exact:         Map<String, Value>,
    #[serde(default)]
    pub(crate) absent:        Vec<String>,
    #[serde(default)]
    pub(crate) extra_allowed: Vec<String>,
    #[serde(default)]
    pub(crate) complete:      bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileExpect {
    pub(crate) path:     String,
    #[serde(default)]
    pub(crate) text:     Option<String>,
    #[serde(default)]
    pub(crate) file:     Option<String>,
    #[serde(default)]
    pub(crate) contains: Vec<String>,
    #[serde(default)]
    pub(crate) json:     Option<Value>,
    #[serde(default)]
    pub(crate) absent:   bool,
    #[serde(default)]
    pub(crate) lines:    Option<usize>,
    #[serde(default)]
    pub(crate) mode:     Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SideEffects {
    #[serde(default)]
    pub(crate) git:  Vec<GitExpect>,
    #[serde(default)]
    pub(crate) http: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitExpect {
    pub(crate) repo:          String,
    #[serde(rename = "ref")]
    pub(crate) reference:     String,
    #[serde(default)]
    pub(crate) is:            Option<String>,
    #[serde(default)]
    pub(crate) advanced_from: Option<String>,
    #[serde(default = "one")]
    pub(crate) commits:       u64,
    #[serde(default)]
    pub(crate) message:       Option<String>,
    #[serde(default)]
    pub(crate) absent:        bool,
}

fn one() -> u64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderExpect {
    #[serde(default)]
    pub(crate) consumed:  Option<Value>,
    #[serde(default)]
    pub(crate) unmatched: Option<usize>,
    #[serde(default)]
    pub(crate) requests:  Option<usize>,
    #[serde(default)]
    pub(crate) model:     Option<String>,
    #[serde(default)]
    pub(crate) effort:    Option<String>,
    #[serde(default)]
    pub(crate) contains:  Vec<RequestContains>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestContains {
    pub(crate) request: usize,
    pub(crate) text:    String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InterviewExpect {
    #[serde(default)]
    pub(crate) questions:    Vec<Value>,
    #[serde(default)]
    pub(crate) errors:       Vec<Value>,
    #[serde(default)]
    pub(crate) consumed:     BTreeMap<String, u64>,
    #[serde(default)]
    pub(crate) no_plaintext: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LifecycleExpect {
    #[serde(default = "yes")]
    pub(crate) no_leaked_processes:      bool,
    #[serde(default = "yes")]
    pub(crate) retained_workspace:       bool,
    #[serde(default)]
    pub(crate) no_requests_after_cancel: bool,
    #[serde(default)]
    pub(crate) cancel_reason:            Option<String>,
    #[serde(default)]
    pub(crate) prune_removes_sandbox:    bool,
}

impl Scenario {
    /// The scenarios directory of the checkout.
    pub(crate) fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(SCENARIOS_DIR)
    }

    /// Load `<family>/<name>` from the tracked scenarios and validate it
    /// against the schema rules and the bundle lock.
    pub(crate) fn from_id(id: &str) -> Result<Self, String> {
        Self::load(&Self::root().join(format!("{id}.scenario.json")))
    }

    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut scenario: Self =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        scenario.dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        scenario.validate(path)?;
        Ok(scenario)
    }

    /// Every tracked scenario file, sorted by path.
    pub(crate) fn all() -> Result<Vec<(PathBuf, Self)>, String> {
        let mut files = Vec::new();
        walk(&Self::root(), &mut files).map_err(|e| e.to_string())?;
        files.sort();
        files
            .into_iter()
            .map(|path| Self::load(&path).map(|scenario| (path, scenario)))
            .collect()
    }

    fn validate(&self, path: &Path) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "{}: schema_version {} is not {SCHEMA_VERSION}",
                path.display(),
                self.schema_version
            ));
        }
        let root = Self::root();
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| format!("{} is outside {}", path.display(), root.display()))?;
        let expected_id = relative
            .to_string_lossy()
            .trim_end_matches(".scenario.json")
            .to_owned();
        if self.id != expected_id {
            return Err(format!(
                "{}: id `{}` does not match the path (`{expected_id}`)",
                path.display(),
                self.id
            ));
        }
        if !FAMILIES.contains(&self.family.as_str()) {
            return Err(format!(
                "{}: unknown family `{}`",
                path.display(),
                self.family
            ));
        }
        if !self.id.starts_with(&format!("{}/", self.family)) {
            return Err(format!(
                "{}: family `{}` is not the id's first segment",
                path.display(),
                self.family
            ));
        }
        if self.title.trim().is_empty() {
            return Err(format!("{}: empty title", path.display()));
        }
        let inline = &self.bundle.inline;
        if !self.dir.join(inline).is_file() {
            return Err(format!(
                "{}: inline graph `{inline}` is not beside the scenario",
                path.display()
            ));
        }
        if !matches!(
            self.expect.process.status.as_str(),
            "success" | "failed" | "cancelled"
        ) {
            return Err(format!(
                "{}: process.status `{}` is not success, failed or cancelled",
                path.display(),
                self.expect.process.status
            ));
        }
        for file in &self.expect.files {
            let ways = usize::from(file.text.is_some())
                + usize::from(file.file.is_some())
                + usize::from(!file.contains.is_empty())
                + usize::from(file.json.is_some())
                + usize::from(file.absent)
                + usize::from(file.lines.is_some());
            if ways == 0 && file.mode.is_none() {
                return Err(format!(
                    "{}: file `{}` asserts nothing",
                    path.display(),
                    file.path
                ));
            }
        }
        for provider in self.expect.providers.keys() {
            if !matches!(provider.as_str(), "openai" | "anthropic" | "openrouter") {
                return Err(format!(
                    "{}: unknown provider `{provider}` in expect.providers",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// The workflow file to run, relative to the scenario directory.
    pub(crate) fn entry_point(&self) -> &str {
        &self.bundle.inline
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, out)?;
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".scenario.json"))
        {
            out.push(path);
        }
    }
    Ok(())
}

// ── The matrix ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Matrix {
    pub(crate) schema_version: u32,
    pub(crate) description:    String,
    pub(crate) cells:          Vec<Cell>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cell {
    /// The cell's name, `<scenario>@<backend>/<agent>`.
    #[serde(rename = "cell")]
    pub(crate) name:     String,
    pub(crate) scenario: Option<String>,
    pub(crate) backend:  Backend,
    pub(crate) agent:    Agent,
    pub(crate) required: bool,
    pub(crate) status:   CellStatus,
    pub(crate) reason:   Option<String>,
    pub(crate) test:     Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CellStatus {
    Planned,
    Blocked,
    Excluded,
}

impl Matrix {
    pub(crate) fn load() -> Result<Self, String> {
        let path = Scenario::root().join("matrix.json");
        let text =
            fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let matrix: Self =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if matrix.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "matrix.json: schema_version is not {SCHEMA_VERSION}"
            ));
        }
        let mut seen = BTreeSet::new();
        for cell in &matrix.cells {
            if !seen.insert(cell.name.clone()) {
                return Err(format!("matrix.json: cell `{}` is listed twice", cell.name));
            }
            match cell.status {
                CellStatus::Planned if cell.test.is_none() => {
                    return Err(format!(
                        "matrix.json: planned cell `{}` names no test",
                        cell.name
                    ));
                }
                CellStatus::Blocked | CellStatus::Excluded if cell.reason.is_none() => {
                    return Err(format!(
                        "matrix.json: cell `{}` is {:?} with no reason",
                        cell.name, cell.status
                    ));
                }
                _ => {}
            }
        }
        Ok(matrix)
    }

    pub(crate) fn cell(&self, name: &str) -> Option<&Cell> {
        self.cells.iter().find(|cell| cell.name == name)
    }
}

// ── Placeholders ────────────────────────────────────────────────────────────

/// The values `{name}` placeholders substitute to.
#[derive(Debug, Clone, Default)]
pub(crate) struct Bindings(pub(crate) BTreeMap<String, String>);

impl Bindings {
    pub(crate) fn bind(&mut self, name: &str, value: impl Into<String>) {
        self.0.insert(name.to_owned(), value.into());
    }

    /// Replace every `{name}` this binding set knows. Unknown braces stay,
    /// so JSON text and shell braces survive.
    pub(crate) fn text(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (name, value) in &self.0 {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }

    pub(crate) fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.value(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

// ── Matchers ────────────────────────────────────────────────────────────────

/// Compare an actual value with an expected value or matcher object, as
/// `SCHEMA.md` defines them. `None` is an absent value.
pub(crate) fn matches(expected: &Value, actual: Option<&Value>) -> Result<(), String> {
    if let Some(map) = expected.as_object()
        && map.len() == 1
        && let Some((key, arg)) = map.iter().next()
        && key.starts_with('$')
    {
        return matcher(key, arg, actual);
    }
    match actual {
        None => Err(format!("expected {expected} but the value is absent")),
        Some(actual) => {
            if let (Some(expected), Some(actual)) = (expected.as_object(), actual.as_object()) {
                for (key, value) in expected {
                    matches(value, actual.get(key)).map_err(|e| format!("`{key}`: {e}"))?;
                }
                let extra: Vec<&String> = actual
                    .keys()
                    .filter(|key| !expected.contains_key(*key))
                    .collect();
                if extra.is_empty() {
                    Ok(())
                } else {
                    Err(format!("unexpected keys {extra:?} in {actual:?}"))
                }
            } else if let (Some(expected), Some(actual)) = (expected.as_array(), actual.as_array())
            {
                if expected.len() != actual.len() {
                    return Err(format!(
                        "expected {} elements, found {}: {actual:?}",
                        expected.len(),
                        actual.len()
                    ));
                }
                for (index, (e, a)) in expected.iter().zip(actual).enumerate() {
                    matches(e, Some(a)).map_err(|err| format!("[{index}]: {err}"))?;
                }
                Ok(())
            } else if expected == actual {
                Ok(())
            } else {
                Err(format!("expected {expected}, found {actual}"))
            }
        }
    }
}

fn matcher(key: &str, arg: &Value, actual: Option<&Value>) -> Result<(), String> {
    let present = || actual.ok_or_else(|| format!("{key}: the value is absent"));
    match key {
        "$any" => present().map(|_| ()),
        "$absent" => match actual {
            None | Some(Value::Null) if arg == &json!(true) => Ok(()),
            _ => Err(format!("expected an absent value, found {actual:?}")),
        },
        "$regex" => {
            let pattern = arg.as_str().ok_or("$regex takes a string")?;
            let regex = regex::Regex::new(pattern).map_err(|e| format!("$regex: {e}"))?;
            let text = present()?
                .as_str()
                .ok_or_else(|| format!("$regex: {actual:?} is not a string"))?;
            if regex.is_match(text) {
                Ok(())
            } else {
                Err(format!("`{text}` does not match /{pattern}/"))
            }
        }
        "$contains" => match present()? {
            Value::String(text) => {
                let needle = arg.as_str().ok_or("$contains on a string takes a string")?;
                if text.contains(needle) {
                    Ok(())
                } else {
                    Err(format!("`{text}` does not contain `{needle}`"))
                }
            }
            Value::Array(items) => {
                if items.iter().any(|item| matches(arg, Some(item)).is_ok()) {
                    Ok(())
                } else {
                    Err(format!("{items:?} does not contain {arg}"))
                }
            }
            other => Err(format!(
                "$contains: {other} is neither a string nor an array"
            )),
        },
        "$len" => {
            let want = arg.as_u64().ok_or("$len takes a number")?;
            let have = match present()? {
                Value::String(text) => text.chars().count() as u64,
                Value::Array(items) => items.len() as u64,
                Value::Object(map) => map.len() as u64,
                other => return Err(format!("$len: {other} has no length")),
            };
            if want == have {
                Ok(())
            } else {
                Err(format!("expected length {want}, found {have}: {actual:?}"))
            }
        }
        "$set" => {
            let want = arg.as_array().ok_or("$set takes an array")?;
            let have = present()?
                .as_array()
                .ok_or_else(|| format!("$set: {actual:?} is not an array"))?;
            if want.len() != have.len() {
                return Err(format!(
                    "expected {} elements, found {}: {have:?}",
                    want.len(),
                    have.len()
                ));
            }
            let mut remaining: Vec<&Value> = have.iter().collect();
            for wanted in want {
                let Some(position) = remaining
                    .iter()
                    .position(|item| matches(wanted, Some(item)).is_ok())
                else {
                    return Err(format!("{wanted} is not in {have:?}"));
                };
                remaining.remove(position);
            }
            Ok(())
        }
        "$subset" => {
            let want = arg.as_object().ok_or("$subset takes an object")?;
            let have = present()?
                .as_object()
                .ok_or_else(|| format!("$subset: {actual:?} is not an object"))?;
            for (key, value) in want {
                matches(value, have.get(key)).map_err(|e| format!("`{key}`: {e}"))?;
            }
            Ok(())
        }
        "$type" => {
            let want = arg.as_str().ok_or("$type takes a string")?;
            let have = match present()? {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => "object",
            };
            if want == have {
                Ok(())
            } else {
                Err(format!("expected a {want}, found a {have}: {actual:?}"))
            }
        }
        other => Err(format!("unknown matcher `{other}`")),
    }
}

// ── Coverage records ────────────────────────────────────────────────────────

/// Where per-cell results go: `$PETRI_FABRO_COVERAGE_DIR`, else
/// `target/fabro-coverage/results` under the workspace.
pub(crate) fn coverage_dir() -> PathBuf {
    env::var_os("PETRI_FABRO_COVERAGE_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/fabro-coverage/results"),
        PathBuf::from,
    )
}

/// One cell's result file. Created when the test starts as `failed`, so a
/// panic or a killed process leaves a failure on record; `pass` rewrites it.
pub(crate) struct CellRecord {
    path:    PathBuf,
    cell:    String,
    /// Set once the record has its final state (`passed` or `skipped`), so
    /// dropping it writes nothing more.
    settled: bool,
}

impl CellRecord {
    pub(crate) fn start(cell: &str) -> Self {
        Self::start_in(&coverage_dir(), cell)
    }

    /// [`start`](Self::start) with an explicit results directory.
    pub(crate) fn start_in(dir: &Path, cell: &str) -> Self {
        let _ = fs::create_dir_all(dir);
        let path = dir.join(format!("{}.json", cell.replace(['/', '@'], "__")));
        let record = Self {
            path,
            cell: cell.to_owned(),
            settled: false,
        };
        record.write("failed", Some("the test did not report a pass"));
        record
    }

    pub(crate) fn skip(cell: &str, reason: &str) {
        Self::skip_in(&coverage_dir(), cell, reason);
    }

    /// [`skip`](Self::skip) with an explicit results directory.
    pub(crate) fn skip_in(dir: &Path, cell: &str, reason: &str) {
        let mut record = Self::start_in(dir, cell);
        record.settled = true;
        record.write("skipped", Some(reason));
    }

    pub(crate) fn pass(mut self) {
        self.settled = true;
        self.write("passed", None);
    }

    fn write(&self, status: &str, note: Option<&str>) {
        let record = json!({
            "cell": self.cell,
            "status": status,
            "note": note,
            "recorded_at": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
        });
        let _ = fs::write(
            &self.path,
            serde_json::to_vec_pretty(&record).unwrap_or_default(),
        );
    }
}

impl Drop for CellRecord {
    fn drop(&mut self) {
        if !self.settled {
            self.write("failed", Some("the test ended without reporting a pass"));
        }
    }
}
