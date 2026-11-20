#!/usr/bin/env bash
# Verify the delivered binary and its Host and Docker execution. The sandbox
# providers are built into the binary; the archive carries nothing else.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo 'Usage: scripts/release-verify.sh <archive.tar.gz>' >&2
  exit 2
fi

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT HUP INT TERM
tar -xzf "$1" -C "$work"
"$work/petri" --version

verify_run() {
  local kind=$1
  local workflow=$2
  local success="$work/$kind-success.log"
  if ! "$work/petri" run --format native --run-dir "$work/$kind-run" "$workflow" > "$success" 2>&1; then
    cat "$success" >&2
    exit 1
  fi
  grep -q 'release-run-ok' "$success"
}

cat > "$work/host.yml" <<'WORKFLOW'
scopes:
  main:
    runtime: host
nodes:
  smoke:
    scope: main
    shell: sh
    run: echo release-run-ok
WORKFLOW
verify_run host "$work/host.yml"

if ! docker info >/dev/null 2>&1; then
  if [[ ${PETRI_REQUIRE_DOCKER:-} == 1 ]]; then
    echo 'Docker is required for the release execution checks.' >&2
    exit 1
  fi
  echo 'Archive and Host execution checks passed; Docker execution checks skipped (daemon unavailable).'
  exit 0
fi

cat > "$work/smoke.yml" <<'WORKFLOW'
scopes:
  main:
    runtime: { container: { image: alpine:3.20 } }
nodes:
  smoke:
    scope: main
    shell: sh
    run: echo release-run-ok
WORKFLOW

verify_run docker "$work/smoke.yml"
echo 'Release archive, Host and Docker execution checks passed.'
