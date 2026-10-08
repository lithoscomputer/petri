//! What every workflow format implements.
//!
//! The CLI and the corpus harness hold a list of [`Frontend`]s and ask each one
//! whether it claims a path, instead of matching on an enum. Adding a format is
//! a new crate that implements this trait, and one entry in that list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use smol_str::SmolStr;

use crate::diag::Lowered;
use crate::files::FileSource;

/// What a host supplies *before* lowering: the run inputs and variables a
/// format renders into its file at compile time (Fabro's `{{ inputs.* }}`
/// and `{{ vars.* }}`). Distinct from [`Frontend::default_params`], which
/// fills `Graph.params` *after* lowering and never changes the graph's
/// shape. A format that renders nothing ignores it.
/// The compile variable the runtime binds to the repository root a file is
/// loaded from (`--repo`, else the frontend's `repo_root`), as an absolute
/// path. A format whose runs start from a checkout of that repository reads
/// it at load; a host that lowers in memory leaves it unset.
pub const REPOSITORY_VAR: &str = "petri.repository";

/// The compile variables `petri run --model` and `--provider` bind: the
/// model the launch asks for, as `fabro run --model` and `--provider` do. A
/// format whose LLM nodes may name a model reads them over its run
/// configuration and the file's own defaults, but below a model a node names
/// itself, and records them in its launch parameter, so the persisted graph
/// carries the launch. Each applies on its own. A provider alone, where
/// nothing names a model, means the provider's default model in the runner's
/// catalog. Unset when the launch named none.
pub const LAUNCH_MODEL_VAR: &str = "petri.launch_model";
pub const LAUNCH_PROVIDER_VAR: &str = "petri.launch_provider";

/// The compile variables a host binds to the model a run falls back to,
/// such as a server's catalog default: the last default, below everything
/// the file and its run configuration name. `petri run` binds neither.
/// Unset when the host has no default.
pub const DEFAULT_MODEL_VAR: &str = "petri.default_model";
pub const DEFAULT_PROVIDER_VAR: &str = "petri.default_provider";

/// The compile variable `petri run --environment` binds: the execution
/// environment the launch selects, by the id a format's run configuration
/// declares it under. A format with environments reads it over every file
/// layer, as `fabro run --environment` overrides the files; a format without
/// them ignores it. Unset when the launch named none.
pub const LAUNCH_ENVIRONMENT_VAR: &str = "petri.launch_environment";

/// The compile variable `petri run --goal` binds: the run goal the launch
/// states, as text. A format whose runs have a goal reads it over its run
/// configuration's goal and over the file's own, as `fabro run --goal` and a
/// Fabro run's goal override do; a format without a goal ignores it. Unset
/// when the launch stated none.
pub const LAUNCH_GOAL_VAR: &str = "petri.launch_goal";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompileInputs {
    pub inputs:             BTreeMap<SmolStr, Value>,
    pub vars:               BTreeMap<SmolStr, Value>,
    /// Whether a template that reads an input no one supplied is a warning
    /// that leaves the text unrendered, instead of an error. `petri check`
    /// sets this when it was given no inputs at all, so a workflow validates
    /// before its inputs exist. A run never sets it.
    pub unbound_is_warning: bool,
}

impl CompileInputs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Report an unbound input as a warning and leave its text unrendered.
    #[must_use]
    pub fn with_unbound_as_warning(mut self) -> Self {
        self.unbound_is_warning = true;
        self
    }

    #[must_use]
    pub fn with_input(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.inputs.insert(SmolStr::new(name), value.into());
        self
    }

    #[must_use]
    pub fn with_var(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.vars.insert(SmolStr::new(name), value.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty() && self.vars.is_empty()
    }
}

/// A workflow format: text in, graph and diagnostics out.
/// When a host keeps a run's workspaces after the run, as a format declares
/// it. The host maps this onto its executor's retention policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceRetention {
    /// Keep every workspace: after success, failure and cancellation.
    Always,
    /// Keep a workspace whose run failed; delete a successful run's.
    #[default]
    OnFailure,
    /// Delete every workspace.
    Never,
}

/// What a workflow's own configuration says about how a standalone host
/// should launch it, read back from the lowered graph. A host applies these
/// only where the user gave no explicit option. Every field is optional: a
/// format with no launch configuration returns the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchSettings {
    /// The sandbox backend the workflow asks for (`host`, `docker`,
    /// `daytona`), in the host's spelling.
    pub sandbox_backend: Option<String>,
    /// Simulate the format's stages instead of running them.
    pub dry_run:         bool,
    /// Answer every question with its default choice.
    pub auto_approve:    bool,
}

pub trait Frontend: Send + Sync {
    /// Short, stable, lower-case and unique: `gha`, `native`. What `--format`
    /// takes.
    fn name(&self) -> &str;

    /// Whether files at `path` are, by convention, in this format.
    ///
    /// Frontends are asked in registration order, so a format that claims every
    /// path goes last.
    fn claims(&self, path: &Path) -> bool;

    /// Parse and lower one file. `file` is the name spans carry — usually the
    /// repository-relative path — `files` resolves local includes, and
    /// `inputs` carries what the host supplies before lowering.
    fn load(
        &self,
        file: &str,
        text: &str,
        files: &dyn FileSource,
        inputs: &CompileInputs,
    ) -> Lowered;

    /// What a standalone host does with a run's workspaces when the user did
    /// not say. The default keeps a failed run's workspace and deletes a
    /// successful one's; a format whose result *is* the workspace (Fabro)
    /// keeps every workspace.
    fn default_retention(&self) -> WorkspaceRetention {
        WorkspaceRetention::OnFailure
    }

    /// Run parameters a host owes this format when it has nothing better: fixed
    /// values, so lowering the same file twice yields the identical graph and a
    /// saved log replays against it. `repo` is the repository root. Default:
    /// none.
    fn default_params(&self, _repo: &Path) -> Vec<(SmolStr, Value)> {
        Vec::new()
    }

    /// Whether `graph` is this format's own lowering, read from the graph
    /// alone (a marker in its `params`). A host that holds only a stored
    /// graph, such as `petri resume`, asks this to find the format whose
    /// launch settings and defaults apply. Default: no.
    fn claims_graph(&self, _graph: &ir::Graph) -> bool {
        false
    }

    /// The launch settings the workflow's own configuration declares, read
    /// from the lowered graph (its `params`), so a persisted graph carries
    /// them. Default: none.
    fn launch_settings(&self, _graph: &ir::Graph) -> LaunchSettings {
        LaunchSettings::default()
    }

    /// Where the repository root is above `file`, when the host was not told. A
    /// format whose files sit at a fixed place in a repository walks up to it;
    /// the default is the file's own directory.
    ///
    /// It is the root local includes resolve against, and the prefix stripped
    /// from the name spans carry.
    fn repo_root(&self, file: &Path) -> PathBuf {
        file.parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }
}

/// The frontend called `name`, if any.
pub fn by_name<'a>(frontends: &[&'a dyn Frontend], name: &str) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.name() == name)
}

/// The first frontend that claims `path`.
pub fn detect<'a>(frontends: &[&'a dyn Frontend], path: &Path) -> Option<&'a dyn Frontend> {
    frontends.iter().copied().find(|f| f.claims(path))
}
