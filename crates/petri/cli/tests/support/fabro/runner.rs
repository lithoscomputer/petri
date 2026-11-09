//! Run one scenario file end to end: stage its fixture repository with
//! real local Git remotes, start the twins its services script, write the
//! interview script, apply its controls, run the shipped binary, and apply
//! every row of its expected observations.
//!
//! The runner reads a finished run only through `petri inspect --json`, the
//! retained workspace, the twins' request logs, and the interview receipt.
//! It never patches a bundle: the fixture repository holds the bundle's
//! files byte for byte, and only the declared environment (`fixture.env`,
//! `fixture.bin`, `fixture.settings`) and fixture data are added.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{env, fs};

use serde_json::{Map, Value, json};
use tokio::time::{Instant, sleep};

use super::launch::{Case, Finished, Launch, sanitized_path};
use super::scenario::{Agent, Backend, Bindings, CellRecord, FileSpec, Scenario, matches};
use super::twins::{Provider, Twin, model, requested_effort, wire_model};
use super::{inspect, interview};

/// How one cell ended, for the caller's own assertions after the runner's.
pub(crate) struct Ran {
    pub(crate) case:      Case,
    pub(crate) finished:  Finished,
    pub(crate) scenario:  Scenario,
    pub(crate) bindings:  Bindings,
    pub(crate) workspace: PathBuf,
    pub(crate) twins:     Vec<Twin>,
    /// The fixture remotes by name.
    pub(crate) remotes:   BTreeMap<String, PathBuf>,
}

/// Run scenario `id` in `backend`/`agent` mode and record the cell.
///
/// # Panics
///
/// Panics with the first failed expectation. The cell's coverage record
/// stays `failed` then; it becomes `passed` only when every row holds.
pub(crate) async fn run_cell(cell: &str, id: &str, backend: Backend, agent: Agent) -> Option<Ran> {
    let record = CellRecord::start(cell);
    let scenario = Scenario::from_id(id).unwrap_or_else(|error| panic!("{error}"));
    if backend == Backend::Docker && !testkit::is_docker_ready().await {
        drop(record);
        CellRecord::skip(cell, "no Docker daemon");
        return None;
    }
    let ran = run(scenario, backend, agent, cell).await;
    // A Fabro run keeps its workspace, so a container cell leaves a sandbox
    // behind. The workspace was copied out before the assertions ran, so the
    // cell prunes its own sandbox and nothing else: three agents share this
    // daemon.
    if backend == Backend::Docker && !ran.scenario.expect.lifecycle.prune_removes_sandbox {
        let (code, stderr) = ran.case.prune().await;
        assert_eq!(code, Some(0), "the cell prunes its own sandbox: {stderr}");
    }
    record.pass();
    Some(ran)
}

async fn run(scenario: Scenario, backend: Backend, agent: Agent, cell: &str) -> Ran {
    let label = cell.replace(['/', '@'], "-");
    let mut case = Case::new(&label);
    if backend == Backend::Docker {
        case = case.docker();
    }
    let mut bindings = Bindings::default();
    bindings.bind("credential", case.credential.clone());
    if let Some(provider) = provider_of(agent) {
        bindings.bind("model", model(provider));
    }

    // ── The fixture repository and its remotes ──────────────────────────
    let fixture = case.root.join("fixture");
    let repo = fixture.join("repo");
    fs::create_dir_all(&repo).expect("fixture repository");
    // Every graph of the scenario's directory lands flat at the repository
    // root, so a parent's `stack.child_workflow` names its child by file
    // name; a `workflow.toml` beside them comes along.
    let source = scenario.dir.join(scenario.entry_point());
    let graphs = source.parent().unwrap_or(&scenario.dir).to_path_buf();
    for entry in fs::read_dir(&graphs).expect("read the graph directory") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let name = entry.file_name();
        let is_graph = path
            .extension()
            .is_some_and(|extension| extension == "fabro" || extension == "dot");
        if is_graph || name == "workflow.toml" {
            fs::copy(&path, repo.join(&name)).expect("copy the inline graph");
        }
    }
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "petri-blackbox"]);
    git(&repo, &[
        "config",
        "user.email",
        "petri-blackbox@example.invalid",
    ]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    let commits = if scenario.fixture.commits.is_empty() {
        vec![super::scenario::Commit {
            message: "fixture".to_owned(),
            write:   BTreeMap::new(),
            remove:  Vec::new(),
        }]
    } else {
        scenario.fixture.commits.clone()
    };
    for (index, commit) in commits.iter().enumerate() {
        for (path, spec) in &commit.write {
            write_spec(&repo.join(path), spec, &scenario.dir, &bindings);
        }
        for path in &commit.remove {
            let target = repo.join(path);
            if target.is_dir() {
                fs::remove_dir_all(&target).expect("remove fixture directory");
            } else {
                fs::remove_file(&target).expect("remove fixture file");
            }
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &commit.message,
        ]);
        let sha = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        bindings.bind(&format!("commit:{index}"), sha);
    }
    bindings.bind("fixture", repo.to_string_lossy().into_owned());
    let mut remotes = BTreeMap::new();
    for (name, remote) in &scenario.fixture.remotes {
        let path = fixture.join(format!("{name}.git"));
        assert!(remote.bare, "only bare remotes are supported");
        git(&fixture, &[
            "clone",
            "-q",
            "--bare",
            &repo.to_string_lossy(),
            &path.to_string_lossy(),
        ]);
        git(&path, &["config", "uploadpack.allowFilter", "true"]);
        git(&path, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        for branch in &remote.branches {
            if branch != "main" {
                git(&path, &["branch", branch, "main"]);
            }
        }
        git(&repo, &["remote", "add", name, &path.to_string_lossy()]);
        git(&repo, &["fetch", "-q", name]);
        bindings.bind(
            &format!("remote:{name}"),
            path.to_string_lossy().into_owned(),
        );
        remotes.insert(name.clone(), path);
    }

    // ── The declared environment ────────────────────────────────────────
    let mut path_prefix = Vec::new();
    if backend == Backend::Host && !scenario.fixture.python_modules.is_empty() {
        // The bundle's image installs these modules into its `python3`, and
        // the pinned runner image carries them, so a Docker scope needs
        // nothing from the host. On the host, the interpreter that has them
        // stands first on PATH and the directories they live in ride
        // `PYTHONPATH`, because the run's isolated `HOME` hides a user
        // site-packages directory.
        let Some((python, site_dirs)) =
            python_with(&scenario.fixture.python_modules, &case.root.join("home"))
        else {
            let reason = format!(
                "no python3 on PATH imports {:?}; install the bundle's pinned requirements",
                scenario.fixture.python_modules
            );
            drop(case);
            CellRecord::skip(cell, &reason);
            panic!("skipping: {reason}");
        };
        // A wrapper, not a link: the sandbox plugin is launched with a
        // curated environment (`PATH`, `HOME`), so `PYTHONPATH` has to be
        // set by the `python3` the steps resolve on that PATH. Bytecode
        // caches stay off: the review helpers digest every untracked file
        // of the tree and refuse to publish when one appeared.
        let bin = fixture.join("python-bin");
        fs::create_dir_all(&bin).expect("python bin");
        let joined = env::join_paths(&site_dirs).expect("site paths");
        let wrapper = format!(
            "#!/bin/sh\nexport PYTHONPATH=\"{}${{PYTHONPATH:+:$PYTHONPATH}}\"\nexport \
             PYTHONDONTWRITEBYTECODE=1\nexec \"{}\" \"$@\"\n",
            joined.to_string_lossy(),
            python.display()
        );
        write_spec(
            &bin.join("python3"),
            &FileSpec {
                text: Some(wrapper),
                file: None,
                mode: Some("0755".to_owned()),
            },
            &scenario.dir,
            &Bindings::default(),
        );
        path_prefix.push(bin);
    }
    if !scenario.fixture.bin.is_empty() {
        let bin = fixture.join("bin");
        fs::create_dir_all(&bin).expect("fixture bin");
        for (name, spec) in &scenario.fixture.bin {
            write_spec(&bin.join(name), spec, &scenario.dir, &bindings);
        }
        path_prefix.push(bin);
    }
    if let Some(settings) = &scenario.fixture.settings {
        // The scenario's layer, with the `local` environment every case
        // selects appended after it.
        let home = case.root.join("home").join(".fabro");
        fs::create_dir_all(&home).expect("fabro home");
        let text = format!(
            "{}\n{}",
            bindings.text(settings),
            super::launch::LOCAL_ENVIRONMENT_SETTINGS
        );
        fs::write(home.join("settings.toml"), text).expect("settings.toml");
    }

    // ── Twins ───────────────────────────────────────────────────────────
    let mut providers: Vec<Provider> = Vec::new();
    if let Some(provider) = provider_of(agent) {
        providers.push(provider);
    }
    for (name, scripts) in [
        ("openai", &scenario.services.openai),
        ("anthropic", &scenario.services.anthropic),
        ("openrouter", &scenario.services.openrouter),
    ] {
        let provider = provider_named(name);
        if !scripts.is_empty() && !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    assert!(
        scenario.services.http.is_empty(),
        "scripted HTTP fixture services are not implemented yet"
    );
    let mut twins = Vec::new();
    for provider in providers {
        let scripts = match provider {
            Provider::OpenAi => &scenario.services.openai,
            Provider::Anthropic => &scenario.services.anthropic,
            Provider::OpenRouter => &scenario.services.openrouter,
        };
        let scripts: Vec<Value> = scripts
            .iter()
            .map(|script| twin_script(provider, &case.credential, &bindings.value(script)))
            .collect();
        let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
        case.redirect(&twin);
        twins.push(twin);
    }

    // ── Launch ──────────────────────────────────────────────────────────
    let entry = scenario.entry_point();
    let entry = Path::new(entry).file_name().map_or_else(
        || entry.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let workflow = repo.join(&entry);
    assert!(
        workflow.is_file(),
        "entry point {} exists",
        workflow.display()
    );
    let mut args: Vec<String> = Vec::new();
    // The cell picks where the run executes, as the pinned Fabro's harness
    // does on every `fabro run` (`--environment local`, declared in the
    // case's settings layer); a bundle's own `[run.environment]` may name
    // Daytona. A Docker cell keeps `--backend docker` over the provider.
    args.push("--environment".into());
    args.push("local".into());
    if backend == Backend::Host {
        args.push("--backend".into());
        args.push("host".into());
    }
    for (key, value) in &scenario.inputs {
        let value = bindings.value(value);
        let rendered = match value {
            Value::String(text) => text,
            other => other.to_string(),
        };
        args.push("--input".into());
        args.push(format!("{key}={rendered}"));
    }
    let script = (!scenario.interviews.is_empty()).then(|| {
        let entries: Vec<Value> = scenario
            .interviews
            .iter()
            .map(|e| bindings.value(e))
            .collect();
        interview::write(&case.root, "scenario", &entries)
    });
    if let Some(script) = &script {
        args.push("--interview-script".into());
        args.push(script.to_string_lossy().into_owned());
    }
    let mut launch = Launch {
        deadline: Some(Duration::from_millis(scenario.bounds.deadline_ms)),
        cwd: Some(repo.clone()),
        ..Launch::default()
    };
    for (key, value) in &scenario.fixture.env {
        launch.env.push((key.clone(), bindings.text(value)));
    }
    let base_path = if scenario.controls.path_without_fabro {
        sanitized_path(&case.root).expect("a sanitized PATH")
    } else {
        env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    };
    if path_prefix.is_empty() {
        launch.path = Some(base_path);
    } else {
        let mut parts: Vec<String> = path_prefix
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        parts.push(base_path);
        launch.path = Some(parts.join(":"));
    }
    if let Some(stdin) = &scenario.controls.stdin {
        launch.stdin = Some(stdin.text.clone());
        launch.close_stdin = stdin.close;
        args.push("--interactive".into());
    }
    let workspace = case.workspace();
    let control_file = case.root.join("controls.txt");
    if !scenario.controls.control_lines.is_empty() {
        fs::write(&control_file, "").expect("control file");
        args.push("--control".into());
        args.push(control_file.to_string_lossy().into_owned());
        for line in &scenario.controls.control_lines {
            launch.append_when.push((
                workspace.join(&line.after_file),
                control_file.clone(),
                format!("{}\n", line.line),
                Duration::from_millis(line.delay_ms),
            ));
        }
    }
    let mut interrupt_marker = None;
    let mut watcher = None;
    if let Some(when) = &scenario.controls.interrupt_when {
        if let Some(file) = &when.file {
            // A workspace path. On the host the harness reads the workspace
            // directly; in a container the workspace is `/workspace` inside
            // the sandbox, read through `docker cp`, so one scenario file
            // serves both backends.
            match backend {
                Backend::Host => launch.interrupt_when = Some(workspace.join(file)),
                Backend::Docker => {
                    launch.interrupt_when_container_file = Some(format!("/workspace/{file}"));
                }
            }
        }
        if let Some(file) = &when.container_file {
            launch.interrupt_when_container_file = Some(file.clone());
        }
        if let Some(text) = &when.stderr {
            launch.interrupt_when_stderr = Some(bindings.text(text));
        }
        if let Some(request) = &when.request {
            // A marker the watcher touches once the twin consumed the named
            // scenario: the interrupt is tied to an observed request.
            let marker = case.root.join("interrupt-on-request");
            launch.interrupt_when = Some(marker.clone());
            let logs: Vec<PathBuf> = twins.iter().map(Twin::log_path).collect();
            let wanted = request.clone();
            let deadline = Instant::now() + Duration::from_millis(scenario.bounds.deadline_ms);
            let touch = marker.clone();
            watcher = Some(tokio::spawn(async move {
                loop {
                    let consumed = logs.iter().any(|log| {
                        fs::read_to_string(log)
                            .unwrap_or_default()
                            .lines()
                            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                            .any(|record| record["scenario_id"] == wanted.as_str())
                    });
                    if consumed {
                        fs::write(&touch, "").expect("interrupt marker");
                        return;
                    }
                    if Instant::now() > deadline {
                        return;
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            }));
            interrupt_marker = Some(marker);
        }
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let finished = case.run_with(&workflow, &args, launch).await;
    if let Some(watcher) = watcher {
        watcher.abort();
    }
    let interrupted = interrupt_marker
        .as_ref()
        .is_some_and(|marker| marker.exists())
        || scenario
            .controls
            .interrupt_when
            .as_ref()
            .is_some_and(|when| {
                when.stderr.is_some() && case.root.join("interrupt-on-stderr").exists()
            })
        || scenario
            .controls
            .interrupt_when
            .as_ref()
            .is_some_and(|when| when.file.is_some() || when.container_file.is_some());
    let requests_at_interrupt: Option<usize> =
        interrupted.then(|| twins.iter().map(|t| t.request_log().len()).sum());

    // ── The workspace, on the host or copied out of the container ───────
    let workspace = if backend == Backend::Docker {
        let copy = case.root.join("workspace-copy");
        let name = testkit::sandbox_name(&case.run_dir, 0);
        let status = Command::new("docker")
            .args([
                "cp",
                &format!("{name}:/workspace/."),
                &copy.to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("docker cp runs");
        assert!(
            status.status.success(),
            "docker cp of the workspace failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        copy
    } else {
        workspace
    };

    let ran = Ran {
        case,
        finished,
        scenario,
        bindings,
        workspace,
        twins,
        remotes,
    };
    expect(&ran, requests_at_interrupt).await;
    ran
}

// ── Expectations ────────────────────────────────────────────────────────────

async fn expect(ran: &Ran, requests_at_interrupt: Option<usize>) {
    let scenario = &ran.scenario;
    let bindings = &ran.bindings;
    let finished = &ran.finished;
    let expect = &scenario.expect;
    let context_for_errors = || {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            finished.stdout, finished.stderr
        )
    };

    // Process and workflow.
    assert!(
        !finished.timed_out,
        "the run exceeded its {} ms deadline\n{}",
        scenario.bounds.deadline_ms,
        context_for_errors()
    );
    assert_eq!(
        finished.code,
        Some(expect.process.exit_code),
        "exit code\n{}",
        context_for_errors()
    );
    let document = finished.inspect();
    assert_eq!(
        document["status"].as_str(),
        Some(expect.process.status.as_str()),
        "run status\n{}",
        context_for_errors()
    );
    let history = root_history(&document);
    // A node the cancel reached before it ran has a `cancelled` record; it
    // did not run, so it counts for neither list.
    let visited: Vec<String> = history
        .iter()
        .filter(|entry| entry["status"] != "cancelled")
        .filter_map(|entry| entry["node"].as_str().map(str::to_owned))
        .collect();
    for node in &expect.process.required_nodes {
        assert!(
            visited.iter().any(|v| base_name(v) == node || v == node),
            "required node `{node}` never finished; finished: {visited:?}\n{}",
            context_for_errors()
        );
    }
    for node in &expect.process.forbidden_nodes {
        assert!(
            !visited.iter().any(|v| base_name(v) == node || v == node),
            "forbidden node `{node}` finished; finished: {visited:?}"
        );
    }
    for (node, count) in &expect.process.visits {
        let seen = visited
            .iter()
            .filter(|v| base_name(v) == node || *v == node)
            .count() as u64;
        assert_eq!(seen, *count, "visits of `{node}`; finished: {visited:?}");
    }
    if !expect.process.attempts.is_empty() {
        let nodes = inspect::root_nodes(&document);
        for (node, count) in &expect.process.attempts {
            assert_eq!(
                nodes[node.as_str()]["attempts"].as_u64(),
                Some(*count),
                "attempts of `{node}`: {:#}",
                nodes[node.as_str()]
            );
        }
    }
    for text in &expect.process.stderr_contains {
        let text = bindings.text(text);
        assert!(
            finished.stderr.contains(&text),
            "stderr does not contain `{text}`\n{}",
            context_for_errors()
        );
    }

    // Final context.
    let context = inspect::root_context(&document)
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (key, value) in &expect.context.exact {
        let value = bindings.value(value);
        matches(&value, context.get(key))
            .unwrap_or_else(|error| panic!("context `{key}`: {error}\ncontext: {context:#?}"));
    }
    for key in &expect.context.absent {
        assert!(
            !context.contains_key(key),
            "context key `{key}` is present: {:?}",
            context.get(key)
        );
    }
    if expect.context.complete {
        let unexpected: Vec<&String> = context
            .keys()
            .filter(|key| {
                !expect.context.exact.contains_key(*key)
                    && !expect.context.extra_allowed.contains(key)
            })
            .collect();
        assert!(
            unexpected.is_empty(),
            "context keys not declared: {unexpected:?}\ncontext: {context:#?}"
        );
    }

    // Files.
    for file in &expect.files {
        let pattern = bindings.text(&file.path);
        let found = glob_one(&ran.workspace, &pattern);
        if file.absent {
            assert!(found.is_none(), "file `{pattern}` exists: {found:?}");
            continue;
        }
        let path = found.unwrap_or_else(|| {
            panic!(
                "file `{pattern}` is missing under {}",
                ran.workspace.display()
            )
        });
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if let Some(text) = &file.text {
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                bindings.text(text),
                "contents of `{pattern}`"
            );
        }
        if let Some(reference) = &file.file {
            let expected = fs::read(scenario.dir.join(reference)).expect("expected file");
            assert_eq!(
                bytes, expected,
                "contents of `{pattern}` differ from {reference}"
            );
        }
        for needle in &file.contains {
            let needle = bindings.text(needle);
            assert!(
                String::from_utf8_lossy(&bytes).contains(&needle),
                "`{pattern}` does not contain `{needle}`:\n{}",
                String::from_utf8_lossy(&bytes)
            );
        }
        if let Some(json) = &file.json {
            let actual: Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("`{pattern}` is not JSON: {e}"));
            matches(&bindings.value(json), Some(&actual))
                .unwrap_or_else(|error| panic!("`{pattern}`: {error}\n{actual:#}"));
        }
        if let Some(lines) = file.lines {
            assert_eq!(
                String::from_utf8_lossy(&bytes).lines().count(),
                lines,
                "line count of `{pattern}`"
            );
        }
        #[cfg(unix)]
        if let Some(mode) = &file.mode {
            use std::os::unix::fs::PermissionsExt as _;
            let actual = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            let wanted = u32::from_str_radix(mode.trim_start_matches('0'), 8).expect("octal mode");
            assert_eq!(actual, wanted, "mode of `{pattern}`");
        }
    }

    // Side effects: Git refs.
    for git_expect in &expect.side_effects.git {
        let repo = match git_expect.repo.as_str() {
            "workspace" => ran.workspace.clone(),
            name => ran
                .remotes
                .get(name)
                .cloned()
                .unwrap_or_else(|| panic!("no fixture remote `{name}`")),
        };
        let reference = bindings.text(&git_expect.reference);
        let resolved = git_try(&repo, &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{reference}^{{commit}}"),
        ]);
        if git_expect.absent {
            assert!(
                resolved.is_none(),
                "`{reference}` exists in {}",
                git_expect.repo
            );
            continue;
        }
        let sha = resolved
            .unwrap_or_else(|| panic!("`{reference}` does not resolve in {}", git_expect.repo))
            .trim()
            .to_owned();
        if let Some(is) = &git_expect.is {
            assert_eq!(
                sha,
                bindings.text(is),
                "`{reference}` in {}",
                git_expect.repo
            );
        }
        if let Some(from) = &git_expect.advanced_from {
            let from = bindings.text(from);
            let count = git(&repo, &["rev-list", "--count", &format!("{from}..{sha}")])
                .trim()
                .parse::<u64>()
                .expect("a count");
            assert_eq!(
                count, git_expect.commits,
                "`{reference}` is {count} commit(s) ahead of {from}, expected {}",
                git_expect.commits
            );
        }
        if let Some(message) = &git_expect.message {
            let subject = git(&repo, &["log", "-1", "--format=%s", &sha]);
            assert_eq!(
                subject.trim(),
                bindings.text(message),
                "subject of `{reference}`"
            );
        }
    }
    assert!(
        expect.side_effects.http.is_empty(),
        "HTTP side effects are not implemented yet"
    );

    // Provider requests.
    for (name, provider_expect) in &expect.providers {
        let provider = provider_named(name);
        let twin = ran
            .twins
            .iter()
            .find(|twin| twin.provider == provider)
            .unwrap_or_else(|| panic!("no twin for `{name}` ran"));
        if let Some(consumed) = &provider_expect.consumed {
            let actual = Value::Array(twin.consumed().into_iter().map(Value::String).collect());
            matches(&bindings.value(consumed), Some(&actual))
                .unwrap_or_else(|error| panic!("`{name}` consumed: {error}"));
        }
        if let Some(unmatched) = provider_expect.unmatched {
            assert_eq!(twin.unmatched(), unmatched, "`{name}` unmatched requests");
        }
        let requests = twin.requests_for(&ran.case.credential);
        if let Some(count) = provider_expect.requests {
            assert_eq!(requests.len(), count, "`{name}` requests: {requests:#?}");
        }
        if let Some(model_expected) = &provider_expect.model {
            let wanted = bindings.text(model_expected);
            let wanted = if wanted == model(provider) {
                wire_model(provider).to_owned()
            } else {
                wanted
            };
            for request in &requests {
                assert_eq!(
                    request["model"],
                    json!(wanted),
                    "`{name}` model on the wire"
                );
            }
        }
        if let Some(effort) = &provider_expect.effort {
            for request in &requests {
                assert_eq!(
                    requested_effort(provider, request),
                    Some(effort.as_str()),
                    "`{name}` reasoning effort"
                );
            }
        }
        for contains in &provider_expect.contains {
            let request = requests
                .get(contains.request)
                .unwrap_or_else(|| panic!("`{name}` request {} does not exist", contains.request));
            let text = serde_json::to_string(request).expect("request");
            let needle = bindings.text(&contains.text);
            assert!(
                text.contains(&needle),
                "`{name}` request {} does not contain `{needle}`:\n{text}",
                contains.request
            );
        }
    }

    // Interviews.
    let interviews = &expect.interviews;
    let receipt_needed = !scenario.interviews.is_empty();
    let receipt = receipt_needed.then(|| finished.receipt());
    if let Some(receipt) = &receipt {
        let questions = receipt["questions"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            questions.len(),
            interviews.questions.len(),
            "question count\n{receipt:#}"
        );
        for (index, (expected, actual)) in interviews.questions.iter().zip(&questions).enumerate() {
            let expected = bindings.value(expected);
            let expected = if expected
                .as_object()
                .is_some_and(|m| m.keys().any(|k| k.starts_with('$')))
            {
                expected
            } else {
                json!({ "$subset": expected })
            };
            matches(&expected, Some(actual))
                .unwrap_or_else(|error| panic!("question {index}: {error}\n{actual:#}"));
        }
        matches(
            &Value::Array(interviews.errors.clone()),
            Some(&receipt["errors"]),
        )
        .unwrap_or_else(|error| panic!("receipt errors: {error}\n{receipt:#}"));
        let entries = receipt["script"]["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for (id, count) in &interviews.consumed {
            let entry = entries
                .iter()
                .find(|entry| entry["id"] == id.as_str())
                .unwrap_or_else(|| panic!("script entry `{id}` is not in the receipt"));
            assert_eq!(entry["consumed"], json!(count), "entry `{id}` consumed");
        }
        for text in &interviews.no_plaintext {
            let text = bindings.text(text);
            let receipt_text = receipt.to_string();
            assert!(!receipt_text.contains(&text), "`{text}` is in the receipt");
            assert!(!finished.stderr.contains(&text), "`{text}` is on stderr");
            let context_text = serde_json::to_string(&context).expect("context");
            assert!(
                !context_text.contains(&text),
                "`{text}` is in the final context"
            );
        }
    } else {
        assert!(
            interviews.questions.is_empty() && interviews.consumed.is_empty(),
            "the scenario expects interviews but scripts none"
        );
    }

    // Lifecycle.
    let lifecycle = &expect.lifecycle;
    if lifecycle.retained_workspace {
        assert!(ran.workspace.is_dir(), "the workspace was retained");
        if ran.scenario.modes.backend == Backend::Host || ran.case.is_docker() {
            let reported = finished.reported_workspaces();
            let sandboxes = finished.reported_sandboxes();
            assert!(
                !reported.is_empty() || !sandboxes.is_empty(),
                "the run reported its workspace\n{}",
                finished.stderr
            );
        }
    }
    if lifecycle.no_requests_after_cancel {
        let at = requests_at_interrupt.expect("the interrupt fired");
        let now: usize = ran.twins.iter().map(|t| t.request_log().len()).sum();
        assert_eq!(now, at, "no model request after the cancel");
    }
    if let Some(reason) = &lifecycle.cancel_reason {
        assert_eq!(
            document["invocations"][0]["cancel_reason"]["kind"],
            json!(reason),
            "the root invocation's cancel reason"
        );
    }
    if lifecycle.no_leaked_processes {
        finished.assert_no_leaked_processes().await;
    }
    if lifecycle.prune_removes_sandbox {
        let name = testkit::sandbox_name(&ran.case.run_dir, 0);
        let (code, stderr) = ran.case.prune().await;
        assert_eq!(code, Some(0), "prune: {stderr}");
        let listed = Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{name}$"),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .expect("docker ps");
        assert!(
            String::from_utf8_lossy(&listed.stdout).trim().is_empty(),
            "the container `{name}` is gone after prune"
        );
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// The first `python3` on PATH that imports every module named, with the
/// site directories those modules live in, verified once more under the
/// run's `home` so a user site-packages directory is carried explicitly.
fn python_with(modules: &[String], home: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let path = env::var_os("PATH")?;
    let locate = format!(
        "import os
{}
for m in [{}]:
    print(os.path.dirname(os.path.dirname(m.__file__)))
",
        modules
            .iter()
            .map(|module| format!("import {module}"))
            .collect::<Vec<_>>()
            .join(
                "
"
            ),
        modules.join(", ")
    );
    let check = modules
        .iter()
        .map(|module| format!("import {module}"))
        .collect::<Vec<_>>()
        .join("; ");
    for candidate in env::split_paths(&path).map(|dir| dir.join("python3")) {
        if !candidate.is_file() {
            continue;
        }
        let Ok(output) = Command::new(&candidate)
            .args(["-c", &locate])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let mut site_dirs: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(PathBuf::from)
            .collect();
        site_dirs.sort();
        site_dirs.dedup();
        let joined = env::join_paths(&site_dirs).ok()?;
        let verified = Command::new(&candidate)
            .args(["-c", &check])
            .env("HOME", home)
            .env("PYTHONPATH", &joined)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if verified {
            return Some((candidate, site_dirs));
        }
    }
    None
}

fn provider_of(agent: Agent) -> Option<Provider> {
    match agent {
        Agent::OpenAi => Some(Provider::OpenAi),
        Agent::Anthropic => Some(Provider::Anthropic),
        Agent::OpenRouter => Some(Provider::OpenRouter),
        Agent::Acp | Agent::None => None,
    }
}

fn provider_named(name: &str) -> Provider {
    match name {
        "openai" => Provider::OpenAi,
        "anthropic" => Provider::Anthropic,
        "openrouter" => Provider::OpenRouter,
        other => panic!("unknown provider `{other}`"),
    }
}

/// A scenario's twin script with the harness defaults filled: the case's
/// namespace, and the provider's endpoint and model when the matcher names
/// none.
fn twin_script(provider: Provider, namespace: &str, script: &Value) -> Value {
    let mut script = script.clone();
    let map = script
        .as_object_mut()
        .expect("a twin scenario is an object");
    map.entry("namespace").or_insert_with(|| json!(namespace));
    let matcher = map
        .entry("matcher")
        .or_insert_with(|| Value::Object(Map::new()));
    let matcher = matcher.as_object_mut().expect("matcher is an object");
    matcher
        .entry("endpoint")
        .or_insert_with(|| json!(provider.endpoint()));
    matcher
        .entry("model")
        .or_insert_with(|| json!(wire_model(provider)));
    script
}

fn root_history(document: &Value) -> Vec<Value> {
    let Some(execution) = document["root"]["final_execution"].as_u64() else {
        // An unfinished root (a cancelled run): the last execution listed.
        return document["executions"]
            .as_array()
            .and_then(|e| e.last())
            .and_then(|e| e["engine"]["history"].as_array())
            .cloned()
            .unwrap_or_default();
    };
    document["executions"]
        .as_array()
        .and_then(|executions| executions.iter().find(|e| e["execution"] == execution))
        .and_then(|record| record["engine"]["history"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn base_name(instance: &str) -> &str {
    instance.split('#').next().unwrap_or(instance)
}

fn write_spec(target: &Path, spec: &FileSpec, dir: &Path, bindings: &Bindings) {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    let bytes = spec.bytes(dir).unwrap_or_else(|error| panic!("{error}"));
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => bindings.text(&text).into_bytes(),
        Err(error) => error.into_bytes(),
    };
    fs::write(target, bytes).unwrap_or_else(|e| panic!("write {}: {e}", target.display()));
    #[cfg(unix)]
    if spec.executable() {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).expect("read bundle dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            fs::create_dir_all(&target).expect("bundle directory");
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy bundle file");
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    git_try(repo, args).unwrap_or_else(|| panic!("git {args:?} failed in {}", repo.display()))
}

fn git_try(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The one file `pattern` names under `root`: a plain path, or a glob with
/// `*` in path segments that must match exactly one file.
fn glob_one(root: &Path, pattern: &str) -> Option<PathBuf> {
    if !pattern.contains('*') {
        let path = root.join(pattern);
        return path.exists().then_some(path);
    }
    let matcher = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .expect("a valid glob")
        .compile_matcher();
    let mut found = Vec::new();
    walk(root, root, &matcher, &mut found);
    assert!(
        found.len() <= 1,
        "glob `{pattern}` matches more than one file: {found:?}"
    );
    found.pop()
}

fn walk(root: &Path, dir: &Path, matcher: &globset::GlobMatcher, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root)
            && matcher.is_match(relative)
        {
            out.push(path.clone());
        }
        if path.is_dir() {
            walk(root, &path, matcher, out);
        }
    }
}
