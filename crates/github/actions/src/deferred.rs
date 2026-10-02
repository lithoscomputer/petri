//! Lazy manifest lookup and graph planning for GitHub actions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use frontend::MapFiles;
use frontend_gha::action::{ActionLocation, ActionSource};
use frontend_gha::{DeferredActionPlan, plan_deferred_action};
use ir::{
    FailureClass, FailureInfo, GraphFragment, LogStream, Outcome, ScopeId, SpliceRequest, Status,
    StepRef, Value,
};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, json};
use smol_str::SmolStr;
use steps::{Step, StepCtx, StepFailure, ValueOrSecretRef};
use tokio::task;

use crate::action::{ActionSourceCap, stage};
use crate::gate;
use crate::session::REPO_DIR;

const DEFERRED_CLASS: FailureClass = FailureClass::new_static("deferred_action");
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// The synchronous action resolver used when a deferred composite discovers a
/// remote action that the static workflow could not see.
pub struct ActionManifestSourceCap(pub Arc<dyn ActionSource>);

/// Reads the action manifest from the job environment and appends its planned
/// main fragment.
pub struct DeferredActionStep;

#[derive(Debug)]
pub struct DeferredConfig {
    plan:      DeferredActionPlan,
    gate:      Option<Value>,
    cancelled: bool,
}

impl<'de> Deserialize<'de> for DeferredConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut fields = Map::<String, Value>::deserialize(deserializer)?;
        let gate = fields
            .remove("gate")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .flatten();
        let cancelled = fields
            .remove("cancelled")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or(false);
        let plan = serde_json::from_value(Value::Object(fields)).map_err(D::Error::custom)?;
        Ok(Self {
            plan,
            gate,
            cancelled,
        })
    }
}

#[async_trait::async_trait]
impl Step for DeferredActionStep {
    const NAME: &'static str = frontend_gha::DEFERRED_ACTION_KIND;
    type Config = DeferredConfig;

    async fn run(&self, config: Self::Config, ctx: StepCtx) -> Outcome {
        let DeferredConfig {
            plan: config,
            gate,
            cancelled,
        } = config;
        let index = config.index;
        let result_name = config.result_name.clone();
        let fail = |message: String| resolved_failure(&result_name, index, message);
        let gate_env: BTreeMap<SmolStr, ValueOrSecretRef> =
            match serde_json::from_value(Value::Object(config.env.clone())) {
                Ok(env) => env,
                Err(error) => {
                    return fail(format!(
                        "the deferred action environment is invalid: {error}"
                    ));
                }
            };
        match gate::refusal(
            gate.as_ref(),
            cancelled,
            &gate_env,
            config.job_environment.as_deref(),
            config.background.as_deref(),
            &ctx,
        )
        .await
        {
            Ok(Some(refusal)) => {
                return resolved_result(
                    &result_name,
                    index,
                    refusal.status.tag(),
                    refusal.output,
                    None,
                );
            }
            Err(failure) => return fail(failure.message),
            Ok(None) => {}
        }

        if config.depth >= frontend_gha::COMPOSITE_MAX_DEPTH {
            return fail(format!(
                "composite actions nest more than {} deep",
                frontend_gha::COMPOSITE_MAX_DEPTH
            ));
        }
        let files = match manifest_files(&config.action, &ctx).await {
            Ok(files) => files,
            Err(failure) => return fail(failure.message),
        };
        let source = ctx.capability::<ActionManifestSourceCap>();
        let planned = match task::spawn_blocking(move || {
            plan_deferred_action(
                &config,
                &files,
                source.as_deref().map(|source| source.0.as_ref()),
            )
        })
        .await
        {
            Err(error) => {
                return fail(format!("the deferred action planner task failed: {error}"));
            }
            Ok(Ok(planned)) => planned,
            Ok(Err(diagnostics)) => {
                let message = diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                return fail(message);
            }
        };
        for diagnostic in &planned.diagnostics {
            ctx.log(LogStream::Stderr, diagnostic.to_string()).await;
        }
        let main = SpliceRequest::append(planned.main).inherit_uploader_context(ScopeId::new(0));
        let post = planned
            .post
            .map(|fragment| {
                SpliceRequest::append(fragment).inherit_uploader_context(ScopeId::new(0))
            })
            .map(|request| {
                serde_json::to_string(&request).expect("a post splice request serializes")
            });
        Outcome::success(json!({ "post": post })).with_splice(main)
    }
}

async fn manifest_files(action: &ActionLocation, ctx: &StepCtx) -> Result<MapFiles, StepFailure> {
    let root = match action {
        ActionLocation::Local { .. } => PathBuf::from(REPO_DIR),
        ActionLocation::Pinned(pinned) => {
            let source = ctx.require_capability::<ActionSourceCap>()?;
            stage(ctx, &source.0, pinned).await?
        }
    };
    let directory = action.directory();
    let prefix = if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/")
    };
    for candidate in [
        format!("{prefix}action.yml"),
        format!("{prefix}action.yaml"),
    ] {
        let path = root.join(&candidate);
        let bytes = ctx
            .env
            .read_file_limited(&path, MAX_MANIFEST_BYTES)
            .await
            .map_err(|error| StepFailure {
                class:   DEFERRED_CLASS,
                message: format!("could not read `{}`: {error}", path.display()),
            })?;
        if let Some(bytes) = bytes {
            let text = String::from_utf8(bytes).map_err(|error| StepFailure {
                class:   DEFERRED_CLASS,
                message: format!("`{}` is not UTF-8: {error}", path.display()),
            })?;
            return Ok(MapFiles(BTreeMap::from([(candidate, text)])));
        }
    }
    Err(StepFailure {
        class:   DEFERRED_CLASS,
        message: format!(
            "no `action.yml` or `action.yaml` under `{directory}` in the job workspace"
        ),
    })
}

fn resolved_failure(result_name: &str, index: Option<u32>, message: String) -> Outcome {
    resolved_result(result_name, index, "failure", Value::Null, Some(message))
}

fn resolved_result(
    result_name: &str,
    index: Option<u32>,
    status: &str,
    output: Value,
    message: Option<String>,
) -> Outcome {
    let name = index.map_or_else(
        || result_name.to_string(),
        |index| format!("{result_name}#{index}"),
    );
    let mut fragment = GraphFragment::chain([(
        name.as_str(),
        StepRef::new(
            frontend_gha::DEFERRED_ACTION_RESULT_KIND,
            serde_json::to_value(DeferredResult {
                status: status.to_string(),
                output,
                message,
            })
            .expect("a deferred result is JSON-serializable"),
        ),
    )]);
    fragment.body.nodes[0].run_on_cancel = true;
    fragment.body.nodes[0].tolerates_failure = true;
    let request = SpliceRequest::append(fragment).inherit_uploader_context(ScopeId::new(0));
    Outcome::success(json!({ "post": null })).with_splice(request)
}

/// The private fragment terminal. It carries a stable envelope and always
/// succeeds so the static publisher can reproduce the result.
pub struct DeferredActionResultStep;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredResult {
    status:  String,
    output:  Value,
    #[serde(default)]
    message: Option<String>,
}

#[async_trait::async_trait]
impl Step for DeferredActionResultStep {
    const NAME: &'static str = frontend_gha::DEFERRED_ACTION_RESULT_KIND;
    type Config = DeferredResult;

    async fn run(&self, config: Self::Config, _ctx: StepCtx) -> Outcome {
        Outcome::success(
            serde_json::to_value(config).expect("a deferred result is JSON-serializable"),
        )
    }
}

/// Publishes the private envelope as the workflow step's result.
pub struct DeferredActionPublishStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredPublishConfig {
    result:    DeferredResult,
    #[serde(default)]
    soft_fail: bool,
}

#[async_trait::async_trait]
impl Step for DeferredActionPublishStep {
    const NAME: &'static str = frontend_gha::DEFERRED_ACTION_PUBLISH_KIND;
    type Config = DeferredPublishConfig;

    async fn run(&self, config: Self::Config, _ctx: StepCtx) -> Outcome {
        let message = config
            .result
            .message
            .unwrap_or_else(|| "the deferred action failed".to_string());
        let failure = || FailureInfo::new(message.clone()).with_class(DEFERRED_CLASS);
        let status = Status::from_tag(&config.result.status, failure).map_or_else(
            || {
                Status::Failure(
                    FailureInfo::new(format!(
                        "unknown deferred action status `{}`",
                        config.result.status
                    ))
                    .with_class(DEFERRED_CLASS),
                )
            },
            |status| match status {
                Status::Failure(underlying) if config.soft_fail => Status::partial(underlying),
                other => other,
            },
        );
        Outcome::new(status, config.result.output)
    }
}

/// Appends the post fragment saved by the resolver, or succeeds as a no-op.
pub struct DeferredActionPostStep;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredPostConfig {
    #[serde(default)]
    request: Option<String>,
}

#[async_trait::async_trait]
impl Step for DeferredActionPostStep {
    const NAME: &'static str = frontend_gha::DEFERRED_ACTION_POST_KIND;
    type Config = DeferredPostConfig;

    async fn run(&self, config: Self::Config, _ctx: StepCtx) -> Outcome {
        match config.request {
            Some(encoded) => match serde_json::from_str(&encoded) {
                Ok(request) => Outcome::success(Value::Null).with_splice(request),
                Err(error) => StepFailure {
                    class:   DEFERRED_CLASS,
                    message: format!("the saved post action plan is invalid: {error}"),
                }
                .into(),
            },
            None => Outcome::success(Value::Null),
        }
    }
}
