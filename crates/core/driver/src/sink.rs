//! Where log lines go, and where masking happens.
//!
//! Masking runs **before** the append, so the persisted log is post-mask. There
//! is no window in which a secret is on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::{fmt, io};

use executor::Masker;
use ir::{LogStream, Value};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt as _;

/// Where the driver persists step output: one append-only log per firing,
/// named `<node>-<firing>.log`. The run directory's `logs/` by default
/// ([`StepLogDir`]); a simulation keeps them in memory.
#[async_trait::async_trait]
pub trait StepLogStore: fmt::Debug + Send + Sync {
    /// Append one masked line from `stream` to the log `name`.
    async fn append(&self, name: &str, stream: LogStream, line: &str) -> io::Result<()>;

    /// Where the log `name` is, for a person reading a terminal.
    fn locate(&self, name: &str) -> String {
        name.to_owned()
    }
}

/// Step logs as files in a directory, each line tagged `[out]` or `[err]`.
#[derive(Debug)]
pub struct StepLogDir {
    dir: PathBuf,
}

impl StepLogDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait::async_trait]
impl StepLogStore for StepLogDir {
    /// The write itself. Its result is the sink's only failure signal: without
    /// it a full disk loses the whole step log while the run reports success.
    async fn append(&self, name: &str, stream: LogStream, line: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(name))
            .await?;
        let tag = match stream {
            LogStream::Stdout => "out",
            LogStream::Stderr => "err",
        };
        file.write_all(format!("[{tag}] {line}\n").as_bytes()).await
    }

    fn locate(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }
}

/// Writes step output to its store, and optionally echoes it.
pub(crate) struct LogSink {
    store:  Arc<dyn StepLogStore>,
    masker: Masker,
    echo:   bool,
    /// The prefix an echoed tag carries before the node: the invocation a
    /// child execution (a parallel branch) belongs to. Empty for the run's
    /// own execution.
    label:  String,
    /// Bytes echoed so far per firing, for the per-stage echo bound.
    echoed: Mutex<HashMap<(String, u64), usize>>,
}

impl LogSink {
    /// A sink over `store`, or over the run directory's `logs/` without one.
    pub(crate) fn new(
        run_dir: &Path,
        store: Option<&Arc<dyn StepLogStore>>,
        masker: Masker,
    ) -> Self {
        Self {
            store: store.map_or_else(
                || Arc::new(StepLogDir::new(run_dir.join("logs"))) as Arc<dyn StepLogStore>,
                Arc::clone,
            ),
            masker,
            echo: false,
            label: String::new(),
            echoed: Mutex::new(HashMap::new()),
        }
    }

    /// Also write lines to this process's stderr, each prefixed with the
    /// node instance and firing that produced it, so interleaved output from
    /// parallel stages stays attributable.
    #[must_use]
    pub(crate) fn with_echo(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    /// Prefix every echoed tag with `label/`: which invocation a child
    /// execution's lines belong to.
    #[must_use]
    pub(crate) fn with_label(mut self, label: &str) -> Self {
        label.clone_into(&mut self.label);
        self
    }

    /// The tag an echoed line carries: `[label/node#firing]`, or
    /// `[node#firing]` for the run's own execution.
    fn tag(&self, node: &str, firing: u64) -> String {
        if self.label.is_empty() {
            format!("{node}#{firing}")
        } else {
            format!("{}/{node}#{firing}", self.label)
        }
    }

    /// Echo a line of the driver's own about a firing (a retry notice), under
    /// the firing's tag. Not persisted: the event log carries the fact.
    #[expect(
        clippy::print_stderr,
        reason = "progress notices go to the user's terminal beside the echoed step output"
    )]
    pub(crate) fn note(&self, node: &str, firing: u64, text: &str) {
        if self.echo {
            eprintln!("[{}] {text}", self.tag(node, firing));
        }
    }

    pub(crate) fn masker(&self) -> &Masker {
        &self.masker
    }

    /// Mask a line and persist it. The masked line is what the caller should
    /// then hand to the core.
    #[expect(
        clippy::print_stderr,
        reason = "echoing step output to the user's terminal is the whole point of the \
                  `echo` option; stderr keeps it clear of a command's own stdout"
    )]
    pub(crate) async fn record(
        &self,
        node: &str,
        firing: u64,
        stream: LogStream,
        line: &str,
    ) -> String {
        let masked = self.masker.mask(line);
        let name = format!("{}-{firing}.log", sanitize(node));
        if let Err(err) = self.store.append(&name, stream, &masked).await {
            tracing::warn!(
                log = %self.store.locate(&name),
                node,
                firing,
                error = ?err,
                "step log write failed"
            );
        }
        if self.echo {
            match self.charge(node, firing, masked.len()) {
                Charge::Within => eprintln!("[{}] {}", self.tag(node, firing), bounded(&masked)),
                Charge::Crossed => eprintln!(
                    "[{}] … [echo truncated after {ECHO_STAGE_LIMIT} bytes; the full log is {}]",
                    self.tag(node, firing),
                    self.store.locate(&name)
                ),
                Charge::Exceeded => {}
            }
        }
        masked
    }

    /// Count `bytes` against the firing's echo budget.
    fn charge(&self, node: &str, firing: u64, bytes: usize) -> Charge {
        let mut echoed = self.echoed.lock().unwrap_or_else(PoisonError::into_inner);
        let used = echoed.entry((node.to_owned(), firing)).or_insert(0);
        if *used > ECHO_STAGE_LIMIT {
            return Charge::Exceeded;
        }
        *used += bytes;
        if *used > ECHO_STAGE_LIMIT {
            // Past the bound now: the marker prints once, then nothing more.
            *used = ECHO_STAGE_LIMIT + 1;
            Charge::Crossed
        } else {
            Charge::Within
        }
    }

    /// Mask every string in an outcome's data before the finish record is
    /// appended.
    pub(crate) fn mask_value(&self, value: &Value) -> Value {
        self.masker.mask_value(value)
    }
}

/// How much of one echoed line reaches the terminal. The log keeps the
/// whole line; the terminal gets a bounded prefix and a marker.
const ECHO_LINE_LIMIT: usize = 4096;

/// How much output of one firing reaches the terminal in total. The log
/// keeps everything; the terminal gets the first bytes, one marker naming
/// the log file, then silence for that firing.
pub(crate) const ECHO_STAGE_LIMIT: usize = 64 * 1024;

/// Where one echoed line landed against its firing's budget.
enum Charge {
    Within,
    /// This line crossed the bound: the marker replaces it.
    Crossed,
    /// The bound was crossed earlier: nothing prints.
    Exceeded,
}

/// A line cut to the echo limit at a character boundary.
fn bounded(line: &str) -> String {
    match line.char_indices().nth(ECHO_LINE_LIMIT) {
        Some((end, _)) => format!("{}… [line truncated]", &line[..end]),
        None => line.to_owned(),
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
