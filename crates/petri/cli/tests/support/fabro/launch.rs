//! Launch the shipped `petri` binary in an isolated environment and read the
//! run back.
//!
//! The child sees none of the developer's environment: no provider
//! credentials, no shell configuration, a fresh `HOME`, and a catalog layer
//! that points every provider a case uses at a loopback twin. A provider the
//! case did not redirect is not enabled at all, so a workflow that names one
//! fails before any live endpoint is reached.
//!
//! Every launch has a deadline. On expiry the child's process group is
//! killed and reaped, so a hung run fails the test instead of hanging
//! Nextest.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use std::{env, fs, process};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};

use super::inspect;
use super::twins::{Provider, Twin};

/// How long one `petri run` may take before the harness kills it.
pub(crate) const RUN_DEADLINE: Duration = Duration::from_secs(120);

/// The `local` environment every case's user settings layer declares, as
/// the pinned Fabro's harness declares it in its server settings. A
/// differential cell, and a test that stages a bundle whose
/// `.fabro/project.toml` selects the Fabro repository's Daytona
/// environment, selects it with `--environment local`, as every `fabro run`
/// of the pinned harness does; a case that selects its own environment in
/// its `workflow.toml` runs in that one.
pub(crate) const LOCAL_ENVIRONMENT_SETTINGS: &str = "[environments.local]\nprovider = \"local\"\n";

/// One case's private directory tree: workflow, run dir, home, store.
pub(crate) struct Case {
    pub(crate) root:       PathBuf,
    pub(crate) run_dir:    PathBuf,
    /// The fake credential this case hands every twin: its namespace.
    pub(crate) credential: String,
    layers:                Vec<String>,
    providers:             Vec<&'static str>,
    plugin_link:           PathBuf,
    /// The container backend the case runs on, when it does not run on the
    /// host: its `--backend` name and its plugin, linked per case.
    container:             Option<ContainerBackend>,
}

/// A container backend a case runs its workflows on: `docker` through the
/// Docker plugin, `daytona` through the Daytona plugin.
struct ContainerBackend {
    kind: &'static str,
    /// The plugin, linked per case, so a leaked plugin process is
    /// attributable to the case by its command line.
    link: PathBuf,
}

impl ContainerBackend {
    /// The variables Petri forwards to this kind's plugin: the daemon or
    /// account this test process was pointed at, so the binary's plugin
    /// reaches the same one. Unset, the plugin uses its defaults.
    fn forwarded_env(&self) -> &'static [&'static str] {
        match self.kind {
            "docker" => &[
                "DOCKER_HOST",
                "DOCKER_TLS_VERIFY",
                "DOCKER_CERT_PATH",
                "DOCKER_API_VERSION",
            ],
            "daytona" => &[
                "DAYTONA_API_KEY",
                "DAYTONA_JWT_TOKEN",
                "DAYTONA_ORGANIZATION_ID",
                "DAYTONA_API_URL",
                "DAYTONA_TARGET",
            ],
            other => panic!("no container backend `{other}`"),
        }
    }

    /// `PETRI_SANDBOX_<KIND>_PLUGIN`.
    fn plugin_variable(&self) -> String {
        format!("PETRI_SANDBOX_{}_PLUGIN", self.kind.to_ascii_uppercase())
    }
}

impl Case {
    /// A fresh case under the system temp dir. `label` names it; the process
    /// id and a counter keep concurrent cases apart.
    pub(crate) fn new(label: &str) -> Self {
        // Kept after the test as failure evidence; the temp dir ages out.
        let root = env::temp_dir().join("petri-tests").join(format!(
            "fabro-blackbox-{label}-{}-{}",
            process::id(),
            testkit::unique_id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("case root");
        for sub in ["home", "store", "cache", "plugins", "workflow"] {
            fs::create_dir_all(root.join(sub)).expect("case subdirectory");
        }
        let fabro_home = root.join("home").join(".fabro");
        fs::create_dir_all(&fabro_home).expect("fabro home");
        fs::write(fabro_home.join("settings.toml"), LOCAL_ENVIRONMENT_SETTINGS)
            .expect("settings.toml");
        let plugin = env::var_os("PETRI_SANDBOX_HOST_PLUGIN")
            .map(PathBuf::from)
            .expect("PETRI_SANDBOX_HOST_PLUGIN names the host sandbox plugin");
        // A per-case path for the plugin executable, so a leaked plugin
        // process is attributable to this case by its command line.
        let plugin_link = root.join("plugins").join("sandbox-driver-host");
        link_plugin(&plugin, &plugin_link);
        Self {
            credential: format!("fake-{label}-{}", testkit::unique_id()),
            run_dir: root.join("run"),
            root,
            layers: Vec::new(),
            providers: Vec::new(),
            plugin_link,
            container: None,
        }
    }

    /// Run this case's workflows on `--backend docker` through the Docker
    /// plugin `PETRI_SANDBOX_DOCKER_PLUGIN` names. The caller checks that a
    /// daemon is reachable first (`testkit::is_docker_ready`).
    pub(crate) fn docker(self) -> Self {
        self.on_container_backend("docker")
    }

    /// Run this case's workflows on `--backend daytona` through the Daytona
    /// plugin `PETRI_SANDBOX_DAYTONA_PLUGIN` names, with the credentials in
    /// this process's environment. The caller checks the tier is available
    /// first (`testkit::is_daytona_ready`).
    pub(crate) fn daytona(self) -> Self {
        self.on_container_backend("daytona")
    }

    fn on_container_backend(mut self, kind: &'static str) -> Self {
        let backend = ContainerBackend {
            kind,
            link: self
                .root
                .join("plugins")
                .join(format!("sandbox-driver-{kind}")),
        };
        let variable = backend.plugin_variable();
        let plugin = env::var_os(&variable).map_or_else(
            || panic!("{variable} names the {kind} sandbox plugin"),
            PathBuf::from,
        );
        link_plugin(&plugin, &backend.link);
        self.container = Some(backend);
        self
    }

    /// Whether this case runs on `--backend docker`.
    pub(crate) fn is_docker(&self) -> bool {
        self.container
            .as_ref()
            .is_some_and(|backend| backend.kind == "docker")
    }

    /// The `--backend` argument of a case on a container backend.
    fn backend_args(&self) -> Vec<&str> {
        self.container
            .as_ref()
            .map_or_else(Vec::new, |backend| vec!["--backend", backend.kind])
    }

    /// The environment every `petri` command of this case runs with.
    fn command(&self, path: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_petri"));
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            // The system temp dir, not a per-case one: the sandbox plugin
            // binds a Unix socket under it, and a long path exceeds the
            // socket path limit.
            .env("TMPDIR", env::temp_dir())
            .env("PETRI_STORE", self.root.join("store"))
            .env("PETRI_CACHE_DIR", self.root.join("cache"))
            .env("PETRI_SANDBOX_HOST_PLUGIN", &self.plugin_link)
            .env("PETRI_SANDBOX_PLUGIN_DEV", "1")
            .env("PETRI_LOG", "warn");
        if let Some(backend) = &self.container {
            command.env(backend.plugin_variable(), &backend.link);
            for name in backend.forwarded_env() {
                if let Some(value) = env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        command
    }

    /// `petri sandbox prune --run-dir <run dir>`: the retrieval command a
    /// container run reports. Returns the exit code and stderr.
    pub(crate) async fn prune(&self) -> (Option<i32>, String) {
        let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
        let mut command = self.command(&path);
        command
            .args(["sandbox", "prune", "--run-dir"])
            .arg(&self.run_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .args(self.backend_args());
        let output = timeout(RUN_DEADLINE, command.output())
            .await
            .expect("prune finishes before the deadline")
            .expect("petri sandbox prune runs");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Point `provider` at `twin` for this case's runs.
    pub(crate) fn redirect(&mut self, twin: &Twin) {
        self.layers.push(twin.catalog_layer());
        self.providers.push(twin.provider.id());
    }

    /// Point `provider` at a base URL nothing listens on.
    pub(crate) fn redirect_to_nothing(&mut self, provider: Provider, base_url: &str) {
        self.layers.push(format!(
            "schema_version = 1\n[providers.{}]\nbase_url = {base_url:?}\nenabled = true\n",
            provider.id()
        ));
        self.providers.push(provider.id());
    }

    /// Write the workflow (and optional `workflow.toml`) this case runs.
    pub(crate) fn workflow(&self, dot: &str, workflow_toml: Option<&str>) -> PathBuf {
        let dir = self.root.join("workflow");
        let path = dir.join("case.fabro");
        fs::write(&path, dot).expect("write the workflow");
        if let Some(toml) = workflow_toml {
            fs::write(dir.join("workflow.toml"), toml).expect("write workflow.toml");
        }
        path
    }

    /// The host workspace of the root invocation's first scope.
    pub(crate) fn workspace(&self) -> PathBuf {
        self.run_dir
            .join("scopes")
            .join("invocation-0-scope-0")
            .join("work")
    }

    /// Run `petri run <args…> <workflow>` to completion, or kill it at the
    /// deadline.
    pub(crate) async fn run(&self, workflow: &Path, args: &[&str]) -> Finished {
        self.run_with(workflow, args, Launch::default()).await
    }

    /// [`Case::run`] with piped terminal input and an optional interrupt.
    pub(crate) async fn run_with(
        &self,
        workflow: &Path,
        args: &[&str],
        launch: Launch,
    ) -> Finished {
        self.launch(Target::Run(workflow), args, launch).await
    }

    /// `petri resume --run-dir <run dir> <args…>` on this case's run
    /// directory, with the same isolated environment as [`Case::run`].
    pub(crate) async fn resume_with(&self, args: &[&str], launch: Launch) -> Finished {
        self.launch(Target::Resume, args, launch).await
    }

    async fn launch(&self, target: Target<'_>, args: &[&str], launch: Launch) -> Finished {
        // One file per layer: a layer is a whole catalog document with its
        // own `schema_version`, and `PETRI_LLM_CATALOG` takes a path list.
        let layers: Vec<PathBuf> = self
            .layers
            .iter()
            .enumerate()
            .map(|(index, layer)| {
                let path = self.root.join(format!("catalog-{index}.toml"));
                fs::write(&path, layer).expect("write the catalog layer");
                path
            })
            .collect();
        let catalog = env::join_paths(&layers).expect("catalog paths");
        let path = launch
            .path
            .clone()
            .or_else(|| env::var("PATH").ok())
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        let deadline = launch.deadline.unwrap_or(RUN_DEADLINE);
        let mut command = self.command(&path);
        if let Some(cwd) = &launch.cwd {
            command.current_dir(cwd);
        }
        command
            .env("PETRI_LLM_CATALOG", &catalog)
            .env("PETRI_LLM_PROVIDERS", self.providers.join(","));
        for (key, value) in &launch.env {
            command.env(key, value);
        }
        for provider in &self.providers {
            let variable = match *provider {
                "openai" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "openrouter" => "OPENROUTER_API_KEY",
                other => panic!("no credential variable for provider `{other}`"),
            };
            command.env(variable, &self.credential);
        }
        command.arg(match target {
            Target::Run(_) => "run",
            Target::Resume => "resume",
        });
        command
            .args(self.backend_args())
            .args(args)
            .arg("--run-dir")
            .arg(&self.run_dir);
        if let Target::Run(workflow) = target {
            command.arg(workflow);
        }
        command
            .stdin(if launch.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().expect("petri starts");
        let pid = child.id().expect("a running child has a pid");
        if let Some(text) = launch.stdin {
            let mut stdin = child.stdin.take().expect("piped stdin");
            tokio::spawn(async move {
                let _ = stdin.write_all(text.as_bytes()).await;
                // Kept open: EOF on the terminal is its own failure mode, and
                // a case that wants it passes an empty script and closes.
                if launch.close_stdin {
                    drop(stdin);
                } else {
                    sleep(RUN_DEADLINE).await;
                }
            });
        }
        let appends = launch.append_when;
        let appender = (!appends.is_empty()).then(|| {
            tokio::spawn(async move {
                for (marker, file, text, delay) in appends {
                    let deadline = Instant::now() + RUN_DEADLINE;
                    while !marker.exists() {
                        assert!(
                            Instant::now() < deadline,
                            "the append marker {} never appeared",
                            marker.display()
                        );
                        sleep(Duration::from_millis(50)).await;
                    }
                    sleep(delay).await;
                    let mut handle = fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&file)
                        .expect("open the control file");
                    handle
                        .write_all(text.as_bytes())
                        .expect("append to the control file");
                }
            })
        });
        let container_marker = launch
            .interrupt_when_container_file
            .map(|file| (self.run_dir.clone(), file));
        let stderr_marker = launch
            .interrupt_when_stderr
            .as_ref()
            .map(|_| self.root.join("interrupt-on-stderr"));
        let interrupt_when = launch.interrupt_when.or_else(|| stderr_marker.clone());
        let interrupt = (interrupt_when.is_some() || container_marker.is_some()).then(|| {
            let marker = interrupt_when;
            tokio::spawn(async move {
                let deadline = Instant::now() + RUN_DEADLINE;
                loop {
                    let appeared = match (&marker, &container_marker) {
                        (Some(marker), _) => marker.exists(),
                        (None, Some((run_dir, file))) => container_has(run_dir, file).await,
                        (None, None) => true,
                    };
                    if appeared {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "the interrupt marker {marker:?} {container_marker:?} never appeared"
                    );
                    sleep(Duration::from_millis(50)).await;
                }
                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .args(["-INT", &pid.to_string()])
                        .stdin(Stdio::null())
                        .status()
                        .await;
                }
            })
        });
        // SIGKILL the `petri` process itself, by pid, once a marker exists:
        // the crash a resume recovers from. Only the child this launch
        // spawned is signalled; its sandbox plugin and scripts are left to
        // notice the closed transport on their own.
        let killer = launch.kill_when.map(|(marker, delay)| {
            tokio::spawn(async move {
                let deadline = Instant::now() + RUN_DEADLINE;
                while !marker.exists() {
                    assert!(
                        Instant::now() < deadline,
                        "the kill marker {} never appeared",
                        marker.display()
                    );
                    sleep(Duration::from_millis(50)).await;
                }
                sleep(delay).await;
                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .stdin(Stdio::null())
                        .status()
                        .await;
                }
            })
        });
        let mut stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        // Every needle the case watches for, the interrupt's among them: each
        // one creates its marker file the first time a stderr line carries it.
        let stderr_watches: Vec<(String, PathBuf)> = launch
            .interrupt_when_stderr
            .clone()
            .zip(stderr_marker.clone())
            .into_iter()
            .chain(launch.mark_when_stderr.clone())
            .collect();
        let drain = async {
            let mut out = Vec::new();
            let read_out = stdout.read_to_end(&mut out);
            // stderr is read line by line so a watched line can fire the
            // interrupt while the run is still live.
            let read_err = async {
                let mut err = Vec::new();
                let mut lines = BufReader::new(stderr).split(b'\n');
                while let Ok(Some(line)) = lines.next_segment().await {
                    let text = String::from_utf8_lossy(&line);
                    for (needle, marker) in &stderr_watches {
                        if !marker.exists() && text.contains(needle.as_str()) {
                            let _ = fs::write(marker, "");
                        }
                    }
                    err.extend_from_slice(&line);
                    err.push(b'\n');
                }
                err
            };
            let (a, err) = tokio::join!(read_out, read_err);
            a.expect("read stdout");
            (out, err)
        };
        let waited = timeout(deadline, async {
            let ((out, err), status) = tokio::join!(drain, child.wait());
            (out, err, status.expect("wait for petri"))
        })
        .await;
        if let Some(interrupt) = interrupt {
            interrupt.abort();
        }
        if let Some(killer) = killer {
            killer.abort();
        }
        if let Some(appender) = appender {
            appender.abort();
        }
        let (stdout, stderr, status, timed_out) = if let Ok((out, err, status)) = waited {
            (out, err, Some(status), false)
        } else {
            kill_group(pid).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            (Vec::new(), Vec::new(), None, true)
        };
        Finished {
            case_root: self.root.clone(),
            run_dir: self.run_dir.clone(),
            plugin_link: self.plugin_link.clone(),
            code: status.and_then(|status| status.code()),
            timed_out,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }
}

#[cfg(unix)]
fn link_plugin(plugin: &Path, link: &Path) {
    use std::os::unix::fs::symlink;
    symlink(plugin, link).expect("link the plugin");
}

#[cfg(not(unix))]
fn link_plugin(plugin: &Path, link: &Path) {
    fs::copy(plugin, link).expect("copy the plugin");
}

/// How a launch differs from the plain `run`.
#[derive(Default)]
pub(crate) struct Launch {
    /// Text written to the child's stdin, for `--interactive`.
    pub(crate) stdin: Option<String>,
    /// Close stdin after writing it (EOF), instead of holding it open.
    pub(crate) close_stdin: bool,
    /// Send SIGINT once this file exists: how a case cancels a run the way a
    /// person at the terminal does.
    pub(crate) interrupt_when: Option<PathBuf>,
    /// Send SIGINT once this file exists inside the run's container sandbox
    /// (lease 0): the marker for a workspace the host cannot see.
    pub(crate) interrupt_when_container_file: Option<String>,
    /// The child's `PATH`. Defaults to the harness's own.
    pub(crate) path: Option<String>,
    /// Append lines to a file once a marker exists: how a case drives
    /// `--control` the way a person at the terminal would. Entries run in
    /// order on one task; each is (marker, file, text, delay after the
    /// previous entry, or after the marker for the first).
    pub(crate) append_when: Vec<(PathBuf, PathBuf, String, Duration)>,
    /// Extra environment variables for the child: what a case hands the run
    /// beyond the isolated baseline, such as a `PETRI_SECRET_*` value.
    pub(crate) env: Vec<(String, String)>,
    /// This launch's deadline, when the case declares one; else
    /// [`RUN_DEADLINE`].
    pub(crate) deadline: Option<Duration>,
    /// The working directory of the `petri` process. Defaults to the
    /// harness's own.
    pub(crate) cwd: Option<PathBuf>,
    /// Send SIGINT once a stderr line contains this text: the cancel a
    /// person sends when they see a gate waiting.
    pub(crate) interrupt_when_stderr: Option<String>,
    /// Create each file once a stderr line contains its text: the marker an
    /// [`Launch::append_when`] entry waits on, so a case orders its controls
    /// by what the run has reached instead of by the clock.
    pub(crate) mark_when_stderr: Vec<(String, PathBuf)>,
    /// SIGKILL the `petri` process, by pid, this long after the marker file
    /// exists: the crash `petri resume` recovers from. The launch then ends
    /// with no exit code.
    pub(crate) kill_when: Option<(PathBuf, Duration)>,
}

/// Which command a launch runs.
enum Target<'a> {
    /// `petri run … <workflow>`.
    Run(&'a Path),
    /// `petri resume` on the case's run directory.
    Resume,
}

/// A `PATH` with an empty directory in front and only the system binaries
/// behind it, so no `fabro` resolves. `None` when a `fabro` lives in one of
/// those system directories, which the harness cannot sanitize.
pub(crate) fn sanitized_path(root: &Path) -> Option<String> {
    let empty = root.join("empty-bin");
    fs::create_dir_all(&empty).expect("the empty bin dir");
    let system = ["/usr/bin", "/bin"];
    if system.iter().any(|d| Path::new(d).join("fabro").exists()) {
        return None;
    }
    Some(format!("{}:{}", empty.display(), system.join(":")))
}

/// Whether the run's first container sandbox holds `file`: the run id is
/// recorded under the run dir once the sandbox exists, and `docker cp` reads
/// from a running or stopped container.
async fn container_has(run_dir: &Path, file: &str) -> bool {
    if !run_dir.join("run.json").exists() {
        return false;
    }
    let name = testkit::sandbox_name(run_dir, 0);
    Command::new("docker")
        .args(["cp", &format!("{name}:{file}"), "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Kill a child's whole process group, then reap what `kill_on_drop` cannot.
async fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = pid;
}

/// What a launch left behind.
pub(crate) struct Finished {
    pub(crate) case_root: PathBuf,
    pub(crate) run_dir:   PathBuf,
    plugin_link:          PathBuf,
    /// The exit code, or `None` when the child was killed (deadline or
    /// signal).
    pub(crate) code:      Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) stdout:    String,
    pub(crate) stderr:    String,
}

impl Finished {
    /// Fail the test unless the run exited with `code`.
    pub(crate) fn assert_code(&self, code: i32) {
        assert!(!self.timed_out, "petri run exceeded its deadline");
        assert_eq!(
            self.code,
            Some(code),
            "exit code\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
    }

    /// The final run status line, `run: success` and the like.
    pub(crate) fn status_line(&self) -> Option<&str> {
        self.stderr
            .lines()
            .find(|line| line.starts_with("run: "))
            .map(|line| &line[5..])
    }

    /// The interview receipt the run wrote.
    pub(crate) fn receipt(&self) -> Value {
        let path = self.run_dir.join("interviews.json");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&text).expect("interviews.json is JSON")
    }

    /// The `petri inspect --json` document for this run.
    pub(crate) fn inspect(&self) -> Value {
        inspect::inspect(&self.run_dir)
    }

    /// The final run context of the root invocation, read through
    /// `petri inspect`.
    pub(crate) fn final_context(&self) -> BTreeMap<String, Value> {
        let document = self.inspect();
        inspect::root_context(&document)
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .expect("the root invocation finished with a context")
    }

    /// The node names that finished, in completion order, from the CLI's
    /// own summary lines (`  <status> <node>`).
    pub(crate) fn finished_nodes(&self) -> Vec<(String, String)> {
        self.stderr
            .lines()
            .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
            .filter_map(|line| {
                let mut parts = line.trim().splitn(2, ' ');
                Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
            })
            .collect()
    }

    /// The retained workspace paths the run reported.
    pub(crate) fn reported_workspaces(&self) -> Vec<PathBuf> {
        self.stderr
            .lines()
            .filter_map(|line| line.strip_prefix("workspace: "))
            .map(|path| PathBuf::from(path.trim()))
            .collect()
    }

    /// Every echoed step line, `[node#firing] text` or
    /// `[invocation-N/node#firing] text`, as (node, text). The invocation
    /// prefix is dropped here; [`Finished::echoed_tags`] keeps it.
    pub(crate) fn echoed(&self) -> Vec<(String, String)> {
        self.echoed_tags()
            .into_iter()
            .map(|(tag, text)| {
                let node = tag.rsplit('/').next().unwrap_or(&tag);
                let node = node.split('#').next().unwrap_or(node);
                (node.to_owned(), text)
            })
            .collect()
    }

    /// Every echoed step line as (tag, text), the tag as printed:
    /// `node#firing`, or `invocation-N/node#firing` for a branch.
    pub(crate) fn echoed_tags(&self) -> Vec<(String, String)> {
        self.stderr
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                let end = rest.find("] ")?;
                Some((rest[..end].to_owned(), rest[end + 2..].to_owned()))
            })
            .collect()
    }

    /// The reported container sandboxes: `(workspace, provider, sandbox id)`
    /// from `workspace: <ws> on <provider> sandbox <id> (delete with ...)`.
    pub(crate) fn reported_sandboxes(&self) -> Vec<(String, String, String)> {
        self.stderr
            .lines()
            .filter_map(|line| line.strip_prefix("workspace: "))
            .filter_map(|rest| {
                let (workspace, rest) = rest.split_once(" on ")?;
                let (provider, rest) = rest.split_once(" sandbox ")?;
                let (id, _) = rest.split_once(" (")?;
                Some((workspace.to_owned(), provider.to_owned(), id.to_owned()))
            })
            .collect()
    }

    /// Fail the test when a process launched for this case is still alive:
    /// the plugin (by its per-case path) or anything naming the run dir.
    pub(crate) async fn assert_no_leaked_processes(&self) {
        for needle in [
            self.plugin_link.to_string_lossy().into_owned(),
            self.run_dir.to_string_lossy().into_owned(),
        ] {
            let output = Command::new("pgrep")
                .args(["-f", &needle])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("pgrep runs");
            let pids = String::from_utf8_lossy(&output.stdout);
            let pids: Vec<&str> = pids
                .lines()
                .map(str::trim)
                .filter(|pid| !pid.is_empty())
                .collect();
            assert!(
                pids.is_empty(),
                "processes still running for `{needle}`: {pids:?}"
            );
        }
    }
}
