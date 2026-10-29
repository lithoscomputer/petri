//! Petri, as it ships.
//!
//! This is the distribution: the one crate a consumer depends on, and the only
//! crate in the tree that names every component. Core is `petri-runtime` and
//! knows the native format and nothing else; each component — today, GitHub
//! Actions — is its own crate and knows core. Here the two meet: [`runtime()`]
//! is core's standard runtime with every in-tree component registered on it.
//!
//! Everything else is re-export. [`ir`], [`engine`], [`driver`], [`frontend`]
//! (with [`frontend::gha`] and [`frontend::native`]), [`steps`] and
//! [`executor`] (with [`executor::sandbox`], the executors) are
//! reachable through this crate, so an external repository names one dependency
//! and never a layer crate.
//!
//! The one thing here that is not wiring or re-export is [`host`]: the
//! standalone host's durable coordinator layout — content-addressed graphs,
//! one engine log per execution, and the run wrappers that keep it resumable
//! when no product store sits behind it.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let rt = petri::runtime().options(petri::RunOptions::new("/tmp/petri-demo"));
//! let inputs = petri::frontend::CompileInputs::new();
//! let lowered = rt.check(std::path::Path::new("pipeline.yml"), None, None, &inputs)?;
//! if let Some(graph) = lowered.graph {
//!     let report = rt.run(graph).await?;
//!     println!("{}", report.status);
//! }
//! # Ok(())
//! # }
//! ```

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

use attractor_steps::pebble::PebbleClient;
use attractor_steps::skills::FabroHome;
pub use execution::{self, host};
use frontend_gha::exprs::GITHUB_TOKEN_SECRET;
use lithos_llm::catalog::{Catalog, CatalogError};
use lithos_llm::client::ClientBuildError;
use lithos_llm::credentials::{ConventionalCredentials, CredentialProvider};
use lithos_llm::middleware::{RetryMiddleware, RetryPolicy};
use pebble_coding_agent::events::RetryEventObserver;
pub use runtime::{
    DaytonaResources, DaytonaSandboxKind, LostSandbox, RunOptions, Runtime, SandboxBackend,
    SandboxOptions, driver, engine, ir,
};

/// The executor interface, with the sandbox-driver adapter as a submodule.
pub mod executor {
    pub use runtime::executor::*;
}

/// The frontend interface, with every format this distribution ships.
pub mod frontend {
    pub use frontend_attractor as attractor;
    pub use frontend_fabro as fabro;
    pub use frontend_gha as gha;
    pub use runtime::frontend::*;
}

/// The Attractor component's run-time half: its step kinds and the stub
/// registry. A Fabro workflow runs on these.
pub mod attractor {
    pub use attractor_steps::*;
}

/// The Fabro component: the frontend that wraps the Attractor language in
/// Fabro's settings layers. Its step kinds are [`attractor`]'s.
pub mod fabro {
    pub use frontend_fabro::*;
}

/// Step kinds, with the standard registry.
pub mod steps {
    pub use runtime::steps::*;
}

/// The GitHub Actions component's run-time half: its step kinds and action
/// source.
pub mod github {
    pub use github_actions::*;
}

/// The shipped configuration: core's standard runtime plus every in-tree
/// component.
///
/// GitHub Actions comes with an action source that fetches from GitHub into
/// [`github::default_cache_dir`], its step kinds, and `GITHUB_TOKEN` as a
/// secret: this machine's (`$GITHUB_TOKEN`, else `gh auth token`), or the empty
/// string with a warning when it has none — actions then run anonymously. Its
/// runner map knows the `ubuntu-*` labels; `PETRI_RUNNER_LABELS` (labels
/// separated by commas or whitespace) adds third-party or self-hosted labels
/// that name Linux environments this machine can stand in for.
///
/// Every run also gets the per-run **ObjectService** — the local stand-in for
/// GitHub's results backend — started beside the run dir (artifacts under
/// `<run_dir>/artifacts`, released with the run) and handed to action steps as
/// `ACTIONS_RESULTS_URL`/`ACTIONS_RUNTIME_TOKEN`. When it cannot start, the
/// run proceeds and only the steps that need the backend fail, routably.
///
/// A consumer that wants a different set builds one itself —
/// `Runtime::standard()` for core alone, `Runtime::bare()` for nothing — and
/// registers what it wants.
pub fn runtime() -> Runtime {
    assemble(attractor_steps::register)
}

/// [`runtime`] with the Attractor step kinds simulated — `petri run --dry-run`:
/// every stage succeeds and a human gate takes its first choice, as Fabro's
/// own `--dry-run` does. A dry run touches no provider: every scope is
/// acquired on the simulated provider, whatever backend the workflow's
/// environment selects, so no sandbox plugin is needed. The GitHub Actions
/// kinds have no simulation; a step of theirs that runs a process fails,
/// routably, because a simulated sandbox runs none.
pub fn dry_run_runtime() -> Runtime {
    assemble(attractor_steps::register_stubs).simulated_sandboxes()
}

fn assemble(attractor: fn(Runtime) -> Runtime) -> Runtime {
    let secrets = GithubSecrets::new();
    // The provisioner registers the results token with this run's mask set, so
    // it is built ahead of the closure that captures its masker.
    let masker = executor::SecretProvider::masker(&secrets);
    let actions = Arc::new(github::GitActionSource::new(github::default_cache_dir()));
    let manifests: Arc<dyn github::ActionSource> = actions.clone();
    let trees: Arc<dyn github::ActionTreeSource> = actions;
    let runners = frontend_gha::RunnerMap::builtin()
        .allow_list(&env::var("PETRI_RUNNER_LABELS").unwrap_or_default());
    // `PETRI_REAL_CHECKOUT` (non-empty) turns the local-checkout substitution
    // off: every `actions/checkout` stays the real action, credentials,
    // network and all.
    let substitute_checkout = !env::var("PETRI_REAL_CHECKOUT").is_ok_and(|v| !v.is_empty());
    // The persistent store: cache entries and the per-OS tool cache under one
    // root ($PETRI_STORE overrides). Created now so the tool-cache prologue's
    // existence probe finds it on the host.
    let store = github_objects::default_store_dir();
    let tool_cache = github_objects::tool_cache_dir(&store);
    if let Err(error) = fs::create_dir_all(&tool_cache) {
        // The prologue's probe then finds nothing, and every `setup-*` action
        // reinstalls its tool on every run.
        tracing::warn!(
            path = %tool_cache.display(),
            error = ?error,
            "tool cache directory could not be created"
        );
    }
    // The Fabro frontend is the one registered for DOT files: it wraps the
    // Attractor language in Fabro's settings layers and degrades to the bare
    // language when no settings file exists. The Fabro home reaches the
    // skills step as a capability, so the step reads the environment only
    // when no host named one.
    let mut runtime = Runtime::standard()
        .frontend(frontend_fabro::Fabro::new().with_settings_toml(fabro_settings_toml()))
        .frontend(
            frontend_gha::GitHubActions::with_actions(manifests.clone())
                .with_runners(runners)
                .with_checkout_substitution(substitute_checkout),
        );
    if let Some(home) = FabroHome::from_env() {
        runtime = runtime.capability(home);
    }
    let runtime = match llm_client() {
        Ok(client) => runtime.capability(PebbleClient(client)),
        Err(error) => {
            tracing::warn!(error = %error, "native Pebble client unavailable");
            runtime
        }
    };
    github::register(attractor(runtime))
        .capability(github::ActionSourceCap(trees))
        .capability(github::ActionManifestSourceCap(manifests))
        .capability(github::ToolCacheCap(tool_cache))
        .run_services(move |run_dir, caps| {
            let cache = github_objects::cache_dir(&store);
            match github_objects::ObjectService::start(run_dir.join("artifacts"), cache) {
                Ok(service) => {
                    // `ACTIONS_RUNTIME_TOKEN` is a bearer credential for a
                    // beyond-loopback listener, injected into every action's
                    // env; registering it here is what makes the deny-list
                    // claim in `.ai/decisions/observability-tracing.md`
                    // ("object service tokens" are never loggable in the
                    // clear) hold for the step logs and the event log too.
                    masker.register(service.token());
                    let cap = github::ResultsServiceCap {
                        port:  service.port(),
                        token: service.token().into(),
                    };
                    (
                        caps.provide(cap),
                        Some(Box::new(ObjectServiceGuard(service)) as _),
                    )
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "results service unavailable");
                    (caps, None)
                }
            }
        })
        .secrets(secrets)
}

/// The environment variable naming extra catalog layers for the model client:
/// one or more paths to `lithos-llm` catalog TOML files, separated by the
/// platform's path separator, layered over the built-in catalog in order. A
/// layer redirects a provider (`[providers.openai] base_url = "http://…"`),
/// adds models, or changes their metadata. A path that cannot be read or does
/// not parse is an error: the client is not built and every native agent node
/// fails with `pebble_unconfigured`, rather than reaching a live provider.
pub const LLM_CATALOG_ENV: &str = "PETRI_LLM_CATALOG";

/// The environment variable naming which providers the model client may
/// route to: provider ids separated by commas. Unset, every built-in provider
/// with credentials is available. Set, a model on any other provider is
/// unavailable, so a test that redirects `openai` and `anthropic` to loopback
/// cannot reach a third provider by accident.
pub const LLM_PROVIDERS_ENV: &str = "PETRI_LLM_PROVIDERS";

/// The environment variable naming how many times the client sends one
/// request on the same route before its error reaches the caller: the
/// provider-request retries `lithos-llm` owns (`RetryMiddleware`, exponential
/// backoff, `Retry-After` honored). Default [`DEFAULT_LLM_RETRY_ATTEMPTS`];
/// `1` sends each request once. Model fallback (`[run.model.fallbacks]`) is a
/// separate policy that starts only once these retries are spent.
pub const LLM_RETRY_ATTEMPTS_ENV: &str = "PETRI_LLM_RETRY_ATTEMPTS";

/// The client's default request retry budget: three sends of one request.
pub const DEFAULT_LLM_RETRY_ATTEMPTS: u32 = 3;

/// The environment variable naming the client's per-call budget in
/// milliseconds, retries and stream consumption included. Unset means no
/// budget; a request's own timeout overrides it. A call that exceeds it fails
/// with the `timeout` kind, which model fallback treats as eligible.
pub const LLM_TIMEOUT_ENV: &str = "PETRI_LLM_TIMEOUT_MS";

/// Why the model client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum LlmClientError {
    #[error("could not read the {LLM_CATALOG_ENV} layer `{path}`")]
    ReadLayer {
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the model catalog is invalid")]
    Catalog(#[source] CatalogError),
    #[error("the model client could not be built")]
    Client(#[source] ClientBuildError),
}

/// The model client native Pebble sessions use, configured the way this
/// distribution documents.
///
/// Credentials come from `lithos-llm`'s conventional environment variables
/// (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, and so on), read per request, never
/// at build time. Endpoints come from the built-in catalog, with
/// [`LLM_CATALOG_ENV`] layers on top, and [`LLM_PROVIDERS_ENV`] narrows which
/// providers are available at all. Both are read from this process's
/// environment only; a harness sets them on the child it launches and leaves
/// the developer's shell alone.
///
/// [`LLM_RETRY_ATTEMPTS_ENV`] sets the client's same-route retries and
/// [`LLM_TIMEOUT_ENV`] its per-call budget. The client carries Pebble's
/// `RetryEventObserver`, so each of those retries reaches the event stream of
/// the session whose call it was, as an `LlmRetry` event with `phase = open`.
pub fn llm_client() -> Result<lithos_llm::Client, LlmClientError> {
    build_llm_client(&LlmClientConfig::from_env()?)
}

/// What [`llm_client`] reads from the environment, as a value a host or a
/// test can build directly.
#[derive(Clone, Default)]
pub struct LlmClientConfig {
    /// Where credentials come from; `None` reads `lithos-llm`'s conventional
    /// environment variables per request.
    pub credentials:    Option<Arc<dyn CredentialProvider>>,
    /// Catalog TOML layers over the built-in catalog: a label and the text.
    pub layers:         Vec<(String, String)>,
    /// The providers that may be routed to; `None` leaves every provider.
    pub providers:      Option<Vec<String>>,
    /// The client's same-route retry budget, sends of one request.
    pub retry_attempts: u32,
    /// The per-call budget, retries included.
    pub timeout:        Option<Duration>,
}

impl LlmClientConfig {
    /// Read [`LLM_CATALOG_ENV`], [`LLM_PROVIDERS_ENV`],
    /// [`LLM_RETRY_ATTEMPTS_ENV`] and [`LLM_TIMEOUT_ENV`].
    pub fn from_env() -> Result<Self, LlmClientError> {
        let mut layers = Vec::new();
        if let Some(paths) = env::var_os(LLM_CATALOG_ENV) {
            for path in env::split_paths(&paths).filter(|p| !p.as_os_str().is_empty()) {
                let text =
                    fs::read_to_string(&path).map_err(|source| LlmClientError::ReadLayer {
                        path: path.clone(),
                        source,
                    })?;
                layers.push((path.display().to_string(), text));
            }
        }
        let providers = env::var(LLM_PROVIDERS_ENV).ok().map(|providers| {
            providers
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        });
        let retry_attempts = env::var(LLM_RETRY_ATTEMPTS_ENV)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_LLM_RETRY_ATTEMPTS);
        let timeout = env::var(LLM_TIMEOUT_ENV)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
        Ok(Self {
            credentials: None,
            layers,
            providers,
            retry_attempts,
            timeout,
        })
    }
}

/// Build the model client from an explicit configuration; [`llm_client`]
/// with the environment read for you.
pub fn build_llm_client(config: &LlmClientConfig) -> Result<lithos_llm::Client, LlmClientError> {
    let mut catalog = Catalog::builder().with_builtin();
    for (label, text) in &config.layers {
        catalog = catalog
            .toml_layer(label.clone(), text)
            .map_err(LlmClientError::Catalog)?;
    }
    let catalog = catalog.build().map_err(LlmClientError::Catalog)?;
    let mut builder = lithos_llm::Client::builder().catalog(catalog);
    builder = match &config.credentials {
        Some(credentials) => builder.credentials_arc(credentials.clone()),
        None => builder.credentials(ConventionalCredentials::new()),
    };
    // The client's own retries, on the same route. Pebble's turn replay and
    // Fabro's model fallback are configured elsewhere and start after these.
    // The observer puts each retry on the event stream of the session whose
    // call it was, as `llm_retry`; a call no session made is ignored.
    let attempts = config.retry_attempts.max(1);
    if attempts > 1 {
        builder = builder.middleware(
            RetryMiddleware::new(RetryPolicy::exponential().max_attempts(attempts))
                .observer(RetryEventObserver),
        );
    }
    if let Some(budget) = config.timeout {
        builder = builder.default_timeout(budget);
    }
    if let Some(providers) = &config.providers {
        builder = builder.enabled_providers(providers.iter().cloned());
    }
    let build = builder.build().map_err(LlmClientError::Client)?;
    for issue in &build.issues {
        tracing::debug!(provider = %issue.provider, cause = %issue.cause, "provider unavailable");
    }
    Ok(build.client)
}

/// The ObjectService as the driver's run guard: teardown is the service's
/// explicit async shutdown, so joining its thread never blocks a Tokio worker.
struct ObjectServiceGuard(github_objects::ObjectService);

#[async_trait::async_trait]
impl driver::RunGuard for ObjectServiceGuard {
    async fn teardown(self: Box<Self>) {
        self.0.shutdown().await;
    }
}

/// A runtime-registerable secret map plus a lazily loaded `GITHUB_TOKEN`.
struct GithubSecrets {
    registered: executor::MapSecrets,
    token:      OnceLock<Option<executor::Secret>>,
}

impl GithubSecrets {
    fn new() -> Self {
        Self {
            registered: executor::MapSecrets::empty(),
            token:      OnceLock::new(),
        }
    }

    /// The machine's token, or — with none configured — the empty string, said
    /// once. `github.token` always exists on GitHub; locally, empty is the
    /// honest analog the toolkit is built for: actions send no auth header and
    /// their API calls go anonymous (public reads work, rate-limited; writes
    /// fail with the API's own error). A missing-secret failure would instead
    /// stop every action that merely *names* the token, setup-* included.
    fn load_token() -> Option<executor::Secret> {
        let token = env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .map(|t| executor::Secret::new(t.into()))
            .or_else(gh_auth_token);
        if token.is_none() {
            tracing::warn!(
                "no GITHUB_TOKEN (set it, or log in with `gh`); actions that use `github.token` \
                 run anonymously"
            );
        }
        token
    }
}

/// The environment variable prefix a workflow secret (`{{ secrets.NAME }}` in
/// `workflow.toml`) is read from in the standalone runner: `PETRI_SECRET_NAME`.
/// Fabro reads the same name from its vault; the standalone runner has no
/// vault, and a variable named after the secret is the explicit source. The
/// value is masked in every log from the moment it is resolved.
pub const SECRET_ENV_PREFIX: &str = "PETRI_SECRET_";

impl executor::SecretProvider for GithubSecrets {
    fn resolve(&self, name: &str) -> Result<executor::Secret, executor::SecretError> {
        match self.registered.resolve(name) {
            Ok(secret) => return Ok(secret),
            Err(executor::SecretError::Unknown(_)) => {}
            Err(error) => return Err(error),
        }
        if name != GITHUB_TOKEN_SECRET {
            let variable = format!("{SECRET_ENV_PREFIX}{name}");
            let value = env::var(&variable)
                .ok()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| executor::SecretError::Unknown(name.into()))?;
            self.registered.masker().register(&value);
            return Ok(executor::Secret::new(value.into()));
        }
        let value = self
            .token
            .get_or_init(Self::load_token)
            .clone()
            .map_or_else(Default::default, executor::Secret::expose);
        // An empty value never enters the mask set (the masker refuses short
        // values), so this is a no-op for the token-less case by construction.
        self.registered.masker().register(&value);
        Ok(executor::Secret::new(value))
    }

    fn register(&self, name: &str, value: &str) -> Result<(), executor::SecretError> {
        self.registered.register(name, value)
    }

    fn masker(&self) -> executor::Masker {
        self.registered.masker()
    }
}

/// Ask `gh` for a token without letting a broken credential helper block
/// forever.
fn gh_auth_token() -> Option<executor::Secret> {
    let mut child = Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                return output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
                    .filter(|token| !token.is_empty())
                    .map(|token| executor::Secret::new(token.into()));
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The text of Fabro's user settings layer: `$FABRO_HOME/settings.toml`,
/// else `$HOME/.fabro/settings.toml`, when the file exists. Fabro's own
/// rule for its home directory.
fn fabro_settings_toml() -> Option<String> {
    let home = env::var_os("FABRO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".fabro")))?;
    fs::read_to_string(home.join("settings.toml")).ok()
}

#[cfg(doctest)]
#[doc = include_str!("../../../../README.md")]
pub struct ReadmeDoctests;
