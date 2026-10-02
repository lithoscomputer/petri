//! What a step kind is handed, what it returns, and the one registry.
//!
//! A step kind is written once, as a [`Step`]: a name, a typed config, and one
//! attempt. Registration erases it into the two faces the rest of the system
//! uses — [`StepRunner`] for the driver to dispatch to, and [`ir::StepKind`]
//! for `validate_with` to check configs against at load. One definition, one
//! registry, so a kind with no runner or a config that cannot deserialize is
//! caught by `petri check` rather than at firing time.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use executor::{CONTAINER_RUNTIME_CLASS, ContainerRunner, ExecEnv, SecretProvider};
// The failure struct lives in `ir` (validation returns it too); step-kind code
// keeps reading it from here.
pub use ir::StepFailure;
use ir::placeholder::contains_placeholder;
use ir::{
    Attempt, Control, FailureClass, FiringId, Outcome, ScopeId, StepEvent, StepKind, StepKindId,
    StepKinds, Value,
};
use serde::de::DeserializeOwned;
use smol_str::SmolStr;
use tokio::sync::mpsc;

use crate::caps::Capabilities;
use crate::progress::ProgressSender;

/// The step's config did not deserialize.
pub(crate) const BAD_CONFIG_CLASS: FailureClass = FailureClass::new_static("bad_config");

/// Everything a step needs to run one attempt.
pub struct StepCtx {
    pub firing:       FiringId,
    pub attempt:      Attempt,
    /// How many attempts the node's retry policy allows in all, so a step
    /// knows when it is on its final one ([`StepCtx::is_final_attempt`]) and
    /// a result policy that depends on exhaustion can be applied where the
    /// step classifies its result.
    pub max_attempts: NonZeroU32,
    /// The resource scope the step runs in.
    pub scope:        ScopeId,
    /// The scope's environment: the fence identity the executor acquired
    /// it under, one per execution and scope. Executions that share a
    /// sandbox (a restart, a child that inherits its caller's) each have
    /// their own.
    pub environment:  SmolStr,
    /// The node's instance name, for log file naming and messages.
    pub node:         SmolStr,
    /// The resolved config. Free of expression placeholders; may hold `$secret`
    /// references, which are resolved here at spawn time and never written
    /// down.
    pub config:       Value,
    pub env:          Arc<dyn ExecEnv>,
    /// The scope-bound one-shot container runner, when the scope's executor
    /// provided one. Steps reach it through
    /// [`StepCtx::require_container_runner`], never a daemon of their own.
    pub runner:       Option<Arc<dyn ContainerRunner>>,
    pub secrets:      Arc<dyn SecretProvider>,
    /// Host services, looked up by type ([`StepCtx::capability`]). The core
    /// never names one.
    pub caps:         Capabilities,
    /// Progress out: logs, artifacts and step-defined events, in arrival
    /// order. `send` queues; `send_acked` resolves once the record is durable
    /// ([`ProgressSender`]).
    pub logs:         ProgressSender,
    /// Control in. A `Cancel` starts the ladder.
    pub control:      mpsc::Receiver<Control>,
}

impl StepCtx {
    /// Whether no attempt follows this one: a retryable failure returned now
    /// is the firing's final outcome.
    pub fn is_final_attempt(&self) -> bool {
        self.attempt.raw() >= self.max_attempts.get()
    }

    /// Emit a log line as if the step had printed it.
    pub async fn log(&self, stream: ir::LogStream, line: impl Into<String>) {
        let _ = self
            .logs
            .send(StepEvent::Log {
                stream,
                line: line.into(),
            })
            .await;
    }

    /// The host service of this type, if the host registered one.
    pub fn capability<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.caps.get::<T>()
    }

    /// The host service of this type, or the routable `capability_unavailable`
    /// failure — a step missing its host service fails its node, mirroring
    /// `secret_unavailable`.
    pub fn require_capability<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, StepFailure> {
        self.caps.require::<T>()
    }

    /// The scope-bound container runner, or the routable `container_runtime`
    /// failure when this scope's executor provided none — one clear error,
    /// never a daemon found by other means.
    pub fn require_container_runner(&self) -> Result<Arc<dyn ContainerRunner>, StepFailure> {
        self.runner.clone().ok_or_else(|| StepFailure {
            class:   CONTAINER_RUNTIME_CLASS,
            message: "this step runs a container, and the scope's executor provides no \
                      container runtime"
                .into(),
        })
    }
}

/// A step kind, as an author writes one.
///
/// One attempt per call. Retries are the core's business: a step never loops.
/// Registering a `Step` yields both faces — the runner the driver dispatches to
/// and the load-time validator — so a kind is defined exactly once.
///
/// `NAME` is the kind's identity: bare for the built-ins (`process`, `noop`),
/// `vendor/kind` for kinds defined in another repository (`attractor/llm`), so
/// two repositories never collide.
#[async_trait::async_trait]
pub trait Step: Send + Sync + 'static {
    const NAME: &'static str;

    /// The config's shape. Deserialization failure is class `bad_config` — at
    /// load when the config is literal, at firing time when it held
    /// placeholders.
    type Config: DeserializeOwned + Send;

    /// Checks on the raw config that outrank deserialization, with their own
    /// failure class. Run at load time and again before each attempt.
    fn check_raw(&self, _config: &Value) -> Result<(), StepFailure> {
        Ok(())
    }

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome;

    /// Reserved seam for content caching: `None` means "never reuse a result".
    fn fingerprint(&self, _config: &Value) -> Option<ir::Digest> {
        None
    }
}

/// A step kind, erased: what the driver dispatches to.
///
/// Authors implement [`Step`] and let registration erase it. Implementing this
/// directly is for the rare kind whose config handling fits no `Config` type;
/// it then carries its `StepKind` half by hand.
#[async_trait::async_trait]
pub trait StepRunner: StepKind {
    async fn run(&self, ctx: StepCtx) -> Outcome;
}

/// The erasure: one `Step` becomes both a `StepRunner` and a `StepKind`.
struct Erased<S>(S);

impl<S: Step> StepKind for Erased<S> {
    fn id(&self) -> StepKindId {
        StepKindId::new_static(S::NAME)
    }

    fn name(&self) -> &str {
        S::NAME
    }

    fn validate_config(&self, config: &Value) -> Result<(), StepFailure> {
        self.0.check_raw(config)?;
        if contains_placeholder(config) {
            // HIR: the value is still an expression. The typed check runs at firing
            // time, against the resolved config.
            return Ok(());
        }
        serde_json::from_value::<S::Config>(config.clone())
            .map(drop)
            .map_err(|e| StepFailure {
                class:   BAD_CONFIG_CLASS,
                message: e.to_string(),
            })
    }

    fn fingerprint(&self, config: &Value) -> Option<ir::Digest> {
        self.0.fingerprint(config)
    }
}

#[async_trait::async_trait]
impl<S: Step> StepRunner for Erased<S> {
    async fn run(&self, ctx: StepCtx) -> Outcome {
        if let Err(failure) = self.0.check_raw(&ctx.config) {
            return failure.into();
        }
        let config = match serde_json::from_value::<S::Config>(ctx.config.clone()) {
            Ok(config) => config,
            Err(e) => {
                return StepFailure {
                    class:   BAD_CONFIG_CLASS,
                    message: format!("step config is invalid: {e}"),
                }
                .into();
            }
        };
        Step::run(&self.0, config, ctx).await
    }
}

/// The step kinds a run can use: the driver dispatches through it, and
/// `validate_with` checks graphs against it, so a kind with no runner cannot
/// get past load.
#[derive(Clone, Default)]
pub struct Registry {
    runners: HashMap<StepKindId, Arc<dyn StepRunner>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed step kind under [`Step::NAME`].
    ///
    /// # Panics
    ///
    /// On a duplicate name. Registration is configuration, and two kinds under
    /// one name is a programming error the run must not paper over.
    pub fn register<S: Step>(&mut self, step: S) -> StepKindId {
        self.insert(Arc::new(Erased(step)))
    }

    /// Register a hand-erased runner.
    ///
    /// # Panics
    ///
    /// On a duplicate name. Same rule as [`Registry::register`].
    pub fn register_runner(&mut self, runner: Arc<dyn StepRunner>) -> StepKindId {
        self.insert(runner)
    }

    fn insert(&mut self, runner: Arc<dyn StepRunner>) -> StepKindId {
        let id = runner.id();
        assert!(
            !self.runners.contains_key(&id),
            "step kind `{id}` is already registered"
        );
        self.runners.insert(id.clone(), runner);
        id
    }

    pub fn get(&self, id: &StepKindId) -> Option<Arc<dyn StepRunner>> {
        self.runners.get(id).cloned()
    }
}

impl StepKinds for Registry {
    fn get(&self, id: &StepKindId) -> Option<&dyn StepKind> {
        self.runners.get(id).map(|r| {
            let runner: &dyn StepRunner = r.as_ref();
            runner as &dyn StepKind
        })
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.runners.values().map(|r| r.name()).collect();
        names.sort_unstable();
        f.debug_struct("Registry").field("kinds", &names).finish()
    }
}
