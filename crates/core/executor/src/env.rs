//! The environment a step runs in, and the process it runs.
//!
//! An [`ExecEnv`] is a capability handed to a step kind. The process step is
//! written once against it and never mentions Docker.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fmt, io, process};

use async_trait::async_trait;
use smol_str::SmolStr;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use crate::error::EnvError;

/// What to run.
///
/// `Debug` is hand-written: this is where resolved secrets land — `env` holds
/// the plaintext a step's `{"$secret": ...}` references resolved to, and the
/// last argument is the script itself — so the shape prints and the values do
/// not.
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: SmolStr,
    pub args:    Vec<SmolStr>,
    pub env:     BTreeMap<SmolStr, SmolStr>,
    /// Relative paths start at the workspace root; absolute paths are
    /// scope-local.
    pub cwd:     Option<PathBuf>,
    /// What the process reads on standard input. The default is nothing.
    pub stdin:   StdinMode,
    /// Select line logs or lossless byte chunks.
    pub output:  OutputMode,
    /// The executor ends the process after this long. The step that sets it
    /// owns the deadline: the driver arms no timer around such a step. `None`
    /// is no deadline.
    pub timeout: Option<Duration>,
}

/// How a process delivers output. Only the selected receiver is available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// Text lines with the executor's standard line cap.
    #[default]
    Lines,
    /// Unmodified bytes, with bounded buffering and backpressure.
    Bytes,
}

/// An unmodified chunk from one process stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputChunk {
    pub stream: ir::LogStream,
    pub bytes:  Vec<u8>,
}

/// Lossless output. Consumers must drain this while waiting for exit.
pub type ByteStream = mpsc::Receiver<OutputChunk>;

/// One workspace directory entry. Paths are relative to the listed directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path:   String,
    pub is_dir: bool,
    pub size:   Option<u64>,
}

/// Where a process's standard input comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StdinMode {
    /// `/dev/null`: the process reads end-of-file at once.
    #[default]
    Null,
    /// A pipe the step writes through [`ProcessHandle::stdin`] — a script's
    /// input, or one half of a protocol spoken over stdio.
    Piped,
}

/// The writing end of a piped standard input.
pub type StdinWriter = Box<dyn AsyncWrite + Send + Sync + Unpin>;

impl ProcessSpec {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: SmolStr::new(program),
            args:    args.iter().map(|a| SmolStr::new(*a)).collect(),
            env:     BTreeMap::new(),
            cwd:     None,
            stdin:   StdinMode::Null,
            output:  OutputMode::Lines,
            timeout: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_output(mut self, output: OutputMode) -> Self {
        self.output = output;
        self
    }

    #[must_use]
    pub fn with_stdin(mut self, stdin: StdinMode) -> Self {
        self.stdin = stdin;
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<SmolStr, SmolStr>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }
}

impl fmt::Debug for ProcessSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessSpec")
            .field("program", &self.program)
            .field("args", &self.args.len())
            .field("env", &self.env.len())
            .field("cwd", &self.cwd)
            .field("stdin", &self.stdin)
            .field("output", &self.output)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// One captured output line, tagged with the stream it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub stream:     ir::LogStream,
    pub line:       String,
    /// How many bytes the line cap cut off the end of `line`, which then
    /// ends with a marker naming the count. Zero for a line kept whole.
    pub dropped:    usize,
    /// The process ended the line with a newline. False only for the last
    /// line of a stream the process left unterminated, so a consumer that
    /// rejoins the lines can restore the exact bytes.
    pub terminated: bool,
}

/// Captured output, merged across both streams in arrival order.
///
/// One stream rather than the separate `stdout()` / `stderr()` of the handoff:
/// the requirement is that lines are emitted in arrival order, and merging two
/// receivers after the fact cannot recover an order that was never recorded.
/// Each line carries its stream tag, so nothing is lost.
pub type LineStream = mpsc::Receiver<LogLine>;

/// The signals this package sends. Always to a process **group**, never to a
/// pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
}

impl Sig {
    pub fn number(self) -> i32 {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    pub code:      Option<i32>,
    /// The signal that killed it, when it was killed.
    pub signal:    Option<i32>,
    /// The executor ended it at the [`ProcessSpec::timeout`] deadline.
    pub timed_out: bool,
}

impl ExitStatus {
    pub fn code(code: i32) -> Self {
        Self {
            code:      Some(code),
            signal:    None,
            timed_out: false,
        }
    }

    pub fn signalled(signal: i32) -> Self {
        Self {
            code:      None,
            signal:    Some(signal),
            timed_out: false,
        }
    }

    /// Ended by the executor at its deadline, with the signal it used.
    pub fn timed_out(signal: i32) -> Self {
        Self {
            code:      None,
            signal:    Some(signal),
            timed_out: true,
        }
    }

    pub fn is_success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none() && !self.timed_out
    }
}

/// The mapping every executor needs when the process it waited on was a real
/// child of this one: an exit code when there is one, otherwise the signal that
/// killed it. When the OS reports neither, the mapping says so rather than
/// fabricating a code.
impl From<process::ExitStatus> for ExitStatus {
    fn from(status: process::ExitStatus) -> Self {
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::code(code),
            (None, Some(signal)) => Self::signalled(signal),
            (None, None) => Self {
                code:      None,
                signal:    None,
                timed_out: false,
            },
        }
    }
}

/// A running process, addressed as a process group.
#[async_trait]
pub trait ProcessHandle: Send {
    /// The merged, stream-tagged output. Available once; later calls return
    /// `None`.
    fn lines(&mut self) -> Option<LineStream>;

    /// Takes the byte stream once, when spawned with [`OutputMode::Bytes`].
    /// Implementations without byte capture must reject that spawn mode.
    fn bytes(&mut self) -> Option<ByteStream> {
        None
    }

    /// The writing end of the process's standard input, when the spec asked
    /// for [`StdinMode::Piped`]. Available once; dropping it closes the pipe.
    fn stdin(&mut self) -> Option<StdinWriter> {
        None
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError>;

    /// Signal the process **group**. Idempotent: signalling an already-dead
    /// group is not an error.
    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError>;
}

/// How the driver's machine reaches a port a process listens on inside an
/// environment: a URL on this machine, and the headers a request to it must
/// carry. The reverse direction of [`ExecEnv::host_address`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewUrl {
    pub url:     String,
    pub headers: BTreeMap<String, String>,
}

/// The capability a step kind receives: somewhere to run a process, and a
/// workspace reached only through this interface.
///
/// Nothing here assumes the workspace is on the machine the driver runs on. A
/// step kind that needs a file in the workspace asks the environment for it,
/// and an executor whose workspace is remote — a cloud instance, an agent
/// elsewhere — answers over whatever transport it has. The local executors
/// answer from the filesystem.
#[async_trait]
pub trait ExecEnv: Send + Sync {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError>;

    /// The workspace root as a process running in this environment sees it, for
    /// building paths to hand to the process.
    fn workspace_path(&self) -> &str;

    /// Read a workspace-relative file. `Ok(None)` when it does not exist.
    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError>;

    /// Read at most `limit` bytes from a workspace-relative file.
    /// Implementations should stop reading once the limit is exceeded. The
    /// default preserves compatibility for remote executors and still
    /// rejects an oversized result.
    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        let Some(bytes) = self.read_file(relative).await? else {
            return Ok(None);
        };
        if bytes.len() > limit {
            return Err(EnvError::workspace(
                "read",
                relative.display(),
                oversized_read(limit),
            ));
        }
        Ok(Some(bytes))
    }

    /// Write a workspace-relative file, creating parent directories.
    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError>;

    /// How long a step gets between `SIGTERM` and `SIGKILL`.
    /// Lists entries in the execution scope without following directory
    /// symlinks. A depth of one lists immediate children; zero returns no
    /// entries.
    async fn list_directory(
        &self,
        path: &Path,
        depth: usize,
    ) -> Result<Vec<DirectoryEntry>, EnvError> {
        let _ = (path, depth);
        Err(EnvError::backend(
            "environment",
            "list_directory",
            "directory listing is unsupported",
        ))
    }

    fn grace(&self) -> Duration;

    /// How a process in *this* environment reaches the driver's machine — the
    /// host half of a URL for a service the driver runs beside the workspace.
    /// One fact, answered where it is known: a host process uses loopback; a
    /// containerized environment answers with the alias its executor
    /// guaranteed resolvable at create (`host.docker.internal`). A remote
    /// sandbox without a route returns [`EnvError::HostUnreachable`].
    fn host_address(&self) -> Result<&str, EnvError> {
        Ok("127.0.0.1")
    }

    /// One variable of the environment a process spawned here starts from,
    /// before the spec's own `env` lands on top: a container's effective env,
    /// snapshotted at create (the image's plus the scope's); a host process's
    /// inherited env under the scope's. A fact, not policy — `None` means this
    /// environment does not carry the variable, which is all the default can
    /// promise for an executor that never looked.
    fn ambient_env(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// Whether an absolute path on the driver's machine names the same file in
    /// this environment. True only for a host process: a container shares
    /// nothing with the host but the workspace (mounted at its own path), and
    /// a remote environment shares nothing at all — so `false` is the default.
    fn shares_host_filesystem(&self) -> bool {
        false
    }

    /// A URL on the driver's machine that reaches `port` inside this
    /// environment, with the headers its requests need: a host process's own
    /// loopback, a forward the executor opens into a container, a remote
    /// provider's preview link. `Ok(None)` when the environment offers no
    /// route to its ports, which is all the default can promise. The route
    /// stays open until [`Self::release_preview_url`] or the scope's release.
    async fn preview_url(&self, port: u16) -> Result<Option<PreviewUrl>, EnvError> {
        let _ = port;
        Ok(None)
    }

    /// End the route [`Self::preview_url`] opened for `port`. Idempotent: a
    /// port never routed, or released twice, succeeds.
    async fn release_preview_url(&self, port: u16) -> Result<(), EnvError> {
        let _ = port;
        Ok(())
    }
}

/// The refusal behind every [`ExecEnv::read_file_limited`] overrun, shared so
/// each executor reports the limit the same way.
pub fn oversized_read(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("the file exceeds the {limit}-byte read limit"),
    )
}
