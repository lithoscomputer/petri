//! [`ExecEnv`] and [`ContainerRunner`] over one [`Sandbox`].
//!
//! A step spawn becomes one contract `exec` call in an owned task; a
//! Docker action becomes one `Containers::run` call the same way. Output
//! chunks are fed through the interface crate's own line pump (a duplex
//! pipe per stream), so the 64 KiB line cap and truncation marker match
//! every other executor exactly. The step's cancellation ladder is the only
//! one: `SIGTERM` fires the sandbox-driver `term` token, `SIGKILL` fires
//! `kill`, and the provider sends exactly that signal and nothing more.
//!
//! Workspace files go through the sandbox's `Files` facet. The
//! workspace lives inside the sandbox — a volume it owns — and nothing on
//! Petri's machine mirrors it, so this is the only path to it, on a local
//! daemon and a remote one alike.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::iter;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use executor::lines::{LINE_CHANNEL_CAPACITY, pump};
use executor::{
    ByteStream, ContainerImage, ContainerRunner, DirectoryEntry, EnvError, ExecEnv, ExitStatus,
    LineStream, OneShotContainer, OutputChunk, OutputMode, PreviewUrl, ProcessHandle, ProcessSpec,
    Sig, StdinMode, StdinWriter,
};
use sandbox_driver::{
    ByteStream as ContractByteStream, Error as DriverError, ExecControls, ExecOutcome, ExecSpec,
    Expose, FileRange, OneShotImage, OneShotSpec, OutputLoss, OutputStream, Sandbox, Termination,
};
use smol_str::SmolStr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::BACKEND;
use crate::gate::{Admission, RunGate};

/// Bytes of pipe buffer between the output sink and each line pump.
const OUTPUT_PIPE_CAPACITY: usize = 64 * 1024;
/// How long a mkdir for a step's cwd may take.
const MKDIR_TIMEOUT: Duration = Duration::from_secs(30);

/// One live sandbox, handed to step kinds as their spawn capability.
pub(crate) struct SandboxEnv {
    pub(crate) host:         bool,
    pub(crate) sandbox:      Arc<dyn Sandbox>,
    /// The workspace as the sandbox sees it: its working directory.
    pub(crate) workspace:    String,
    /// The effective environment, read once at acquire.
    pub(crate) ambient:      BTreeMap<String, String>,
    /// This holder's scope environment and container options, over the
    /// sandbox's original environment and under the process's own.
    pub(crate) env:          BTreeMap<SmolStr, SmolStr>,
    pub(crate) grace:        Duration,
    /// How a process in the sandbox reaches services on Petri's machine.
    pub(crate) host_address: Option<String>,
    /// The run's admission: every operation passes it, and none starts
    /// after the run closes it.
    pub(crate) gate:         RunGate,
}

/// Maps a finished exec to the executor's exit status. The observed signal
/// wins over a code, so a foreign signal and the step's own ladder read the
/// same on every backend. The fallbacks below are for a provider that
/// stopped the command but could not observe how (Daytona ends a session
/// without seeing the child's status). A command whose outcome the provider
/// lost has no status to report.
fn exit_status(outcome: &ExecOutcome) -> Result<ExitStatus, String> {
    let (termination, code, signal) = (outcome.termination, outcome.exit_code, outcome.signal);
    // The provider's own deadline ended the command: the step reads it as a
    // timeout, whatever signal the provider observed on the way.
    if termination == Termination::TimedOut {
        return Ok(ExitStatus::timed_out(
            signal.unwrap_or_else(|| Sig::Kill.number()),
        ));
    }
    if termination == Termination::Lost {
        return Err("the sandbox lost the command; its outcome is unknown".to_owned());
    }
    if let Some(signal) = signal {
        return Ok(ExitStatus::signalled(signal));
    }
    Ok(match termination {
        Termination::Killed => ExitStatus::signalled(Sig::Kill.number()),
        // The step's ladder decides cancel vs timeout; the handle only needs a
        // plausible signalled status for a stop it did not exit from.
        Termination::Terminated => ExitStatus::signalled(Sig::Term.number()),
        Termination::Exited | Termination::TimedOut | Termination::Lost => ExitStatus {
            code,
            signal: None,
            timed_out: false,
        },
    })
}

/// The line appended to a command's stderr when the provider dropped
/// output on its own transport (Daytona's encoded exec resyncs past a torn
/// record and counts it), so the agent reading the output and the run log
/// both learn that some is missing. `None` when nothing was lost.
fn output_loss_line(loss: OutputLoss) -> Option<String> {
    loss.is_lossy().then(|| {
        format!(
            "[sandbox] {} output frame(s), {} bytes dropped by the provider",
            loss.dropped_frames, loss.dropped_bytes
        )
    })
}

/// Logs a counted loss and appends its line to the stderr pipe, after the
/// command's own output, which the sink has fully written by the time the
/// run resolves.
async fn report_output_loss(loss: OutputLoss, stderr: &Mutex<Option<DuplexStream>>) {
    let Some(line) = output_loss_line(loss) else {
        return;
    };
    tracing::warn!(
        dropped_frames = loss.dropped_frames,
        dropped_bytes = loss.dropped_bytes,
        "sandbox provider dropped command output"
    );
    let mut guard = stderr.lock().await;
    if let Some(writer) = guard.as_mut() {
        let _ = writer.write_all(line.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
    }
}

/// What one streamed job runs: a step's exec, or an action's container.
enum Job {
    Exec(ExecSpec),
    OneShot(OneShotSpec),
}

/// Runs `job` in an owned task with the interface's line pumps on its
/// output and the step's stop tokens on its controls. The task holds
/// `admission` until the job settles, so the run's gate waits for it.
fn spawn_streamed(
    sandbox: Arc<dyn Sandbox>,
    job: Job,
    stdin: bool,
    output: OutputMode,
    admission: Admission,
) -> SandboxProcess {
    // Stdin: a piped step gets a writer whose read half streams into the
    // command for its whole life.
    let (stdin_writer, stdin_source) = if stdin {
        let (writer, reader) = duplex(OUTPUT_PIPE_CAPACITY);
        let source: ContractByteStream = Box::pin(reader);
        (Some(Box::new(writer) as StdinWriter), Some(source))
    } else {
        (None, None)
    };

    // Output: one duplex per stream feeds the interface line pump, so the
    // line cap and truncation marker are the shared ones.
    let (line_tx, line_rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
    let (stdout_writer, stdout_reader) = duplex(OUTPUT_PIPE_CAPACITY);
    let (stderr_writer, stderr_reader) = duplex(OUTPUT_PIPE_CAPACITY);
    let mut workers = JoinSet::new();
    let (byte_tx, byte_rx) = mpsc::channel(16);
    let (lines, bytes) = match output {
        OutputMode::Lines => {
            workers.spawn(pump(stdout_reader, ir::LogStream::Stdout, line_tx.clone()));
            workers.spawn(pump(stderr_reader, ir::LogStream::Stderr, line_tx));
            (Some(line_rx), None)
        }
        OutputMode::Bytes => {
            workers.spawn(pump_bytes(
                stdout_reader,
                ir::LogStream::Stdout,
                byte_tx.clone(),
            ));
            workers.spawn(pump_bytes(stderr_reader, ir::LogStream::Stderr, byte_tx));
            (None, Some(byte_rx))
        }
    };

    let term = CancellationToken::new();
    let kill = CancellationToken::new();
    let (status_tx, status_rx) = watch::channel(None);

    // The sink writes each chunk into the matching stream pipe; the pumps
    // read those pipes and split into lines. The writers are shared into
    // the closure so the task can shut them for EOF when the run ends.
    let stdout_slot = Arc::new(Mutex::new(Some(stdout_writer)));
    let stderr_slot = Arc::new(Mutex::new(Some(stderr_writer)));
    let sink_stdout = stdout_slot.clone();
    let sink_stderr = stderr_slot.clone();
    let controls = ExecControls {
        term:   Some(term.clone()),
        kill:   Some(kill.clone()),
        stdin:  stdin_source,
        resize: None,
        sink:   Some(Arc::new(move |stream, chunk| {
            let slot = match stream {
                OutputStream::Stdout | OutputStream::Terminal => sink_stdout.clone(),
                OutputStream::Stderr => sink_stderr.clone(),
            };
            Box::pin(async move {
                let mut guard = slot.lock().await;
                if let Some(writer) = guard.as_mut() {
                    let _ = writer.write_all(&chunk).await;
                }
                Ok(())
            })
        })),
    };

    workers.spawn(async move {
        let outcome: Result<ExecOutcome, DriverError> = match job {
            Job::Exec(spec) => sandbox.exec(spec, controls).await,
            Job::OneShot(spec) => match sandbox.containers() {
                Some(containers) => containers.run(spec, controls).await,
                None => Err(DriverError::Unsupported {
                    capability: "containers".to_owned(),
                }),
            },
        };
        // A loss the provider counted does not fail the command: it ran to
        // its status, and the loss reaches the step as one stderr line.
        if let Ok(outcome) = &outcome {
            report_output_loss(outcome.output_loss, &stderr_slot).await;
        }
        // Close the pipes so the pumps flush and end.
        if let Some(mut writer) = stdout_slot.lock().await.take() {
            let _ = writer.shutdown().await;
        }
        if let Some(mut writer) = stderr_slot.lock().await.take() {
            let _ = writer.shutdown().await;
        }
        let status: Result<ExitStatus, String> = match outcome {
            Ok(outcome) => exit_status(&outcome),
            Err(error) => Err(error.to_string()),
        };
        let _ = status_tx.send(Some(status));
        drop(admission);
    });

    SandboxProcess {
        lines,
        bytes,
        stdin: stdin_writer,
        term,
        kill,
        status: status_rx,
        cached: None,
        workers,
    }
}

/// The duplex reader cannot fail independently of its writer. A closed
/// receiver ends the pump; dropping the process owns remote cancellation.
async fn pump_bytes(
    mut reader: impl AsyncRead + Unpin,
    stream: ir::LogStream,
    sender: mpsc::Sender<OutputChunk>,
) {
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let Ok(length) = reader.read(&mut buffer).await else {
            return;
        };
        if length == 0 {
            return;
        }
        if sender
            .send(OutputChunk {
                stream,
                bytes: buffer[..length].to_vec(),
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Maps a facet failure to the executor's error.
fn facet_error(operation: &str, error: &DriverError) -> EnvError {
    EnvError::backend(BACKEND, operation, error.to_string())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The scope's env, with the spec's own on top.
fn merged_env(
    scope: &BTreeMap<SmolStr, SmolStr>,
    own: &BTreeMap<SmolStr, SmolStr>,
) -> BTreeMap<String, String> {
    scope
        .iter()
        .chain(own.iter())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Reads a workspace file, or `None` when the sandbox has no such file.
async fn read(
    sandbox: &dyn Sandbox,
    relative: &Path,
    range: Option<FileRange>,
) -> Result<Option<Vec<u8>>, EnvError> {
    let mut stream = match sandbox
        .files()
        .get(&relative.to_string_lossy(), range)
        .await
    {
        Ok(stream) => stream,
        Err(DriverError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(facet_error("read", &error)),
    };
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.map_err(|error| {
        EnvError::backend(BACKEND, "read", format!("reading the file failed: {error}"))
    })?;
    Ok(Some(bytes))
}

#[async_trait]
impl ExecEnv for SandboxEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let admission = self.gate.admit("exec")?;
        // `docker exec -w` refuses a directory that does not exist yet
        // (`repo/` before the first checkout), so create the step's cwd
        // first, with the one command every image contract provides.
        if let Some(cwd) = &spec.cwd {
            let cwd = cwd.to_string_lossy().into_owned();
            let mut mkdir = ExecSpec::argv(["mkdir", "-p", cwd.as_str()]);
            mkdir.timeout_ms = Some(millis(MKDIR_TIMEOUT));
            let stderr = Arc::new(StdMutex::new(Vec::new()));
            let sink_stderr = stderr.clone();
            let controls = ExecControls {
                sink: Some(Arc::new(move |stream, chunk| {
                    if stream == OutputStream::Stderr {
                        sink_stderr
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend_from_slice(&chunk);
                    }
                    Box::pin(async { Ok(()) })
                })),
                ..ExecControls::default()
            };
            let outcome = self
                .sandbox
                .exec(mkdir, controls)
                .await
                .map_err(|error| facet_error("mkdir", &error))?;
            if outcome.termination != Termination::Exited || outcome.exit_code != Some(0) {
                let stderr = stderr
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                return Err(EnvError::backend(
                    BACKEND,
                    "mkdir",
                    format!(
                        "could not create `{cwd}`: {}",
                        String::from_utf8_lossy(&stderr).trim()
                    ),
                ));
            }
        }
        let working_dir = spec
            .cwd
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().into_owned());

        // The step's program and arguments go to the sandbox as given: the
        // exec contract is an argument vector, so nothing is quoted or
        // interpreted on the way.
        // The step's deadline rides to the provider, which ends the command
        // itself; a step that sets none runs until it exits or is stopped.
        let mut exec_spec = ExecSpec::argv(
            iter::once(spec.program.as_str()).chain(spec.args.iter().map(SmolStr::as_str)),
        );
        exec_spec.timeout_ms = spec.timeout.map(millis);
        exec_spec.working_directory = working_dir;
        exec_spec.env = merged_env(&self.env, &spec.env);
        let stdin = matches!(spec.stdin, StdinMode::Piped);
        Ok(Box::new(spawn_streamed(
            self.sandbox.clone(),
            Job::Exec(exec_spec),
            stdin,
            spec.output,
            admission,
        )))
    }

    fn workspace_path(&self) -> &str {
        &self.workspace
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.host_address
            .as_deref()
            .ok_or(EnvError::HostUnreachable)
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        self.env
            .get(name)
            .map(ToString::to_string)
            .or_else(|| self.ambient.get(name).cloned())
    }

    fn shares_host_filesystem(&self) -> bool {
        self.host
    }

    /// The provider's preview URL for the port: the Host provider answers
    /// with its own loopback, the Docker provider opens a forward on the
    /// plugin's loopback bridged into the container (nothing is published on
    /// the daemon), Daytona returns its preview link and token header. A
    /// provider without the facet offers no route.
    async fn preview_url(&self, port: u16) -> Result<Option<PreviewUrl>, EnvError> {
        let _admission = self.gate.admit("preview_url")?;
        let Some(port) = NonZeroU16::new(port) else {
            return Ok(None);
        };
        let options = Expose {
            signed: false,
            ttl_ms: None,
        };
        match self.sandbox.ports().expose(port, options).await {
            Ok(exposure) => Ok(Some(PreviewUrl {
                url:     exposure.url,
                headers: exposure.headers,
            })),
            // A provider that exposes no port offers no route.
            Err(DriverError::Unsupported { .. }) => Ok(None),
            Err(error) => Err(facet_error("preview_url", &error)),
        }
    }

    async fn release_preview_url(&self, port: u16) -> Result<(), EnvError> {
        let _admission = self.gate.admit("release_preview_url")?;
        let Some(port) = NonZeroU16::new(port) else {
            return Ok(());
        };
        match self.sandbox.ports().release(port).await {
            Ok(()) | Err(DriverError::Unsupported { .. }) => Ok(()),
            Err(error) => Err(facet_error("release_preview_url", &error)),
        }
    }

    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        let _admission = self.gate.admit("read")?;
        read(&*self.sandbox, relative, None).await
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let _admission = self.gate.admit("read")?;
        // One byte past the limit is enough to know the file is too big,
        // without ever pulling the whole of it across.
        let bounded = limit.saturating_add(1) as u64;
        let range = FileRange {
            offset: 0,
            length: Some(bounded),
        };
        match read(&*self.sandbox, relative, Some(range)).await? {
            Some(bytes) if bytes.len() > limit => Err(EnvError::workspace(
                "read",
                relative.display(),
                executor::oversized_read(limit),
            )),
            bytes => Ok(bytes),
        }
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        let _admission = self.gate.admit("write")?;
        self.sandbox
            .files()
            .put(
                &relative.to_string_lossy(),
                Box::pin(Cursor::new(contents.to_vec())),
            )
            .await
            .map_err(|error| facet_error("write", &error))
    }

    async fn list_directory(
        &self,
        path: &Path,
        depth: usize,
    ) -> Result<Vec<DirectoryEntry>, EnvError> {
        let _admission = self.gate.admit("list_directory")?;
        // Petri counts immediate children as depth one; the contract counts
        // them as depth zero.
        let Some(depth) = depth.checked_sub(1) else {
            return Ok(Vec::new());
        };
        let depth = u32::try_from(depth).unwrap_or(u32::MAX);
        // The contract names entries by their workload path; Petri names
        // them relative to the directory listed.
        let root = Path::new(&self.workspace).join(path);
        self.sandbox
            .files()
            .list(&path.to_string_lossy(), depth)
            .await
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| DirectoryEntry {
                        path:   Path::new(&entry.path)
                            .strip_prefix(&root)
                            .map_or(entry.path.clone(), |relative| {
                                relative.to_string_lossy().into_owned()
                            }),
                        is_dir: entry.kind == sandbox_driver::FileKind::Directory,
                        size:   (entry.kind == sandbox_driver::FileKind::File)
                            .then_some(entry.size),
                    })
                    .collect()
            })
            .map_err(|error| facet_error("list_directory", &error))
    }

    fn grace(&self) -> Duration {
        self.grace
    }
}

/// The process handle a step drives: its output, its stdin, its wait, and the
/// two raw stop signals the step's ladder fires.
struct SandboxProcess {
    lines:   Option<LineStream>,
    bytes:   Option<ByteStream>,
    stdin:   Option<StdinWriter>,
    term:    CancellationToken,
    kill:    CancellationToken,
    status:  watch::Receiver<Option<Result<ExitStatus, String>>>,
    cached:  Option<ExitStatus>,
    workers: JoinSet<()>,
}

impl Drop for SandboxProcess {
    fn drop(&mut self) {
        // Aborting the RPC drops its stream cancellation guard. Scope release
        // remains the fence for remote work; local pumps cannot outlive their
        // process handle while waiting on output or a slow provider.
        self.workers.abort_all();
    }
}

#[async_trait]
impl ProcessHandle for SandboxProcess {
    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    fn bytes(&mut self) -> Option<ByteStream> {
        self.bytes.take()
    }

    fn stdin(&mut self) -> Option<StdinWriter> {
        self.stdin.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        if let Some(status) = self.cached {
            return Ok(status);
        }
        let mut status = self.status.clone();
        loop {
            let current = status.borrow_and_update().clone();
            if let Some(result) = current {
                let value =
                    result.map_err(|message| EnvError::backend(BACKEND, "exec", message))?;
                self.cached = Some(value);
                return Ok(value);
            }
            if status.changed().await.is_err() {
                return Err(EnvError::Gone);
            }
        }
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        match sig {
            Sig::Term => self.term.cancel(),
            Sig::Kill => self.kill.cancel(),
        }
        Ok(())
    }
}

/// One-shot containers in a sandbox's world: the provider's own operation,
/// which shares the sandbox's workspace and network namespace, streams
/// through the same pumps, and is ended by the sandbox's stop and delete.
pub(crate) struct OneShotRunner {
    pub(crate) sandbox:      Arc<dyn Sandbox>,
    pub(crate) workspace:    String,
    pub(crate) host_address: Option<String>,
    /// The scope's env, under the spec's own.
    pub(crate) env:          BTreeMap<SmolStr, SmolStr>,
    pub(crate) gate:         RunGate,
}

impl OneShotRunner {
    /// Runs `spec` under an admission its caller already holds.
    pub(crate) fn run_admitted(
        &self,
        spec: OneShotContainer,
        admission: Admission,
    ) -> Result<Box<dyn ProcessHandle>, EnvError> {
        if self.sandbox.containers().is_none() {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                "this sandbox provider runs no one-shot containers",
            ));
        }
        let image = match spec.image {
            ContainerImage::Registry { image } => OneShotImage::Image(image.to_string()),
            ContainerImage::Build {
                context,
                dockerfile,
                tag,
                reuse,
            } => OneShotImage::Build {
                context: context.to_string_lossy().into_owned(),
                dockerfile: dockerfile.map(|file| file.to_string()),
                tag: tag.to_string(),
                reuse,
            },
        };
        let mut one_shot = OneShotSpec::new(image);
        one_shot.entrypoint = spec.entrypoint.map(|entrypoint| entrypoint.to_string());
        one_shot.args = spec.args.iter().map(ToString::to_string).collect();
        // The scope's env first, the spec's own on top.
        one_shot.env = merged_env(&self.env, &spec.env);
        one_shot.working_directory = spec.workdir.map(|workdir| workdir.to_string());
        Ok(Box::new(spawn_streamed(
            self.sandbox.clone(),
            Job::OneShot(one_shot),
            false,
            OutputMode::Lines,
            admission,
        )))
    }
}

#[async_trait]
impl ContainerRunner for OneShotRunner {
    fn workspace_path(&self) -> &str {
        &self.workspace
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.host_address
            .as_deref()
            .ok_or(EnvError::HostUnreachable)
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let admission = self.gate.admit("one-shot")?;
        self.run_admitted(spec, admission)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::path::PathBuf;

    use sandbox_driver::{ByteStream, Exposure, FileEntry, Files, Ports, SandboxRef, Status};

    use super::*;

    fn outcome(termination: Termination, code: Option<i32>, signal: Option<i32>) -> ExecOutcome {
        ExecOutcome {
            termination,
            exit_code: code,
            signal,
            output_loss: OutputLoss::default(),
        }
    }

    #[test]
    fn the_observed_signal_wins_and_a_lost_command_has_no_status() {
        let status = exit_status(&outcome(Termination::Exited, Some(3), None)).expect("exited");
        assert_eq!((status.code, status.signal), (Some(3), None));
        let status = exit_status(&outcome(Termination::Terminated, None, Some(9))).expect("ended");
        assert_eq!(status.signal, Some(9));
        let status = exit_status(&outcome(Termination::Terminated, None, None)).expect("ended");
        assert_eq!(status.signal, Some(Sig::Term.number()));
        let status = exit_status(&outcome(Termination::TimedOut, None, None)).expect("timed out");
        assert!(status.timed_out);
        assert!(exit_status(&outcome(Termination::Lost, Some(0), None)).is_err());
    }

    fn counted_loss() -> OutputLoss {
        let mut loss = OutputLoss::default();
        loss.dropped_frames = 2;
        loss.dropped_bytes = 300;
        loss
    }

    #[test]
    fn a_counted_loss_is_described_in_one_line() {
        assert_eq!(
            output_loss_line(counted_loss()).as_deref(),
            Some("[sandbox] 2 output frame(s), 300 bytes dropped by the provider")
        );
        assert_eq!(output_loss_line(OutputLoss::default()), None);
    }

    /// A sandbox whose one command prints a line to each stream and exits
    /// 2, reporting the given loss.
    struct ScriptedSandbox {
        loss:      OutputLoss,
        directory: PathBuf,
        env:       BTreeMap<String, String>,
    }

    #[async_trait]
    impl Sandbox for ScriptedSandbox {
        fn reference(&self) -> SandboxRef {
            panic!("a step does not read its sandbox's reference")
        }

        fn working_directory(&self) -> &Path {
            &self.directory
        }

        fn env(&self) -> &BTreeMap<String, String> {
            &self.env
        }

        async fn status(&self) -> Result<Status, DriverError> {
            panic!("a step does not read its sandbox's status")
        }

        async fn start(&self) -> Result<(), DriverError> {
            panic!("a step does not start its sandbox")
        }

        async fn stop(&self) -> Result<(), DriverError> {
            panic!("a step does not stop its sandbox")
        }

        async fn exec(
            &self,
            spec: ExecSpec,
            controls: ExecControls,
        ) -> Result<ExecOutcome, DriverError> {
            assert_eq!(spec.argv, vec!["true".to_owned()]);
            let sink = controls.sink.expect("the step supplies a sink");
            sink(OutputStream::Stdout, b"first\n".to_vec()).await?;
            sink(OutputStream::Stderr, b"warned\n".to_vec()).await?;
            let mut outcome = outcome(Termination::Exited, Some(2), None);
            outcome.output_loss = self.loss;
            Ok(outcome)
        }

        fn files(&self) -> &dyn Files {
            self
        }

        fn ports(&self) -> &dyn Ports {
            self
        }
    }

    #[async_trait]
    impl Files for ScriptedSandbox {
        async fn get(&self, _: &str, _: Option<FileRange>) -> Result<ByteStream, DriverError> {
            panic!("a step's spawn does not touch the filesystem")
        }

        async fn put(&self, _: &str, _: ByteStream) -> Result<(), DriverError> {
            panic!("a step's spawn does not touch the filesystem")
        }

        async fn list(&self, _: &str, _: u32) -> Result<Vec<FileEntry>, DriverError> {
            panic!("a step's spawn does not touch the filesystem")
        }
    }

    #[async_trait]
    impl Ports for ScriptedSandbox {
        async fn expose(&self, _: NonZeroU16, _: Expose) -> Result<Exposure, DriverError> {
            panic!("a step's spawn exposes no port")
        }

        async fn release(&self, _: NonZeroU16) -> Result<(), DriverError> {
            panic!("a step's spawn exposes no port")
        }
    }

    /// Runs the scripted command through the step's spawn path; returns
    /// its stdout lines, its stderr lines in order, and its exit status.
    async fn run_scripted(loss: OutputLoss) -> (Vec<String>, Vec<String>, ExitStatus) {
        let sandbox: Arc<dyn Sandbox> = Arc::new(ScriptedSandbox {
            loss,
            directory: PathBuf::from(crate::CONTAINER_WORKSPACE),
            env: BTreeMap::new(),
        });
        let admission = RunGate::default()
            .admit("exec")
            .expect("an open gate admits");
        let mut process = spawn_streamed(
            sandbox,
            Job::Exec(ExecSpec::argv(["true"])),
            false,
            OutputMode::Lines,
            admission,
        );
        let mut lines = process.lines().expect("a lines stream");
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        while let Some(line) = lines.recv().await {
            match line.stream {
                ir::LogStream::Stdout => stdout.push(line.line),
                ir::LogStream::Stderr => stderr.push(line.line),
            }
        }
        let status = process.wait().await.expect("the command's status");
        (stdout, stderr, status)
    }

    #[tokio::test]
    async fn a_counted_loss_is_one_stderr_line_after_the_output() {
        let (stdout, stderr, status) = run_scripted(counted_loss()).await;
        assert_eq!(status.code, Some(2));
        assert_eq!(stdout, vec!["first".to_owned()]);
        assert_eq!(stderr, vec![
            "warned".to_owned(),
            "[sandbox] 2 output frame(s), 300 bytes dropped by the provider".to_owned(),
        ]);
    }

    #[tokio::test]
    async fn a_lossless_result_passes_the_output_through_unchanged() {
        let (stdout, stderr, status) = run_scripted(OutputLoss::default()).await;
        assert_eq!(status.code, Some(2));
        assert_eq!(stdout, vec!["first".to_owned()]);
        assert_eq!(stderr, vec!["warned".to_owned()]);
    }
}
