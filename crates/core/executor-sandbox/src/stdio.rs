//! Text protocols over the driver's dedicated bidirectional stdio facet.
//!
//! The handle owns a supervisor. Dropping it requests termination rather than
//! aborting remote cleanup; its run admission fences that cleanup at shutdown.

use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use std::{future, io};

use async_trait::async_trait;
use executor::lines::{self, LINE_CHANNEL_CAPACITY};
use executor::{EnvError, ExitStatus, LineStream, ProcessHandle, Sig, StdinWriter};
use sandbox_driver::{StdioProcess, StdioProcessHandle, Termination};
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time;

use crate::gate::{Admission, DRAIN_BUDGET};
use crate::{BACKEND, env};

pub(crate) fn adapt(
    process: StdioProcess,
    deadline: Option<Duration>,
    grace: Duration,
    admission: Admission,
) -> StdioHandle {
    let StdioProcess {
        stdin,
        stdout,
        stderr_tail,
        handle,
    } = process;
    let (sender, receiver) = mpsc::channel(LINE_CHANNEL_CAPACITY);
    let (stop, mut stopping) = watch::channel(None);
    let worker = tokio::spawn(async move {
        let _admission = admission;
        let mut pumps = JoinSet::new();
        pumps.spawn(lines::pump(stdout, ir::LogStream::Stdout, sender.clone()));
        let deadline = async {
            match deadline {
                Some(duration) => time::sleep(duration).await,
                None => future::pending().await,
            }
        };
        // A plugin wait consumes its final reply (including stderr). Keep
        // that same future alive while terminating, rather than cancelling
        // it and issuing a second wait that can lose the reply.
        let waiting = handle.wait();
        tokio::pin!(waiting);
        let outcome = tokio::select! {
            (termination, code) = &mut waiting => Ok(env::exit_status(termination, code, None)),
            signal = async {
                match stopping.wait_for(Option::is_some).await {
                    Ok(signal) => signal.unwrap_or(Sig::Kill),
                    Err(_) => Sig::Kill,
                }
            } => stop_process(&*handle, waiting.as_mut()).await.map(|()| ExitStatus::signalled(signal.number())),
            () = deadline => stop_process(&*handle, waiting.as_mut()).await
                .map(|()| ExitStatus::timed_out(Sig::Kill.number())),
        };
        // A descendant or a stalled output consumer must not hold cleanup
        // indefinitely after the process ends. Aborting here owns only the
        // local reader; the provider has already been waited or terminated.
        let drained = time::timeout(grace, async {
            while let Some(result) = pumps.join_next().await {
                result.map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(())
        })
        .await;
        pumps.shutdown().await;
        // The facet retains stderr separately from protocol stdout. Forward
        // its bounded tail after exit, through the same line rules as exec.
        let tail = io::Cursor::new(stderr_tail.to_string_lossy().into_bytes());
        let diagnostics =
            time::timeout(grace, lines::pump(tail, ir::LogStream::Stderr, sender)).await;
        let status = outcome?;
        if !status.timed_out && status.signal.is_none() {
            drained.map_err(|_| "stdio stdout did not drain before its deadline".to_owned())??;
            diagnostics.map_err(|_| "stdio stderr did not drain before its deadline".to_owned())?;
        }
        Ok(status)
    });
    StdioHandle {
        stdin: Some(Box::new(TextWriter(Mutex::new(stdin)))),
        lines: Some(receiver),
        stop,
        worker,
        cached: None,
    }
}

async fn stop_process(
    handle: &dyn StdioProcessHandle,
    waiting: Pin<&mut impl future::Future<Output = (Termination, Option<i32>)>>,
) -> Result<(), String> {
    time::timeout(DRAIN_BUDGET, async {
        // Some providers share transport state between wait and terminate.
        // Keep polling wait while termination acquires that state.
        tokio::join!(handle.terminate(), waiting);
    })
    .await
    .map_err(|_| "stdio process did not stop before its deadline".to_owned())
}

pub(crate) struct StdioHandle {
    stdin:  Option<StdinWriter>,
    lines:  Option<LineStream>,
    stop:   watch::Sender<Option<Sig>>,
    worker: JoinHandle<Result<ExitStatus, String>>,
    cached: Option<Result<ExitStatus, String>>,
}

impl Drop for StdioHandle {
    fn drop(&mut self) {
        // The supervisor keeps its admission until remote termination and
        // local output cleanup finish, even if the caller abandons this handle.
        let _ = self.stop.send(Some(Sig::Kill));
    }
}

#[async_trait]
impl ProcessHandle for StdioHandle {
    fn stdin(&mut self) -> Option<StdinWriter> {
        self.stdin.take()
    }

    fn lines(&mut self) -> Option<LineStream> {
        self.lines.take()
    }

    async fn signal(&mut self, signal: Sig) -> Result<(), EnvError> {
        let _ = self.stop.send(Some(signal));
        Ok(())
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        if self.cached.is_none() {
            self.cached = Some(match (&mut self.worker).await {
                Ok(outcome) => outcome,
                Err(error) => Err(error.to_string()),
            });
        }
        self.cached
            .clone()
            .ok_or(EnvError::Gone)?
            .map_err(|message| EnvError::backend(BACKEND, "stdio", message))
    }
}

/// The driver's erased writer is Send, whereas StdinWriter is also Sync.
/// Exclusive poll access needs no locking; the mutex supplies the Sync bound.
struct TextWriter(Mutex<Pin<Box<dyn AsyncWrite + Send>>>);

impl AsyncWrite for TextWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .poll_shutdown(cx)
    }
}
