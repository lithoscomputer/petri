//! Pebble tools act through the firing's execution scope, never host paths.
//! The same scope is the route to a sandbox-hosted MCP server's port
//! ([`ScopePortRoutes`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use executor::{
    EnvError, ExecEnv, OutputMode, ProcessHandle, ProcessSpec, Sig, SpawnEnv, SpawnTarget,
};
use globset::{GlobBuilder, GlobMatcher};
use ir::LogStream;
use pebble_coding_agent::environment::support::{
    ExecFailure, OutputCaptureBuffer, classify_exec_error, tree_order, validate_glob,
};
use pebble_coding_agent::environment::{
    DirEntry, EnvResult, Environment, EnvironmentError, EnvironmentErrorKind, ExecOutcome,
    ExecOutputStream, ExecRequest, ExecResult, GrepOptions,
};
use pebble_coding_agent::events::CommandTermination;
use pebble_coding_agent::mcp::{PortRoute, PortRouteError, PortRoutes};
use pebble_coding_agent::tools::OutputCaptureStats;
use smol_str::SmolStr;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

/// What a tool shell's environment starts from beneath the variables its tool
/// call sets: the workflow's secrets, resolved when the session opened.
pub struct InheritedEnv(BTreeMap<SmolStr, SmolStr>);

impl InheritedEnv {
    pub fn new(env: BTreeMap<SmolStr, SmolStr>) -> Self {
        Self(env)
    }
}

#[async_trait]
impl SpawnEnv for InheritedEnv {
    async fn apply(
        &self,
        _target: SpawnTarget,
        env: &mut BTreeMap<SmolStr, SmolStr>,
    ) -> Result<(), EnvError> {
        for (key, value) in &self.0 {
            env.entry(key.clone()).or_insert_with(|| value.clone());
        }
        Ok(())
    }
}

/// Adapts the execution scope supplied by Petri to Pebble's coding tools.
/// Bash, find, and grep must be available inside the scope. Uses ripgrep
/// when present.
pub struct PebbleEnvironment {
    env:        Arc<dyn ExecEnv>,
    cancel:     CancellationToken,
    kill:       CancellationToken,
    platform:   String,
    os_version: String,
    ripgrep:    bool,
}

impl PebbleEnvironment {
    /// Probes the scope, including remote scopes, before building a session.
    pub async fn prepare(
        env: Arc<dyn ExecEnv>,
        cancel: CancellationToken,
        kill: CancellationToken,
    ) -> EnvResult<Self> {
        let mut adapter = Self {
            env,
            cancel,
            kill,
            platform: "unknown".into(),
            os_version: "unknown".into(),
            ripgrep: false,
        };
        let result = adapter
            .command(
                "command -v bash >/dev/null && command -v find >/dev/null && command -v grep >/dev/null || { echo 'Pebble requires Bash, find, and grep in the execution scope' >&2; exit 127; }; uname -s && uname -r; if command -v rg >/dev/null; then printf rg; fi",
            )
            .await?;
        let mut lines = result.lines();
        adapter.platform = match lines.next() {
            Some("Darwin") => "darwin",
            Some("Linux") => "linux",
            _ => "unknown",
        }
        .into();
        adapter.os_version = format!("{} {}", adapter.platform, lines.next().unwrap_or("unknown"));
        adapter.ripgrep = lines.next() == Some("rg");
        Ok(adapter)
    }

    /// An adapter for file reads alone, with no probe: what a prompt node
    /// loads its project memory through. `platform` says unknown, and nothing
    /// that runs a command should be asked of it.
    pub fn for_files(env: Arc<dyn ExecEnv>) -> Self {
        Self {
            env,
            cancel: CancellationToken::new(),
            kill: CancellationToken::new(),
            platform: "unknown".into(),
            os_version: "unknown".into(),
            ripgrep: false,
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        // Path joining is lexical. It does not inspect Petri's host filesystem.
        Path::new(self.env.workspace_path()).join(path)
    }

    async fn command(&self, command: &str) -> EnvResult<String> {
        let outcome = self
            .exec(ExecRequest {
                timeout_ms: Some(30_000),
                output_bytes_cap: Some(4 * 1024 * 1024),
                ..ExecRequest::new(command)
            })
            .await?;
        if !outcome.result.is_success() {
            return Err(error(
                EnvironmentErrorKind::Io,
                format!("Scope command failed: {}", outcome.result.stderr.trim()),
            ));
        }
        if outcome.output_capture().omitted_bytes != 0 {
            return Err(error(
                EnvironmentErrorKind::Io,
                "Scope command output exceeded 4 MiB",
            ));
        }
        Ok(outcome.result.stdout)
    }
}

#[async_trait]
impl Environment for PebbleEnvironment {
    fn working_directory(&self) -> &str {
        self.env.workspace_path()
    }
    fn platform(&self) -> &str {
        &self.platform
    }
    fn os_version(&self) -> String {
        self.os_version.clone()
    }

    async fn read_file_bytes(&self, path: &str) -> EnvResult<Vec<u8>> {
        self.env
            .read_file(&self.path(path))
            .await
            .map_err(io_error)?
            .ok_or_else(|| {
                error(
                    EnvironmentErrorKind::NotFound,
                    format!("File not found: {path}"),
                )
            })
    }

    async fn write_file(&self, path: &str, content: &str) -> EnvResult<()> {
        self.env
            .write_file(&self.path(path), content.as_bytes())
            .await
            .map_err(io_error)
    }

    async fn rename_file(&self, source: &str, destination: &str) -> EnvResult<()> {
        let source = quote(&self.path(source).to_string_lossy());
        let destination_path = self.path(destination);
        let destination = quote(&destination_path.to_string_lossy());
        let parent = quote(
            &destination_path
                .parent()
                .unwrap_or(Path::new("/"))
                .to_string_lossy(),
        );
        self.command(&format!("if [[ {source} -ef {destination} ]]; then exit 0; fi\nif [[ -d {source} || -d {destination} ]]; then echo 'rename_file requires file paths' >&2; exit 1; fi\nmkdir -p -- {parent} && mv -f -- {source} {destination}")).await?;
        Ok(())
    }

    async fn delete_file(&self, path: &str) -> EnvResult<()> {
        self.command(&format!(
            "rm -- {}",
            quote(&self.path(path).to_string_lossy())
        ))
        .await?;
        Ok(())
    }

    async fn file_exists(&self, path: &str) -> EnvResult<bool> {
        Ok(self
            .command(&format!(
                "if [[ -e {} ]]; then printf yes; else printf no; fi",
                quote(&self.path(path).to_string_lossy())
            ))
            .await?
            == "yes")
    }

    async fn list_directory(&self, path: &str, depth: Option<usize>) -> EnvResult<Vec<DirEntry>> {
        let mut entries: Vec<DirEntry> = self
            .env
            .list_directory(&self.path(path), depth.unwrap_or(1))
            .await
            .map_err(io_error)?
            .into_iter()
            .map(|entry| DirEntry {
                name:   entry.path,
                is_dir: entry.is_dir,
                size:   entry.size,
            })
            .collect();
        tree_order(&mut entries);
        Ok(entries)
    }

    async fn grep(
        &self,
        pattern: &str,
        path: &str,
        options: &GrepOptions,
    ) -> EnvResult<Vec<String>> {
        if options.max_results == Some(0) {
            return Ok(Vec::new());
        }
        let mut command = if self.ripgrep {
            "rg --no-config --no-ignore-parent --line-number --with-filename --color never"
        } else {
            "grep -rnHI"
        }
        .to_owned();
        if options.case_insensitive {
            command.push_str(" -i");
        }
        if let Some(glob) = &options.glob_filter {
            let option = if self.ripgrep { "--glob" } else { "--include" };
            let _ = write!(command, " {option} {}", quote(glob));
        }
        if let Some(limit) = options.max_results {
            let _ = write!(command, " -m {limit}");
        }
        let _ = write!(
            command,
            " -- {} {}\nstatus=$?; if [[ $status == 1 ]]; then exit 0; else exit \"$status\"; fi",
            quote(pattern),
            quote(&self.path(path).to_string_lossy())
        );
        let text = self.command(&command).await?;
        Ok(text
            .lines()
            .take(options.max_results.unwrap_or(usize::MAX))
            .map(str::to_owned)
            .collect())
    }

    async fn glob(&self, pattern: &str, path: Option<&str>) -> EnvResult<Vec<String>> {
        let matcher = compile_glob(pattern)?;
        let base = self.path(path.unwrap_or("."));
        let base_argument = quote(&base.to_string_lossy());
        let paths = self.command(&format!("if [[ ! -e {base_argument} ]]; then exit 0; fi; find {base_argument} -type f -print0")).await?;
        let mut matches = paths
            .split('\0')
            .filter(|path| !path.is_empty())
            .filter(|path| {
                Path::new(path)
                    .strip_prefix(&base)
                    .is_ok_and(|relative| matcher.is_match(relative))
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        matches.sort();
        Ok(matches)
    }

    async fn exec(&self, request: ExecRequest<'_>) -> EnvResult<ExecOutcome> {
        let started = Instant::now();
        let cancel = request.cancel_token.clone().unwrap_or_default();
        let spec = ProcessSpec::new("bash", &["-c", request.command])
            .with_output(OutputMode::Bytes)
            .with_cwd(request.working_dir.map(|path| self.path(path)))
            .with_env(
                request
                    .env_vars
                    .into_iter()
                    .flat_map(|vars| vars.iter())
                    .map(|(k, v)| (k.as_str().into(), v.as_str().into()))
                    .collect(),
            );
        if cancel.is_cancelled() || self.cancel.is_cancelled() || self.kill.is_cancelled() {
            return Ok(empty_stopped(CommandTermination::Cancelled, started));
        }
        let deadline = sleep(Duration::from_millis(
            request.timeout_ms.unwrap_or(u64::MAX),
        ));
        tokio::pin!(deadline);
        let mut process = tokio::select! {
            biased;
            () = self.kill.cancelled() => return Ok(empty_stopped(CommandTermination::Cancelled, started)),
            () = self.cancel.cancelled() => return Ok(empty_stopped(CommandTermination::Cancelled, started)),
            () = cancel.cancelled() => return Ok(empty_stopped(CommandTermination::Cancelled, started)),
            () = &mut deadline, if request.timeout_ms.is_some() => return Ok(empty_stopped(CommandTermination::TimedOut, started)),
            process = self.env.spawn(spec) => process.map_err(|e| error(classify_exec_error(ExecFailure::Start), e.to_string()))?,
        };
        let Some(mut bytes) = process.bytes() else {
            let _ = process.signal(Sig::Kill).await;
            let _ = process.wait().await;
            return Err(error(
                classify_exec_error(ExecFailure::Unsupported),
                "Executor does not provide raw process output",
            ));
        };
        let mut drains = JoinSet::new();
        let cap = request.output_bytes_cap;
        let sink = request.output_sink.clone();
        drains.spawn(async move {
            let mut stdout = OutputCaptureBuffer::new(cap);
            let mut stderr = OutputCaptureBuffer::new(cap);
            while let Some(chunk) = bytes.recv().await {
                // Every byte reaches the sink as it is read, uncapped; the
                // buffers keep what the outcome retains.
                match chunk.stream {
                    LogStream::Stdout => {
                        if let Some(sink) = &sink {
                            sink(ExecOutputStream::Stdout, &chunk.bytes);
                        }
                        stdout.push(&chunk.bytes);
                    }
                    LogStream::Stderr => {
                        if let Some(sink) = &sink {
                            sink(ExecOutputStream::Stderr, &chunk.bytes);
                        }
                        stderr.push(&chunk.bytes);
                    }
                }
            }
            (stdout, stderr)
        });
        let (mut termination, mut status) = tokio::select! {
            biased;
            () = self.kill.cancelled() => (CommandTermination::Cancelled, None),
            () = self.cancel.cancelled() => (CommandTermination::Cancelled, None),
            () = cancel.cancelled() => (CommandTermination::Cancelled, None),
            () = &mut deadline, if request.timeout_ms.is_some() => (CommandTermination::TimedOut, None),
            status = process.wait() => (CommandTermination::Exited, Some(status.map_err(io_error)?)),
        };
        if status.is_none() {
            stop(process.as_mut(), self.env.grace(), &self.kill).await?;
        }
        let capture = if status.is_some() {
            tokio::select! {
                result = drains.join_next() => result,
                () = async {
                    tokio::select! {
                        () = self.kill.cancelled() => {},
                        () = self.cancel.cancelled() => {},
                        () = cancel.cancelled() => {},
                        () = &mut deadline, if request.timeout_ms.is_some() => { termination = CommandTermination::TimedOut; },
                    }
                } => {
                    if termination != CommandTermination::TimedOut { termination = CommandTermination::Cancelled; }
                    status = None;
                    stop(process.as_mut(), self.env.grace(), &self.kill).await?;
                    drains.join_next().await
                }
            }
        } else {
            drains.join_next().await
        };
        let (stdout, stderr) = capture
            .ok_or_else(|| {
                error(
                    classify_exec_error(ExecFailure::Collect),
                    "Output capture task missing",
                )
            })?
            .map_err(|cause| {
                error(
                    classify_exec_error(ExecFailure::Collect),
                    format!("Output capture task failed: {cause}"),
                )
            })?;
        let (stdout, stdout_capture) = stdout.into_text();
        let (stderr, stderr_capture) = stderr.into_text();
        Ok(ExecOutcome {
            result: ExecResult {
                stdout,
                stderr,
                exit_code: status.and_then(|status| status.code),
                termination,
                duration_ms: elapsed_ms(started.elapsed()),
            },
            streams_separated: true,
            stdout_capture,
            stderr_capture,
        })
    }
}

async fn stop(
    process: &mut dyn ProcessHandle,
    grace: Duration,
    kill: &CancellationToken,
) -> EnvResult<()> {
    if !kill.is_cancelled() {
        process.signal(Sig::Term).await.map_err(io_error)?;
        tokio::select! {
            result = process.wait() => return result.map(|_| ()).map_err(io_error),
            () = sleep(grace) => {},
            () = kill.cancelled() => {},
        }
    }
    process.signal(Sig::Kill).await.map_err(io_error)?;
    timeout(Duration::from_secs(5), process.wait())
        .await
        .map_err(|_| {
            error(
                EnvironmentErrorKind::Io,
                "Process did not stop after SIGKILL",
            )
        })?
        .map(|_| ())
        .map_err(io_error)
}

fn empty_stopped(termination: CommandTermination, started: Instant) -> ExecOutcome {
    ExecOutcome {
        result:            ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            termination,
            duration_ms: elapsed_ms(started.elapsed()),
        },
        streams_separated: true,
        stdout_capture:    OutputCaptureStats::complete(0),
        stderr_capture:    OutputCaptureStats::complete(0),
    }
}

/// Pebble's glob grammar, then a matcher for it. Pebble states the rules
/// once for every environment; the matcher is this one's.
fn compile_glob(pattern: &str) -> EnvResult<GlobMatcher> {
    validate_glob(pattern)?;
    GlobBuilder::new(pattern.trim_start_matches("./"))
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|cause| {
            error(
                EnvironmentErrorKind::InvalidInput,
                format!("Invalid glob {pattern:?}: {cause}"),
            )
        })
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}
fn error(kind: EnvironmentErrorKind, message: impl Into<String>) -> EnvironmentError {
    EnvironmentError::new(kind, message)
}
fn io_error(cause: executor::EnvError) -> EnvironmentError {
    EnvironmentError::with_source(EnvironmentErrorKind::Io, cause.to_string(), cause)
}
pub(crate) fn elapsed_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The route from Petri to a port inside the scope, as Pebble reaches a
/// sandbox-hosted MCP server: Pebble's own [`PortRoutes`] contract over
/// [`ExecEnv::preview_url`]. The provider answers (the host's own loopback,
/// the forward the Docker plugin opens into the container, Daytona's preview
/// link with its token header); an environment with no route to its ports
/// reports [`PortRouteError::Unsupported`], which Pebble makes the server's
/// failure reason. The trait is Pebble's, so Petri shares no sandbox crate
/// with Pebble to implement it.
pub struct ScopePortRoutes(Arc<dyn ExecEnv>);

impl ScopePortRoutes {
    pub fn new(env: Arc<dyn ExecEnv>) -> Self {
        Self(env)
    }
}

#[async_trait]
impl PortRoutes for ScopePortRoutes {
    async fn route(&self, port: u16) -> Result<PortRoute, PortRouteError> {
        match self.0.preview_url(port).await {
            Ok(Some(route)) => Ok(PortRoute {
                url:     route.url,
                headers: route.headers,
            }),
            Ok(None) => Err(PortRouteError::Unsupported),
            Err(error) => Err(route_error("opening the route", error)),
        }
    }

    async fn release(&self, port: u16) -> Result<(), PortRouteError> {
        self.0
            .release_preview_url(port)
            .await
            .map_err(|error| route_error("releasing the route", error))
    }
}

/// The environment's refusal as Pebble's route failure: `step` names what
/// was being done and the refusal is the source, so the chain Pebble
/// reports as the reason the server has no route reads `<step>: <refusal>`,
/// the refusal's own text once.
fn route_error(step: &'static str, error: EnvError) -> PortRouteError {
    PortRouteError::failed_with_source(step, error)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error as _;

    use executor::PreviewUrl;

    use super::*;

    /// What the environment says when it refuses a route.
    const REFUSAL: &str = "the forward could not be opened";

    /// An environment whose only answers are about its ports: it runs
    /// nothing and holds no files.
    enum Ports {
        /// Routes every port to a loopback URL with one header.
        Routed,
        /// Offers no route to its ports.
        Unrouted,
        /// Refuses to open or release a route.
        Refusing,
    }

    #[async_trait]
    impl ExecEnv for Ports {
        async fn spawn(&self, _spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
            Err(EnvError::backend("test", "spawn", "runs nothing"))
        }

        fn workspace_path(&self) -> &'static str {
            "/work"
        }

        async fn read_file(&self, _relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
            Ok(None)
        }

        async fn write_file(&self, _relative: &Path, _contents: &[u8]) -> Result<(), EnvError> {
            Ok(())
        }

        fn grace(&self) -> Duration {
            Duration::from_millis(10)
        }

        async fn preview_url(&self, port: u16) -> Result<Option<PreviewUrl>, EnvError> {
            match self {
                Self::Routed => Ok(Some(PreviewUrl {
                    url:     format!("http://127.0.0.1:{port}"),
                    headers: BTreeMap::from([("X-Token".to_owned(), "t".to_owned())]),
                })),
                Self::Unrouted => Ok(None),
                Self::Refusing => Err(EnvError::backend("test", "preview_url", REFUSAL)),
            }
        }

        async fn release_preview_url(&self, _port: u16) -> Result<(), EnvError> {
            match self {
                Self::Routed | Self::Unrouted => Ok(()),
                Self::Refusing => Err(EnvError::backend("test", "release_preview_url", REFUSAL)),
            }
        }
    }

    fn routes(env: Ports) -> ScopePortRoutes {
        ScopePortRoutes::new(Arc::new(env))
    }

    #[tokio::test]
    async fn a_routed_port_is_the_environments_url_and_headers() {
        let route = routes(Ports::Routed).route(8080).await.expect("routed");
        assert_eq!(route.url, "http://127.0.0.1:8080");
        assert_eq!(
            route.headers,
            BTreeMap::from([("X-Token".to_owned(), "t".to_owned())])
        );
    }

    #[tokio::test]
    async fn an_environment_without_a_route_reports_unsupported() {
        let error = routes(Ports::Unrouted)
            .route(8080)
            .await
            .expect_err("no route");
        assert!(matches!(error, PortRouteError::Unsupported), "{error:?}");
        assert_eq!(
            error.detail(),
            "the environment does not route to its ports"
        );
    }

    #[tokio::test]
    async fn a_refused_route_fails_with_the_refusal_as_its_cause() {
        let error = routes(Ports::Refusing)
            .route(8080)
            .await
            .expect_err("refused");
        assert!(matches!(error, PortRouteError::Failed { .. }), "{error:?}");
        assert_eq!(error.to_string(), "opening the route");
        let cause = error.source().expect("the refusal is the cause");
        assert_eq!(
            cause.to_string(),
            format!("test preview_url failed: {REFUSAL}")
        );
        // The one-line reason Pebble reports names the step, then the
        // refusal once.
        let detail = error.detail();
        assert_eq!(
            detail,
            format!("opening the route: test preview_url failed: {REFUSAL}")
        );
        assert_eq!(detail.matches(REFUSAL).count(), 1);
    }

    #[tokio::test]
    async fn a_release_succeeds_or_fails_as_the_environment_says() {
        routes(Ports::Routed).release(8080).await.expect("released");
        routes(Ports::Unrouted)
            .release(8080)
            .await
            .expect("nothing to release");
        let error = routes(Ports::Refusing)
            .release(8080)
            .await
            .expect_err("refused");
        assert!(matches!(error, PortRouteError::Failed { .. }), "{error:?}");
        let detail = error.detail();
        assert_eq!(
            detail,
            format!("releasing the route: test release_preview_url failed: {REFUSAL}")
        );
        assert_eq!(detail.matches(REFUSAL).count(), 1);
    }
}
