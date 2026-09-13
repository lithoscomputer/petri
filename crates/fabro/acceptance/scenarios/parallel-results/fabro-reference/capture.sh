#!/bin/bash
# Capture the parallel-results reference from a pinned Fabro binary.
#
# This is test and parity tooling. It never uses the `fabro` on PATH. Build the
# binary first, from a confined clone at the exact compatibility revision:
#
#   scripts/corpus-fetch-fabro.sh      # fetches crates/fabro/corpus-pin.txt's commit
#   scripts/fabro-provision.sh          # builds it into crates/fabro/corpus/fabro-target/
#
# or, by hand, at the pinned commit (on main):
#
#   git clone --no-checkout https://github.com/fabro-sh/fabro.git fabro
#   git -C fabro checkout --detach 05ebd0fd1beec214b558f4b478e36bd08b507dc7
#   cargo build --locked -p fabro-cli --manifest-path fabro/Cargo.toml
#
# Usage: capture.sh <fabro-bin> <sandbox-root> <workflow-dir> <workflow-file> [fabro run args...]
#
# Everything Fabro touches lives under <sandbox-root>: a private HOME with a
# dev-token-only settings.toml, the server storage, the CLI auth store, and the
# captured outputs (run.out, events.jsonl, inspect.json, dump/). The server
# binds a loopback TCP port (FABRO_PROBE_PORT, default 47831) because the default
# Unix socket path exceeds the 104-byte limit under deep temp dirs. The dev
# token, session secret, and provider key are fixed placeholders; the provider
# key is never sent anywhere because the scenario has no agent nodes. The server
# is killed on exit.
set -u
F=$1; ROOT=$2; WDIR=$3; WF=$4; shift 4
TOK=fabro_dev_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
SS=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
PORT=${FABRO_PROBE_PORT:-47831}
rm -rf "$ROOT"; mkdir -p "$ROOT/home/.fabro"
printf '_version = 1\n\n[server.auth]\nmethods = ["dev-token"]\n' > "$ROOT/home/.fabro/settings.toml"
export HOME="$ROOT/home" NO_COLOR=1 FABRO_NO_UPGRADE_CHECK=true FABRO_HTTP_PROXY_POLICY=disabled FABRO_DEV_TOKEN=$TOK SESSION_SECRET=$SS FABRO_TELEMETRY=0
unset FABRO_SERVER FABRO_CONFIG FABRO_HOME FABRO_STORAGE_DIR
"$F" server start --foreground --no-web --bind 127.0.0.1:$PORT > "$ROOT/server.log" 2>&1 &
SPID=$!
trap 'kill $SPID 2>/dev/null; wait $SPID 2>/dev/null' EXIT
for i in $(seq 1 60); do grep -q 'listening' "$ROOT/server.log" && break; sleep 0.5; done
grep -q 'listening' "$ROOT/server.log" || { echo "server did not start"; cat "$ROOT/server.log"; exit 2; }
"$F" auth login --server http://127.0.0.1:$PORT --dev-token $TOK > "$ROOT/login.log" 2>&1 || { cat "$ROOT/login.log"; exit 3; }
"$F" secret set OPENAI_API_KEY test-openai-key --type token > "$ROOT/provider.log" 2>&1 || { cat "$ROOT/provider.log"; exit 5; }
cd "$WDIR" || exit 4
OPENAI_API_KEY=test "$F" run --server http://127.0.0.1:$PORT --auto-approve --environment local --provider openai "$@" "$WF" > "$ROOT/run.out" 2>&1
echo "run exit=$?"
RUN_ID=$(grep -o 'Run: [0-9A-Z]*' "$ROOT/run.out" | head -1 | awk '{print $2}')
echo "run id=$RUN_ID"
[ -n "$RUN_ID" ] || exit 0
"$F" events --server http://127.0.0.1:$PORT --json "$RUN_ID" > "$ROOT/events.jsonl" 2> "$ROOT/events.err"; echo "events exit=$?"
"$F" inspect --server http://127.0.0.1:$PORT --json "$RUN_ID" > "$ROOT/inspect.json" 2> "$ROOT/inspect.err"; echo "inspect exit=$?"
"$F" dump --server http://127.0.0.1:$PORT --output "$ROOT/dump" "$RUN_ID" > "$ROOT/dump.log" 2>&1; echo "dump exit=$?"
