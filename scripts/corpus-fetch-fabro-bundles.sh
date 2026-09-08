#!/usr/bin/env bash
# Materialize the Fabro black box bundles named in
# crates/fabro/acceptance/bundles.lock.json and verify every digest.
#
#   scripts/corpus-fetch-fabro-bundles.sh            fetch and verify
#   scripts/corpus-fetch-fabro-bundles.sh --verify   verify an existing checkout only
#
# Each source repository is fetched at its locked revision into
# crates/fabro/acceptance/bundles/.sources/<owner>/<repo> (depth 1). Every file
# a bundle lists is copied into crates/fabro/acceptance/bundles/<id>/ with its
# mode, and its SHA-256 is compared with the lock. A missing file, a digest that
# differs, a bundle hash that differs, or a source that cannot be fetched fails
# the script. No bundle is skipped silently.
#
# Private sources are reached over SSH. CI configures deploy keys through
# .github/actions/private-dependencies, which rewrites
# ssh://git@github.com/<owner>/<repo> to a per-repository host alias. A
# developer machine uses its own SSH agent. FABRO_BUNDLE_SOURCE_<OWNER>_<REPO>
# (for example FABRO_BUNDLE_SOURCE_LITHOSCOMPUTER_CODE_REVIEW) overrides one
# source URL with a local path, for offline use.
set -euo pipefail
cd "$(dirname "$0")/.."

LOCK=crates/fabro/acceptance/bundles.lock.json
ROOT=crates/fabro/acceptance/bundles
SOURCES="$ROOT/.sources"
VERIFY_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --verify) VERIFY_ONLY=1 ;;
    *) echo "usage: $0 [--verify]" >&2; exit 2 ;;
  esac
done

command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }
[ -f "$LOCK" ] || { echo "error: $LOCK is missing" >&2; exit 1; }
mkdir -p "$SOURCES"

# Fetch every source at its revision.
if [ "$VERIFY_ONLY" -eq 0 ]; then
  while IFS=$'\t' read -r repository url revision visibility access; do
    dir="$SOURCES/$repository"
    env_name="FABRO_BUNDLE_SOURCE_$(echo "$repository" | tr '[:lower:]/-' '[:upper:]__')"
    source="${!env_name:-$url}"
    if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$revision" ]; then
      echo "$repository already at $revision"
      continue
    fi
    mkdir -p "$dir"
    git -C "$dir" init -q 2>/dev/null || true
    if ! git -C "$dir" fetch -q --depth 1 "$source" "$revision"; then
      # A required source that cannot be reached fails the whole set. Say
      # what would have reached it, because in CI the usual cause is a deploy
      # key that is not configured.
      {
        echo "error: could not fetch $repository at $revision from $source"
        echo "  visibility: $visibility"
        echo "  access: $access"
        if [ "$visibility" = private ]; then
          echo "  In CI, pass the matching *-key input of .github/actions/private-dependencies from"
          echo "  the repository secret named above (DEVELOPING.md, \"Dependencies and bundle sources\")."
          echo "  Locally, use an SSH agent with read access, or set $env_name to a checkout."
        fi
        echo "  The bundle set is required: no bundle is skipped."
      } >&2
      exit 1
    fi
    git -C "$dir" checkout -qf FETCH_HEAD
    [ "$(git -C "$dir" rev-parse HEAD)" = "$revision" ] || {
      echo "error: $repository checkout is not at $revision" >&2
      exit 1
    }
    echo "$repository fetched at $revision"
  done < <(python3 -c '
import json, sys
lock = json.load(open(sys.argv[1]))
for key, source in lock["sources"].items():
    print(source["repository"], source["url"], source["revision"],
          source.get("visibility", "unknown"), source.get("access", ""), sep="\t")
' "$LOCK")
fi

# Copy and verify every bundle.
python3 - "$LOCK" "$ROOT" "$SOURCES" <<'EOF'
import hashlib
import json
import os
import shutil
import stat
import sys

lock_path, root, sources = sys.argv[1:4]
lock = json.load(open(lock_path))
failures = []


def sha256(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def bundle_hash(files):
    lines = []
    for f in files:
        if f["kind"] == "symlink":
            lines.append(f"symlink {f['target']} {f['path']}")
        else:
            lines.append(f"{f['mode']} {f['sha256']} {f['path']}")
    return hashlib.sha256(("\n".join(lines) + "\n").encode()).hexdigest()


for bundle in lock["bundles"]:
    source = lock["sources"][bundle["source"]]
    src_root = os.path.join(sources, source["repository"])
    dest_root = os.path.join(root, bundle["id"])
    if os.path.isdir(dest_root):
        shutil.rmtree(dest_root)
    problems = []
    for entry in bundle["files"]:
        src = os.path.join(src_root, entry["path"])
        dest = os.path.join(dest_root, entry["path"])
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        if entry["kind"] == "symlink":
            if not os.path.islink(src) or os.readlink(src) != entry["target"]:
                problems.append(f"symlink {entry['path']} missing or points elsewhere")
                continue
            os.symlink(entry["target"], dest)
            continue
        if not os.path.isfile(src):
            problems.append(f"missing {entry['path']}")
            continue
        digest = sha256(src)
        if digest != entry["sha256"]:
            problems.append(f"digest drift {entry['path']}: {digest} != {entry['sha256']}")
            continue
        shutil.copyfile(src, dest)
        os.chmod(dest, 0o755 if entry["mode"] == "0755" else 0o644)
    computed = bundle_hash(bundle["files"])
    if computed != bundle["bundle_hash"]:
        problems.append(f"bundle hash {computed} != {bundle['bundle_hash']} (lock is inconsistent)")
    for name, value in bundle["dependencies"]["helper_hashes_embedded_in_graph"].items():
        listed = next((f for f in bundle["files"] if f["path"] == name), None)
        if listed is None or listed.get("sha256") != value:
            problems.append(f"graph-embedded hash for {name} does not match the bundle file")
    if bundle["dependencies"]["missing_from_bundle"]:
        problems.append(f"unresolved dependencies: {bundle['dependencies']['missing_from_bundle']}")
    if problems:
        failures.append((bundle["id"], problems))
        print(f"FAIL {bundle['id']}")
        for p in problems:
            print(f"     {p}")
    else:
        print(f"ok   {bundle['id']} ({len(bundle['files'])} files, {bundle['status']})")

with open(os.path.join(root, "MANIFEST.txt"), "w") as out:
    out.write(f"materialized from {os.path.basename(lock_path)}; fabro {lock['fabro_reference']['commit']}\n")
    for bundle in lock["bundles"]:
        out.write(f"{bundle['id']}\t{bundle['status']}\t{bundle['bundle_hash']}\n")

if failures:
    print(f"{len(failures)} bundle(s) failed verification", file=sys.stderr)
    sys.exit(1)
print(f"{len(lock['bundles'])} bundles materialized and verified under {root}")
EOF
