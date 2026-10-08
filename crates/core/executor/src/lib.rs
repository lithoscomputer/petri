//! The interface between the driver and the environments that run steps.
//!
//! An [`Executor`] materializes the environment for a scope instance and hands
//! out an [`ExecEnv`] capability. Step kinds are written once against that
//! capability and never mention where a process actually runs. The executors
//! themselves live in their own crate — `executor-sandbox` — and this crate
//! knows nothing about them: a new kind of environment is a new crate, not an
//! edit here.
//!
//! Everything in this crate signals **process groups**, never individual pids.
//! A `run:` script that backgrounds children has to die as a unit.
//!
//! [`lines`] is support for implementing the interface rather than part of it:
//! the line-capped output pump every executor needs, kept here so it is written
//! once.

mod container;
mod env;
mod error;
pub mod lines;
mod progress;
mod scope;
mod secrets;
mod spawn_env;

pub use container::{CONTAINER_RUNTIME_CLASS, ContainerImage, ContainerRunner, OneShotContainer};
pub use env::{
    ByteStream, DirectoryEntry, ExecEnv, ExitStatus, LineStream, LogLine, OutputChunk, OutputMode,
    PreviewUrl, ProcessHandle, ProcessSpec, Sig, StdinMode, StdinWriter, oversized_read,
};
pub use error::{EnvError, ReleaseReport};
pub use progress::{NoProgress, Progress, ProgressSink};
pub use scope::{
    AcquireContext, DEFAULT_GRACE, EnvHandle, EnvironmentId, Executor, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec, ServiceSpec, Teardown, WorkspaceId,
};
pub use secrets::{MapSecrets, Masker, SECRET_REF_KEY, Secret, SecretError, SecretProvider};
pub use spawn_env::{SpawnEnv, SpawnTarget, layer_exec};
