#!/usr/bin/env bash
set -euo pipefail

# The Rust dependencies (sandbox-driver, Pebble, lithos-llm, the twins) are
# public and resolve over HTTPS with no credentials. Only the two private
# Fabro black box bundle sources (scripts/corpus-fetch-fabro-bundles.sh) need
# a key. Each GitHub deploy key belongs to one repository, so an SSH alias
# per repository ensures Git offers the correct key.
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"
: "${ACTION_PATH:?ACTION_PATH is required}"
if [ -z "${CODE_REVIEW_DEPLOY_KEY:-}" ] && [ -z "${FACTORY_DEPLOY_KEY:-}" ]; then
  echo "No bundle source deploy keys are configured; the bundle fetch will name the missing key."
  exit 0
fi
umask 077
credentials="$RUNNER_TEMP/petri-dependencies"
mkdir -p "$credentials"
: > "$credentials/ssh_config"
configure() {
  local owner="$1" repository="$2"
  cat >> "$credentials/ssh_config" <<CONFIG
Host petri-$repository
  HostName github.com
  HostKeyAlias github.com
  User git
  IdentityFile "$credentials/$repository"
  IdentitiesOnly yes
  IdentityAgent none
  StrictHostKeyChecking yes
  UserKnownHostsFile "$ACTION_PATH/known_hosts"
CONFIG
  git config --global "url.ssh://git@petri-$repository/$owner/$repository.insteadOf" \
    "ssh://git@github.com/$owner/$repository"
}
if [ -n "${CODE_REVIEW_DEPLOY_KEY:-}" ]; then
  printf '%s\n' "$CODE_REVIEW_DEPLOY_KEY" > "$credentials/code-review"
  configure lithoscomputer code-review
fi
if [ -n "${FACTORY_DEPLOY_KEY:-}" ]; then
  printf '%s\n' "$FACTORY_DEPLOY_KEY" > "$credentials/factory"
  configure veniceai factory
fi
printf -v ssh_command 'ssh -F %q' "$credentials/ssh_config"
git config --global core.sshCommand "$ssh_command"
