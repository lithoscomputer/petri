#!/usr/bin/env bash
# Install the sandbox-driver plugin executables the tests launch, from the
# sandbox-driver commit Petri's `Cargo.lock` locks, under
# `target/plugins/bin`: the host and Docker plugins by default, or the kinds
# SANDBOX_DRIVER_PLUGINS lists (`mise run plugins:build:daytona` adds the
# Daytona plugin for the live tier). With a release target, install all three
# plugins under `target/<target>/plugins/bin` for bundling. Cargo skips a
# plugin already installed from the same commit. Set SANDBOX_DRIVER_SOURCE
# to a local checkout to build the provider packages during coordinated
# development.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $# -gt 1 ]]; then
  echo "Usage: scripts/plugins-build.sh [release-target]" >&2
  exit 2
fi
install_root="$root/target/plugins"
# shellcheck disable=SC2206 # the kinds are space-separated words by contract
kinds=(${SANDBOX_DRIVER_PLUGINS:-host docker})
target_args=()
if [[ $# -eq 1 ]]; then
  install_root="$root/target/$1/plugins"
  target_args=(--target "$1")
  kinds=(host docker daytona)
fi
if [[ -n "${SANDBOX_DRIVER_SOURCE:-}" ]]; then
  source_root=$(cd "$SANDBOX_DRIVER_SOURCE" && pwd)
  for kind in "${kinds[@]}"; do
    # The executable names are unchanged from the former wrapper packages.
    cargo install --quiet --locked --force \
      --path "$source_root/crates/sandbox-driver-$kind" \
      --root "$install_root" \
      ${target_args[@]+"${target_args[@]}"}
  done
  echo "plugins: $install_root/bin (local checkout)"
  exit 0
fi

packages=()
for kind in "${kinds[@]}"; do
  packages+=("sandbox-driver-$kind")
done
# Read the resolved commit, including the manifest pin during the API freeze.
locked_source="$(awk '
  $0 == "[[package]]" { name = "" }
  $0 == "name = \"sandbox-driver\"" { name = "sandbox-driver" }
  name == "sandbox-driver" && /^source = "git\+/ { print; exit }
' "$root/Cargo.lock")"
url="$(sed -E 's/^source = "git\+([^?#"]+).*/\1/' <<<"$locked_source")"
rev="$(sed -E 's/.*#([0-9a-f]+)"$/\1/' <<<"$locked_source")"
if [ -z "$locked_source" ] || [ -z "$url" ] || [ -z "$rev" ]; then
  echo "plugins-build: the locked sandbox-driver commit was not found in $root/Cargo.lock" >&2
  exit 1
fi

# Replace binaries installed by the old wrapper packages once. Later runs
# retain Cargo's normal skip when this exact commit is already installed.
install_args=()
installed=$(cargo install --list --root "$install_root")
for kind in "${kinds[@]}"; do
  if [[ "$installed" == *"sandbox-driver-$kind-plugin "* ]]; then
    install_args=(--force)
    break
  fi
done
cargo install --quiet --locked \
  ${install_args[@]+"${install_args[@]}"} \
  --git "$url" --rev "$rev" \
  --root "$install_root" \
  ${target_args[@]+"${target_args[@]}"} \
  "${packages[@]}"
echo "plugins: $install_root/bin ($rev)"
