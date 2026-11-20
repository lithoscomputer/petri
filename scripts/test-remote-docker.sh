#!/usr/bin/env bash
# Run the real cross-job artifact battery against an isolated Docker daemon.
# The daemon has no host bind mounts; workspace access must cross the provider.
set -euo pipefail

if ! docker info >/dev/null 2>&1; then
  if [[ ${PETRI_REQUIRE_DOCKER:-} == 1 ]]; then
    echo 'Docker is required for the remote-daemon acceptance test.' >&2
    exit 1
  fi
  echo 'Remote Docker acceptance skipped (daemon unavailable).'
  exit 0
fi

images=crates/github/acceptance/src/runs.rs
image=$(sed -n 's/^pub const RUNNER_IMAGE_2404_DIND:.*= "\([^"]*\)";/\1/p' "$images")
runner=$(sed -n 's/^pub const RUNNER_IMAGE_2404:.*= "\([^"]*\)";/\1/p' "$images")
if [[ -z "$image" || -z "$runner" ]]; then
  echo 'The acceptance runner image pins were not found.' >&2
  exit 1
fi

daemon="petri-remote-acceptance-$$"
cleanup() {
  result=$?
  if [[ $result -ne 0 ]]; then
    docker logs "$daemon" >&2 || true
  fi
  docker rm -f -v "$daemon" >/dev/null 2>&1 || true
  exit "$result"
}
trap cleanup EXIT HUP INT TERM

docker run --detach --rm --privileged --name "$daemon" \
  --publish 127.0.0.1::2375 --add-host host.docker.internal:host-gateway \
  --entrypoint dockerd "$image" \
  --host=unix:///var/run/docker.sock --host=tcp://0.0.0.0:2375 \
  --tls=false --bip=172.28.0.1/16 >/dev/null
deadline=$((SECONDS + 120))
until docker exec "$daemon" docker info >/dev/null 2>&1; do
  if [[ $SECONDS -ge $deadline ]]; then
    echo 'The isolated Docker daemon did not become ready.' >&2
    exit 1
  fi
  sleep 0.25
done

bindings=$(docker inspect --format '{{json .HostConfig.Binds}}' "$daemon")
if [[ "$bindings" != null ]]; then
  echo "The isolated daemon unexpectedly has host bind mounts: $bindings" >&2
  exit 1
fi
port=$(docker port "$daemon" 2375/tcp | sed -n 's/^127\.0\.0\.1://p')
address=$(docker exec "$daemon" getent ahostsv4 host.docker.internal | awk 'NR == 1 {print $1}')
if [[ -z "$port" || -z "$address" ]]; then
  echo 'The remote daemon endpoint or Petri host address could not be resolved.' >&2
  exit 1
fi

# Stream the cached image into the isolated daemon. No filesystem is shared,
# and the test needs no registry access after these two pinned image pulls.
docker pull "$runner" >/dev/null
docker save "$runner" | docker exec -i "$daemon" docker load >/dev/null
DOCKER_HOST="tcp://127.0.0.1:$port" \
  PETRI_SANDBOX_DOCKER_HOST_ADDRESS="$address" PETRI_REQUIRE_DOCKER=1 \
  cargo nextest run --locked -p petri-github-acceptance --test artifacts_e2e \
    -E 'test(=artifacts_flow_across_containerized_jobs)'
echo 'Remote Docker artifact upload and download passed without shared host files.'
