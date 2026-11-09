//! Required assets of the black box battery: the pinned Fabro binary, the Fabro
//! corpus, a Docker daemon, and a Daytona account.
//!
//! Every check follows the convention `PETRI_REQUIRE_DOCKER` set: an absent
//! asset skips with a visible notice on a developer machine, and fails when
//! the matching `PETRI_REQUIRE_*` variable says CI expected it. A silently
//! skipped scenario is indistinguishable from a passing one, and the
//! readiness gate never counts a skip as a pass.
//!
//! | Asset | Variable | Materialized by |
//! |---|---|---|
//! | fabro binary | `PETRI_REQUIRE_FABRO_BINARY` | `scripts/fabro-provision.sh` (handed over as `FABRO_BIN`) |
//! | corpus | `PETRI_REQUIRE_FABRO_CORPUS` | `scripts/corpus-fetch-fabro.sh` |
//! | Docker | `PETRI_REQUIRE_DOCKER` | a reachable daemon and the Docker plugin |
//! | Daytona | `PETRI_REQUIRE_DAYTONA` | `DAYTONA_API_KEY` and the Daytona plugin (`mise run test:daytona`) |

use std::env;
use std::path::{Path, PathBuf};

/// The skip-or-fail decision, kept pure so it can be tested without touching
/// the process environment. `required` is the value of the `PETRI_REQUIRE_*`
/// variable, `None` when unset. Returns whether the asset is usable, or the
/// failure message when it is absent and required.
pub(crate) fn decide(
    present: bool,
    required: Option<&str>,
    variable: &str,
    what: &str,
) -> Result<bool, String> {
    if present {
        return Ok(true);
    }
    match required {
        Some(value) if !value.is_empty() => Err(format!("{variable} is set, but {what}")),
        _ => Ok(false),
    }
}

/// Apply [`decide`] to the process environment, printing the skip notice.
///
/// # Panics
///
/// Panics with the failure message when the asset is absent and required.
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
pub(crate) fn require_or_skip(present: bool, variable: &str, what: &str) -> bool {
    let required = env::var(variable).ok();
    match decide(present, required.as_deref(), variable, what) {
        Ok(true) => true,
        Ok(false) => {
            eprintln!("skipping: {what}");
            false
        }
        Err(message) => panic!("{message}"),
    }
}

/// The workspace root, from this crate's manifest directory.
pub(crate) fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    root.canonicalize().unwrap_or(root)
}

/// The pinned `fabro` binary `FABRO_BIN` names, or the one at the cache
/// path `scripts/fabro-provision.sh` builds into, or `None` (with a notice)
/// when neither exists. The differential adapter (`fabro_adapter.rs`)
/// checks the binary reports the pinned commit; a `fabro` on `PATH` is
/// never used.
///
/// # Panics
///
/// Panics when `PETRI_REQUIRE_FABRO_BINARY` is set and the binary is absent.
pub(crate) fn fabro_binary() -> Option<PathBuf> {
    let path = env::var_os("FABRO_BIN").map_or_else(
        || workspace_root().join("crates/fabro/corpus/fabro-target/debug/fabro"),
        PathBuf::from,
    );
    require_or_skip(
        path.is_file(),
        "PETRI_REQUIRE_FABRO_BINARY",
        "no pinned fabro binary (FABRO_BIN, or the cache scripts/fabro-provision.sh builds into)",
    )
    .then_some(path)
}

/// The fetched Fabro corpus `crates/fabro/corpus/fabro`, or `None` (with a
/// notice) when it is not fetched.
///
/// # Panics
///
/// Panics when `PETRI_REQUIRE_FABRO_CORPUS` is set and the corpus is absent.
pub(crate) fn fabro_corpus() -> Option<PathBuf> {
    let root = workspace_root().join("crates/fabro/corpus/fabro");
    require_or_skip(
        root.join(".fabro/workflows").is_dir(),
        "PETRI_REQUIRE_FABRO_CORPUS",
        "the Fabro corpus is not fetched (run scripts/corpus-fetch-fabro.sh)",
    )
    .then_some(root)
}

/// A reachable Docker daemon with the Docker plugin, under
/// `PETRI_REQUIRE_DOCKER`'s convention.
pub(crate) async fn docker() -> bool {
    testkit::is_docker_ready().await
}

/// A Daytona plugin whose backend accepts the credentials in the
/// environment, under `PETRI_REQUIRE_DAYTONA`'s convention.
pub(crate) async fn daytona() -> bool {
    testkit::is_daytona_ready().await
}
