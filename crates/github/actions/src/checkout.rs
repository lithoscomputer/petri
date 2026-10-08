//! `github/checkout`: the local-checkout substitute.
//!
//! A local runner should test *the tree you have*. Where the lowering
//! substituted a supportable `actions/checkout` call, this step materializes
//! `GITHUB_WORKSPACE` from the run's own repository — the committed HEAD as a
//! depth-1 local clone, plus the uncommitted tracked diff and the
//! untracked-but-not-ignored files, never the ignored bulk (`target/`,
//! `node_modules/`). `.git` rides along, so later `git` steps keep working —
//! shaped like GitHub's checkout leaves it: a branch run's branch checked out
//! with a matching remote-tracking ref, `origin` naming the repository.
//! Offline, token-less, and an explicit documented delta from GitHub.
//!
//! The workspace stays a copy: the snapshot is assembled host-side in a
//! scratch directory (the real repository is never bind-mounted and never
//! mutated), packed as one tarball — mode bits and symlinks survive — written
//! through `ExecEnv::write_file` like every other runner file, and extracted
//! by the environment's own `tar`. Host and container scopes get the workspace
//! identically, owned by the user the job's steps run as.
//!
//! A repository without `.git` — the corpus's fetched workflow trees — copies
//! as a plain tree: no history to clone, nothing ignored to skip.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, io, process};

use ir::{FailureClass, LogStream, Outcome, Value};
use serde_json::Map;
use steps::{Ending, Step, StepCtx, StepFailure, ending_outcome, ladder};
use tokio::process::Command;
use tokio::{fs as async_fs, task, time};

use crate::config::CheckoutConfig;
use crate::gate;
use crate::session::{self, REPO_DIR};

/// The failure class for everything checkout-shaped: a missing source, a git
/// that cannot snapshot, an extraction that failed.
pub(crate) const CHECKOUT_CLASS: FailureClass = FailureClass::new_static("checkout");

/// The step kind. `NAME` must match what the frontend emits.
pub struct CheckoutStep;

#[async_trait::async_trait]
impl Step for CheckoutStep {
    const NAME: &'static str = frontend_gha::CHECKOUT_KIND;
    type Config = CheckoutConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        match execute(config, ctx).await {
            Ok(outcome) => outcome,
            Err(failure) => failure.into(),
        }
    }
}

#[tracing::instrument(
    name = "github.checkout_step",
    level = "debug",
    skip_all,
    fields(
        git_ref = config.reference.as_deref().unwrap_or(""),
        dest_path = config.path.as_deref().unwrap_or(""),
    )
)]
async fn execute(config: CheckoutConfig, mut ctx: StepCtx) -> Result<Outcome, StepFailure> {
    // The snapshot's name carries a process-global counter, not the firing
    // id: concurrent *runs* in one process (the corpus sweep) each count
    // firings from zero, and a shared name would let one run's cleanup delete
    // another's snapshot mid-copy.
    static SNAPSHOT: AtomicU64 = AtomicU64::new(0);

    let no_env = BTreeMap::new();
    if let Some(outcome) = gate::refusal(
        config.gate.as_ref(),
        config.cancelled,
        &no_env,
        config.job_environment.as_deref(),
        config.background.as_deref(),
        &ctx,
    )
    .await?
    {
        return Ok(outcome);
    }
    let Some(source) = config.source.as_deref().filter(|s| !s.is_empty()) else {
        return Err(StepFailure {
            class:   CHECKOUT_CLASS,
            message: format!(
                "no local repository is configured for this run — the host fills the `{}.{}` \
                 run parameter from the workflow's repository root",
                frontend_gha::REPO_PARAM_CONTEXT,
                frontend_gha::REPO_PARAM_KEY
            ),
        });
    };
    let source = PathBuf::from(source);

    // The snapshot, assembled beside nothing the run owns and removed on the
    // way out whatever happens.
    let scratch = env::temp_dir().join(format!(
        "petri-checkout-{}-{}",
        process::id(),
        SNAPSHOT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = async_fs::remove_dir_all(&scratch).await;
    let result = materialize(&source, &scratch, &config, &mut ctx).await;
    let _ = async_fs::remove_dir_all(&scratch).await;
    result
}

async fn materialize(
    source: &Path,
    scratch: &Path,
    config: &CheckoutConfig,
    ctx: &mut StepCtx,
) -> Result<Outcome, StepFailure> {
    let clone = scratch.join("clone");
    async_fs::create_dir_all(&clone)
        .await
        .map_err(|e| checkout_error(format!("could not create a scratch dir: {e}")))?;

    let commit = if source.join(".git").exists() {
        snapshot_repository(source, &clone, config, ctx).await?
    } else {
        // No history: a plain tree (the corpus case) copies wholesale.
        let (from, to) = (source.to_path_buf(), clone.clone());
        blocking(move || {
            copy_tree(&from, &to)
                .map_err(|e| checkout_error(format!("could not copy `{}`: {e}", from.display())))
        })
        .await?;
        String::new()
    };

    // One tarball, entries already prefixed with their workspace-relative
    // destination, extracted at the workspace root by the environment's own
    // `tar` — which is what preserves mode bits inside a container.
    let destination = match config.path.as_deref() {
        Some(path) => format!("{REPO_DIR}/{}", path.trim_matches('/')),
        None => REPO_DIR.to_string(),
    };
    let clone_dir = clone.clone();
    let tarball = blocking(move || {
        let mut builder = tar::Builder::new(Vec::new());
        builder.follow_symlinks(false);
        builder
            .append_dir_all(&destination, &clone_dir)
            .and_then(|()| builder.into_inner())
            .map_err(|e| checkout_error(format!("could not pack the snapshot: {e}")))
    })
    .await?;

    let tar_rel = format!(".ci/checkout/{}.tar", ctx.firing.raw());
    ctx.env
        .write_file(Path::new(&tar_rel), &tarball)
        .await
        .map_err(|e| checkout_error(format!("could not write the snapshot: {e}")))?;
    drop(tarball);

    let ending = extract(&tar_rel, ctx).await?;
    let mut output = Map::new();
    output.insert("commit".into(), Value::String(commit));
    Ok(ending_outcome(&ending, &config.soft_fail, output))
}

/// The committed HEAD plus the working tree's uncommitted state, as a fresh
/// depth-1 clone overlaid with every path `git status` names.
async fn snapshot_repository(
    source: &Path,
    clone: &Path,
    config: &CheckoutConfig,
    ctx: &mut StepCtx,
) -> Result<String, StepFailure> {
    let url = format!("file://{}", source.display());
    let clone_arg = clone.display().to_string();
    let clone_args = ["clone", "--depth", "1", "--quiet", "--", &url, &clone_arg];
    // The clone and the status walk read the source independently; only the
    // overlay needs both.
    let (_, porcelain) = tokio::try_join!(
        git(None, &clone_args),
        git(Some(source), &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all"
        ],),
    )?;
    shape_clone(clone, config).await?;

    // The dirty overlay: worktree truth wins, path by path.
    let (source_dir, clone_dir) = (source.to_path_buf(), clone.to_path_buf());
    let copied = blocking(move || {
        let mut copied = 0usize;
        for entry in porcelain_paths(porcelain.as_bytes()) {
            let from = source_dir.join(&entry);
            let to = clone_dir.join(&entry);
            if from.symlink_metadata().is_ok() {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| checkout_error(format!("overlay `{entry}`: {e}")))?;
                }
                let _ = fs::remove_file(&to);
                copy_entry(&from, &to)
                    .map_err(|e| checkout_error(format!("overlay `{entry}`: {e}")))?;
                copied += 1;
            } else {
                // Deleted in the working tree: deleted in the snapshot.
                let _ = fs::remove_file(&to);
            }
        }
        Ok(copied)
    })
    .await?;
    if copied > 0 {
        ctx.log(
            LogStream::Stdout,
            format!("local checkout: committed HEAD plus {copied} uncommitted path(s)"),
        )
        .await;
    }

    let head = git(Some(clone), &["rev-parse", "HEAD"]).await?;
    tracing::info!(
        commit = head.trim(),
        dirty_path_count = copied,
        "local checkout snapshot ready"
    );
    Ok(head.trim().to_string())
}

/// Give the fresh clone the git shape GitHub's checkout leaves behind, so
/// actions asking ordinary questions — the current branch, a rev-parse, a
/// diff against a base — see what they expect. A branch run gets its branch:
/// the clone of a detached source (a corpus pin) arrives branchless, and a
/// branched source's name may not be the run's, so `-B` from a matching
/// remote-tracking ref covers both, exactly as the real checkout does. When
/// the repository is named, `origin` is set to its real URL: the clone's
/// `file://` host path is dead inside a container, and not what an action
/// reading the remote should see.
async fn shape_clone(clone: &Path, config: &CheckoutConfig) -> Result<(), StepFailure> {
    if let Some(branch) = config
        .reference
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .filter(|b| !b.is_empty())
    {
        let tracking = format!("refs/remotes/origin/{branch}");
        git(Some(clone), &["update-ref", &tracking, "HEAD"]).await?;
        git(Some(clone), &[
            "checkout", "--quiet", "-B", branch, &tracking,
        ])
        .await?;
    }
    if let (Some(server), Some(repository)) = (
        config.server_url.as_deref().filter(|s| !s.is_empty()),
        config.repository.as_deref().filter(|s| !s.is_empty()),
    ) {
        let url = format!("{}/{repository}", server.trim_end_matches('/'));
        git(Some(clone), &["remote", "set-url", "origin", &url]).await?;
    }
    Ok(())
}

/// Paths out of `git status --porcelain -z`: `XY path\0`, with a rename's
/// original consumed alongside so it never reads as its own entry.
fn porcelain_paths(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = bytes.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        let text = String::from_utf8_lossy(field);
        let Some((status, path)) = text.split_at_checked(3) else {
            continue;
        };
        if status.starts_with('R') || status.starts_with('C') {
            let _original = fields.next();
        }
        out.push(path.to_string());
    }
    out
}

fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let meta = from.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        unix_fs::symlink(target, to)?;
        return Ok(());
    }
    fs::copy(from, to).map(|_| ())
}

/// A plain tree, copied whole (skipping nothing: with no git there is no
/// ignore file semantics to honor).
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            copy_entry(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Run the environment's `tar` over the streamed tarball; the cancel ladder
/// applies as it does to every process. `--no-same-owner` gives the files to
/// the extracting user: a container job runs as root, and root's `tar` would
/// otherwise restore the host user's uid, so a later `git` step refuses the
/// workspace as a repository of dubious ownership.
async fn extract(tar_rel: &str, ctx: &mut StepCtx) -> Result<Ending, StepFailure> {
    let workspace = ctx.env.workspace_path().to_string();
    let archive = format!("{workspace}/{tar_rel}");
    let mut handle = ctx
        .env
        .spawn(executor::ProcessSpec::new("tar", &[
            "--no-same-owner",
            "-xf",
            &archive,
            "-C",
            &workspace,
        ]))
        .await
        .map_err(|e| checkout_error(format!("could not run `tar`: {e}")))?;
    let drain = session::forward_lines(handle.lines(), ctx.logs.clone());
    let grace = ctx.env.grace();
    let ending = ladder(&mut *handle, &mut ctx.control, grace).await;
    // `tar`'s tail output lands before the step outcome, under the same bound
    // the docker action's sink drain uses.
    if let Some(drain) = drain {
        let _ = time::timeout(session::SINK_LIMIT, drain).await;
    }
    Ok(ending)
}

/// Filesystem-heavy snapshot work runs on the blocking pool, not this
/// worker thread — a large tree walk must not stall the runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, StepFailure> + Send + 'static,
) -> Result<T, StepFailure> {
    task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(checkout_error(format!("a snapshot task failed: {e}"))))
}

async fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, StepFailure> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    // Never prompt, as the action source's git runner never prompts: a path
    // that unexpectedly needs credentials fails fast instead of hanging.
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| checkout_error(format!("could not run `git`: {e}")))?;
    if !output.status.success() {
        return Err(checkout_error(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn checkout_error(message: String) -> StepFailure {
    StepFailure {
        class: CHECKOUT_CLASS,
        message,
    }
}
