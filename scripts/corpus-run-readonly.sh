#!/usr/bin/env bash
# Run the GitHub corpus with the dedicated macOS Keychain credential.
# Create this fine-grained token with Public repositories access and no added
# permissions. The Keychain name does not validate the token's permissions.
# Never populate this entry with the token from `gh auth token`.
# Arguments are passed to the Rust test harness; PETRI_SWEEP_* controls apply.
set +x
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--help" ]]; then
  echo "Usage: scripts/corpus-run-readonly.sh [test harness arguments]"
  echo "Loads petri-corpus-readonly from macOS Keychain; never falls back to gh auth."
  exit 0
fi

# Do not let an inherited personal token override the dedicated credential.
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN PETRI_SWEEP_TOKEN
# The harness loads Keychain itself, so direct Cargo invocations follow the
# same credential policy and the token never enters the Cargo environment.
export PETRI_SWEEP_READONLY=1

exec cargo test --locked -p petri-github-acceptance --test runs -- --ignored --nocapture "$@"
