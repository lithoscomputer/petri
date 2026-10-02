//! Fabro's `[run.clone]`: a run starts from a checkout of its repository.
//!
//! Fabro's server clones the repository into the sandbox before the first
//! stage. The standalone runner has the repository on the host (the root
//! `--repo` names, else the bundle root above the workflow file) and the
//! workspace wherever the scope's executor put it, on the host or inside a
//! container. The root `start` stage therefore clones the repository on the
//! host at the configured depth, packs the clone as one tarball, and hands
//! it to the environment's own `tar` through the executor, the delivery
//! the GitHub Actions checkout uses, so a Docker workspace receives the
//! same files as a host workspace and mode bits survive.
//!
//! The clone's `origin` is the repository's own `origin` when it has one
//! (a fresh clone from the host path would otherwise name that path), and
//! the remote-tracking refs the local clone created stay, so
//! `origin/main` resolves offline. Nothing is fetched from a remote.
//!
//! A non-empty `sparse` list checks out only those directories, in Git's
//! cone mode: the clone skips its checkout, `git sparse-checkout set --cone`
//! names the directories, and `git checkout` fills the work tree with them
//! and the root's own files. The cone lives in the clone's `.git`, which the
//! tarball carries, so the extracted workspace stays sparse.

use std::collections::VecDeque;
use std::fs::DirEntry;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io, process};

use executor::ProcessSpec;
use ir::{StepEvent, Value};
use serde::Deserialize;
use steps::StepCtx;
use tokio::task;

/// The failure class of a checkout that could not be delivered.
pub const CLASS: &str = "checkout";

/// The event kind that records a delivered checkout.
pub const EVENT_KIND: &str = "attractor.checkout";

/// The archive's name inside the workspace while it is extracted.
const ARCHIVE: &str = ".petri-checkout.tar";

/// The `start` stage's `checkout` config: the run's clone settings and the
/// repository the host bound, as the lowering wrote them.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Checkout {
    pub enabled:    bool,
    pub depth:      i64,
    /// The cone's directories, relative to the repository root. Empty is a
    /// full checkout.
    pub sparse:     Vec<String>,
    pub repository: Option<String>,
}

impl Default for Checkout {
    fn default() -> Self {
        Self {
            enabled:    true,
            depth:      100,
            sparse:     Vec::new(),
            repository: None,
        }
    }
}

/// What a delivered checkout looked like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub repository: PathBuf,
    pub commit:     String,
    pub depth:      i64,
    /// The cone's directories, normalized. Empty for a full checkout.
    pub sparse:     Vec<String>,
    pub files:      usize,
}

/// A checkout that could not be delivered. The run cannot start from the
/// repository it was asked to start from.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CheckoutError(String);

/// Seed the stage's workspace from the configured repository. `Ok(None)`
/// when the config asks for no checkout, the host bound no repository, or
/// the repository is not a Git work tree (the workspace then starts empty,
/// as before, with a log line saying so).
pub async fn seed(ctx: &StepCtx, config: &Value) -> Result<Option<Delivered>, CheckoutError> {
    let checkout: Checkout = match config {
        Value::Null => return Ok(None),
        other => serde_json::from_value(other.clone())
            .map_err(|e| CheckoutError(format!("the checkout config is malformed: {e}")))?,
    };
    if !checkout.enabled {
        ctx.log(
            ir::LogStream::Stderr,
            "checkout: [run.clone] is disabled; the workspace starts empty",
        )
        .await;
        return Ok(None);
    }
    let sparse = cone_directories(&checkout.sparse).map_err(CheckoutError)?;
    let Some(repository) = checkout.repository.as_deref().map(PathBuf::from) else {
        return Ok(None);
    };
    let Some(toplevel) = git_toplevel(&repository) else {
        ctx.log(
            ir::LogStream::Stderr,
            format!(
                "checkout: {} is not a Git repository; the workspace starts empty",
                repository.display()
            ),
        )
        .await;
        return Ok(None);
    };
    let depth = checkout.depth;
    let (tarball, commit, files) = {
        let toplevel = toplevel.clone();
        let sparse = sparse.clone();
        task::spawn_blocking(move || pack_clone(&toplevel, depth, &sparse))
            .await
            .map_err(|e| CheckoutError(format!("the checkout did not complete: {e}")))?
            .map_err(CheckoutError)?
    };
    let archive_bytes = tarball.len();
    ctx.env
        .write_file(Path::new(ARCHIVE), &tarball)
        .await
        .map_err(|e| CheckoutError(format!("could not write the checkout archive: {e}")))?;
    drop(tarball);
    unpack(ctx).await?;
    tracing::info!(
        repository = %toplevel.display(),
        %commit,
        depth,
        ?sparse,
        archive_bytes,
        files,
        "workspace checked out"
    );
    ctx.log(
        ir::LogStream::Stderr,
        format!(
            "checkout: {} at {} ({}{})",
            toplevel.display(),
            &commit[..commit.len().min(12)],
            if depth > 0 {
                format!("depth {depth}")
            } else {
                "full history".to_owned()
            },
            if sparse.is_empty() {
                String::new()
            } else {
                format!(", sparse {}", sparse.join(", "))
            }
        ),
    )
    .await;
    let _ = ctx
        .logs
        .send(StepEvent::Custom(serde_json::json!({
            "kind": EVENT_KIND,
            "node": ctx.node.as_str(),
            "firing": ctx.firing,
            "repository": toplevel.to_string_lossy(),
            "commit": commit,
            "depth": depth,
            "sparse": sparse,
            "files": files,
        })))
        .await;
    Ok(Some(Delivered {
        repository: toplevel,
        commit,
        depth,
        sparse,
        files,
    }))
}

/// The cone's directories as Git's cone mode reads them: relative paths
/// with no trailing `/`. An entry that is empty, absolute, has an empty,
/// `.` or `..` component, starts with `!`, or holds a glob character names
/// no single directory, and is refused before anything is cloned.
fn cone_directories(entries: &[String]) -> Result<Vec<String>, String> {
    entries
        .iter()
        .map(|entry| {
            let directory = entry.trim_end_matches('/');
            let refuse = |why: &str| Err(format!("[run.clone] sparse entry {entry:?} {why}"));
            if directory.is_empty() {
                return refuse("names no directory");
            }
            if entry.starts_with('/') {
                return refuse("is absolute; name a directory relative to the repository root");
            }
            if directory.starts_with('!') {
                return refuse("starts with `!`; cone mode takes directories, not patterns");
            }
            if directory.contains(['*', '?', '[', ']', '\\']) {
                return refuse("holds a glob character; cone mode takes directories, not patterns");
            }
            if directory
                .split('/')
                .any(|component| matches!(component, "" | "." | ".."))
            {
                return refuse("has an empty, `.` or `..` component");
            }
            Ok(directory.to_owned())
        })
        .collect()
}

/// The work tree root of `path`, when it is inside one.
fn git_toplevel(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// Clone `toplevel` into a temporary directory at `depth` (0 is the full
/// history), point `origin` at the repository's own origin when it has
/// one, check out only the `sparse` cone when it names directories, and
/// pack the clone (its `.git` included) as one tar archive whose entries are
/// relative to the workspace root.
fn pack_clone(
    toplevel: &Path,
    depth: i64,
    sparse: &[String],
) -> Result<(Vec<u8>, String, usize), String> {
    let staging = env::temp_dir().join(format!("petri-checkout-{}-{}", process::id(), unique()));
    let clone = staging.join("clone");
    fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create {}: {e}", staging.display()))?;
    let result = clone_and_pack(toplevel, depth, sparse, &clone);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn clone_and_pack(
    toplevel: &Path,
    depth: i64,
    sparse: &[String],
    clone: &Path,
) -> Result<(Vec<u8>, String, usize), String> {
    // `file://` so `--depth` applies: Git ignores depth on a plain local path.
    let url = format!("file://{}", toplevel.display());
    let mut command = Command::new("git");
    command.args(["clone", "--quiet", "--no-hardlinks"]);
    if depth > 0 {
        command.arg(format!("--depth={depth}"));
    }
    if !sparse.is_empty() {
        command.arg("--no-checkout");
    }
    command.arg(&url).arg(clone);
    run(&mut command, "git clone")?;
    if !sparse.is_empty() {
        let mut command = Command::new("git");
        command
            .args(["sparse-checkout", "set", "--cone", "--"])
            .args(sparse)
            .current_dir(clone);
        run(&mut command, "git sparse-checkout set")?;
        let mut command = Command::new("git");
        command.args(["checkout", "--quiet"]).current_dir(clone);
        run(&mut command, "git checkout")?;
    }
    if let Some(origin) = remote_url(toplevel, "origin") {
        let mut command = Command::new("git");
        command
            .args(["remote", "set-url", "origin", &origin])
            .current_dir(clone);
        run(&mut command, "git remote set-url")?;
    }
    let mut command = Command::new("git");
    command.args(["rev-parse", "HEAD"]).current_dir(clone);
    let commit = run(&mut command, "git rev-parse")?.trim().to_owned();
    let mut files = 0;
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    append_dir(&mut builder, clone, Path::new(""), &mut files)
        .map_err(|e| format!("could not pack the checkout: {e}"))?;
    let bytes = builder
        .into_inner()
        .map_err(|e| format!("could not finish the checkout archive: {e}"))?;
    Ok((bytes, commit, files))
}

/// Append every entry under `dir` at `prefix`, so the archive's root is the
/// clone's root itself and `tar -x -C <workspace>` lands the files there.
fn append_dir(
    builder: &mut tar::Builder<Vec<u8>>,
    dir: &Path,
    prefix: &Path,
    files: &mut usize,
) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = prefix.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            builder.append_dir(&name, &path)?;
            append_dir(builder, &path, &name, files)?;
        } else {
            builder.append_path_with_name(&path, &name)?;
            *files += 1;
        }
    }
    Ok(())
}

fn remote_url(repository: &Path, remote: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(repository)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|url| !url.is_empty())
}

fn run(command: &mut Command, what: &str) -> Result<String, String> {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    let output = command
        .output()
        .map_err(|e| format!("could not run {what}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Extract the archive with the environment's own `tar`, then remove it.
async fn unpack(ctx: &StepCtx) -> Result<(), CheckoutError> {
    let workspace = ctx.env.workspace_path().to_string();
    let archive = format!("{workspace}/{ARCHIVE}");
    let spec = ProcessSpec::new("sh", &[
        "-c",
        "tar -xf \"$1\" -C \"$2\" && rm -f \"$1\"",
        "petri-checkout",
        &archive,
        &workspace,
    ]);
    let mut handle = ctx
        .env
        .spawn(spec)
        .await
        .map_err(|e| CheckoutError(format!("could not run `tar` for the checkout: {e}")))?;
    let mut said = VecDeque::with_capacity(4);
    if let Some(mut lines) = handle.lines() {
        while let Some(line) = lines.recv().await {
            if said.len() == 4 {
                said.pop_front();
            }
            said.push_back(line.line);
        }
    }
    let status = handle
        .wait()
        .await
        .map_err(|e| CheckoutError(format!("extracting the checkout did not complete: {e}")))?;
    if status.code != Some(0) {
        return Err(CheckoutError(format!(
            "could not extract the checkout: tar said {}",
            said.into_iter().collect::<Vec<_>>().join(" | ")
        )));
    }
    Ok(())
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
