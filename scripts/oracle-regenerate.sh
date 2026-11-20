#!/usr/bin/env bash
# Regenerate the Fabro oracle fixtures in crates/fabro/oracle/expected/ from the
# shared cases in crates/fabro/oracle/cases/, by running the cases through the
# pinned Fabro *binary*. Then record Petri's own expected results for the cases
# that declare a deliberate departure. Run this when the Fabro pin or a case
# changes; Petri CI only compares against the committed fixtures.
#
#   scripts/oracle-regenerate.sh                     regenerate every fixture
#   scripts/oracle-regenerate.sh --case NAME ...     regenerate some fixtures
#   scripts/oracle-regenerate.sh --refresh-fake-agent
#       also re-extract crates/fabro/acceptance/testdata/fake_acp_agent.py from
#       the pinned Fabro source
#
# This is testing and parity tooling. It builds `fabro` from the fetched corpus
# checkout (scripts/corpus-fetch-fabro.sh) into a confined target directory and
# launches it as a subprocess. Nothing here links a Fabro crate into Petri, and
# `fabro` is never taken from PATH. FABRO_BIN overrides the binary; the harness
# still checks that it reports the pinned commit.
set -euo pipefail
cd "$(dirname "$0")/.."

CORPUS=crates/fabro/corpus/fabro
PIN_FILE=crates/fabro/corpus-pin.txt
HARNESS=crates/fabro/oracle/harness
TARGET="${FABRO_ORACLE_TARGET_DIR:-$PWD/crates/fabro/corpus/fabro-target}"
REFRESH_FAKE_AGENT=0
CASE_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --refresh-fake-agent) REFRESH_FAKE_AGENT=1; shift ;;
    --case) CASE_ARGS+=(--case "$2"); shift 2 ;;
    *) echo "usage: $0 [--refresh-fake-agent] [--case NAME]..." >&2; exit 2 ;;
  esac
done

[ -d "$CORPUS/lib" ] || { echo "error: the Fabro corpus is not fetched; run scripts/corpus-fetch-fabro.sh" >&2; exit 1; }
pin=$(awk '$1 !~ /^#/ && NF { print $1; exit }' "$PIN_FILE")
commit=$(git -C "$CORPUS" rev-parse HEAD)
[ "$commit" = "$pin" ] || { echo "error: the corpus is at $commit but the pin is $pin; run scripts/corpus-fetch-fabro.sh" >&2; exit 1; }

# scripts/fabro-provision.sh builds the binary into $TARGET (or checks
# FABRO_BIN) and refuses one that does not report the pinned commit.
FABRO_BIN=$(FABRO_ORACLE_TARGET_DIR="$TARGET" scripts/fabro-provision.sh)

if [ "$REFRESH_FAKE_AGENT" -eq 1 ]; then
  python3 - "$CORPUS/lib/components/fabro-acp/src/test_support.rs" crates/fabro/acceptance/testdata/fake_acp_agent.py <<'EOF'
import sys
source, dest = sys.argv[1:3]
text = open(source, encoding="utf-8").read()
start = text.index("pub fn fake_acp_agent_script()")
body = text[start:]
body = body[body.index('r#"') + 3:]
body = body[: body.index('"#')]
header = open(dest, encoding="utf-8").read().split("\nimport ", 1)[0]
open(dest, "w", encoding="utf-8").write(header + "\n" + body.lstrip("\n"))
print(f"refreshed {dest}")
EOF
fi

python3 "$HARNESS/oracle_harness.py" \
  --fabro "$FABRO_BIN" --commit "$commit" \
  --cases crates/fabro/oracle/cases --expected crates/fabro/oracle/expected \
  "${CASE_ARGS[@]+"${CASE_ARGS[@]}"}"

# Petri's half runs on the Host and Docker providers built into the binary.
PETRI_ORACLE_RECORD=1 cargo nextest run -p petri-fabro-acceptance --test routing every_case_matches_the_fabro_oracle
echo "regenerated crates/fabro/oracle/expected at fabro $commit"
