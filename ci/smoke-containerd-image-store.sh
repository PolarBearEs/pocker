#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-target/debug/pocker}"
REGISTRY_PORT="${CONTAINERD_REGISTRY_PORT:-5004}"
REGISTRY_NAME="pocker-containerd-registry-${REGISTRY_PORT}"
REF="localhost:${REGISTRY_PORT}/pocker/config-update:latest"
OLD_REF="localhost:${REGISTRY_PORT}/pocker/config-update:old"
WORKDIR="$(mktemp -d)"

cleanup() {
  docker rm -f "${REGISTRY_NAME}" >/dev/null 2>&1 || true
  docker image rm -f "${REF}" "${OLD_REF}" >/dev/null 2>&1 || true
  rm -rf "${WORKDIR}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

wait_for_registry() {
  for _ in $(seq 1 60); do
    local status
    status="$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:${REGISTRY_PORT}/v2/" || true)"
    if [[ "${status}" == "200" ]]; then
      return 0
    fi
    sleep 1
  done
  echo "registry on port ${REGISTRY_PORT} did not become ready" >&2
  return 1
}

driver_status="$(docker info -f '{{json .DriverStatus}}')"
if [[ "${driver_status}" != *"driver-type"*"io.containerd.snapshotter.v1"* ]]; then
  echo "Docker is not using the containerd image store: ${driver_status}" >&2
  exit 1
fi

docker run -d --name "${REGISTRY_NAME}" -p "${REGISTRY_PORT}:5000" registry:2 >/dev/null
wait_for_registry

cat >"${WORKDIR}/Dockerfile" <<'EOF'
FROM alpine:3.20
ARG REVISION
LABEL io.pocker.test.revision="${REVISION}"
EOF

docker build --build-arg REVISION=old -t "${OLD_REF}" "${WORKDIR}" >/dev/null
old_id="$(docker image inspect "${OLD_REF}" --format '{{.Id}}')"
old_layers="$(docker image inspect "${OLD_REF}" --format '{{json .RootFS.Layers}}')"

docker build --build-arg REVISION=new -t "${REF}" "${WORKDIR}" >/dev/null
new_id="$(docker image inspect "${REF}" --format '{{.Id}}')"
new_layers="$(docker image inspect "${REF}" --format '{{json .RootFS.Layers}}')"

if [[ "${old_id}" == "${new_id}" ]]; then
  echo "config-only test images unexpectedly have the same image ID" >&2
  exit 1
fi
if [[ "${old_layers}" != "${new_layers}" ]]; then
  echo "config-only test images unexpectedly have different filesystem layers" >&2
  exit 1
fi

docker push "${REF}" >/dev/null
docker image tag "${OLD_REF}" "${REF}"
if [[ "$(docker image inspect "${REF}" --format '{{.Id}}')" != "${old_id}" ]]; then
  echo "failed to put the old config at the local test reference" >&2
  exit 1
fi

echo "containerd smoke: import a new config using existing filesystem layers"
first_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache" \
    pull \
    --plain-http \
    --no-animations \
    "${REF}" 2>&1
)"
printf '%s\n' "${first_output}"

if [[ "$(docker image inspect "${REF}" --format '{{.Id}}')" != "${new_id}" ]]; then
  echo "pocker did not replace the old image config with the registry config" >&2
  exit 1
fi
if [[ "${first_output}" != *"Already exists in Docker daemon"* ]]; then
  echo "pocker did not reuse the matching daemon filesystem layers" >&2
  exit 1
fi

echo "containerd smoke: skip an image whose config is already loaded"
second_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache" \
    pull \
    --plain-http \
    --no-animations \
    "${REF}" 2>&1
)"
printf '%s\n' "${second_output}"

if [[ "${second_output}" != *"image ${REF}: Already exists"* ]]; then
  echo "pocker reloaded an image whose config ID already matched" >&2
  exit 1
fi

echo "containerd image store smoke checks passed"
