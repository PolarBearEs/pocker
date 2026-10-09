#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-target/debug/pocker}"
REGISTRY_PORT="${CONTAINERD_REGISTRY_PORT:-5004}"
REGISTRY_NAME="pocker-containerd-registry-${REGISTRY_PORT}"
REF="localhost:${REGISTRY_PORT}/pocker/config-update:latest"
OLD_REF="localhost:${REGISTRY_PORT}/pocker/config-update:old"
DOCKER_PULLED_REF="alpine:3.20"
BASE_REF="localhost:${REGISTRY_PORT}/pocker/unexportable-base:latest"
CHILD_REF="localhost:${REGISTRY_PORT}/pocker/unexportable-child:latest"
WORKDIR="$(mktemp -d)"

cleanup() {
  docker rm -f "${REGISTRY_NAME}" >/dev/null 2>&1 || true
  docker image rm -f "${REF}" "${OLD_REF}" "${BASE_REF}" "${CHILD_REF}" >/dev/null 2>&1 || true
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

# Removes a blob from the daemon's containerd content store while keeping its
# unpacked snapshot. Set POCKER_SMOKE_CTR to a ctr command already bound to the
# daemon's containerd (for example `docker exec <dind> ctr -a <socket>`);
# otherwise the socket of the containerd holding the blob is located.
remove_daemon_content() {
  local digest="$1"
  if [[ -n "${POCKER_SMOKE_CTR:-}" ]]; then
    ${POCKER_SMOKE_CTR} -n moby content rm "${digest}" >/dev/null
    return
  fi
  # Prefer the ctr shipped next to dockerd so its version matches the daemon's
  # containerd; resolve an absolute path because sudo resets PATH.
  local ctr dockerd
  dockerd="$(command -v dockerd || true)"
  if [[ -n "${dockerd}" && -x "$(dirname "${dockerd}")/ctr" ]]; then
    ctr="$(dirname "${dockerd}")/ctr"
  else
    ctr="$(command -v ctr || true)"
  fi
  if [[ -z "${ctr}" ]]; then
    echo "ctr is required to remove containerd content; set POCKER_SMOKE_CTR" >&2
    return 1
  fi
  local socket
  while IFS= read -r socket; do
    if sudo "${ctr}" -a "${socket}" -n moby content info "${digest}" >/dev/null 2>&1; then
      sudo "${ctr}" -a "${socket}" -n moby content rm "${digest}" >/dev/null
      return
    fi
  done < <(sudo find /run /var/run /tmp "${RUNNER_TEMP:-/tmp}" -name containerd.sock -type s 2>/dev/null | sort -u)
  echo "could not find the containerd socket holding ${digest}" >&2
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
set +e
first_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache" \
    pull \
    --plain-http \
    --no-animations \
    "${REF}" 2>&1
)"
first_status=$?
set -e
printf '%s\n' "${first_output}"
if [[ "${first_status}" -ne 0 ]]; then
  echo "pocker failed while importing the config-only update (exit ${first_status})" >&2
  exit "${first_status}"
fi

loaded_revision="$(docker image inspect "${REF}" --format '{{index .Config.Labels "io.pocker.test.revision"}}')"
if [[ "${loaded_revision}" != "new" ]]; then
  loaded_id="$(docker image inspect "${REF}" --format '{{.Id}}')"
  echo "pocker did not replace the old image config with the registry config: expected revision new, got ${loaded_revision:-<none>} (image ${loaded_id})" >&2
  exit 1
fi
if [[ "${first_output}" != *"Already exists in Docker daemon"* ]]; then
  echo "pocker did not reuse the matching daemon filesystem layers" >&2
  exit 1
fi

echo "containerd smoke: skip an image whose config is already loaded"
set +e
second_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache" \
    pull \
    --plain-http \
    --no-animations \
    "${REF}" 2>&1
)"
second_status=$?
set -e
printf '%s\n' "${second_output}"
if [[ "${second_status}" -ne 0 ]]; then
  echo "pocker failed while checking the already-loaded config (exit ${second_status})" >&2
  exit "${second_status}"
fi

if [[ "${second_output}" != *"image ${REF}: Already exists"* ]]; then
  echo "pocker reloaded an image whose config ID already matched" >&2
  exit 1
fi

echo "containerd smoke: skip an image Docker pulled itself"
# Docker reports the index digest as the image ID here, and the platform
# manifest pocker resolves only appears in the inspect Manifests list.
docker pull "${DOCKER_PULLED_REF}" >/dev/null
set +e
docker_pulled_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache-docker-pulled" \
    pull \
    --no-animations \
    "${DOCKER_PULLED_REF}" 2>&1
)"
docker_pulled_status=$?
set -e
printf '%s\n' "${docker_pulled_output}"
if [[ "${docker_pulled_status}" -ne 0 ]]; then
  echo "pocker failed while checking an image Docker pulled itself (exit ${docker_pulled_status})" >&2
  exit "${docker_pulled_status}"
fi
if [[ "${docker_pulled_output}" != *"Already exists"* || "${docker_pulled_output}" == *"Loading"* ]]; then
  echo "pocker reloaded an image Docker had already pulled" >&2
  exit 1
fi

echo "containerd smoke: download layers Docker can no longer export"
# Containerd may keep only the unpacked snapshot of a layer. Docker still lists
# it in RootFS, but `docker save` silently leaves the blob out of the archive,
# so pocker must fall back to the registry for it.
docker image tag "${DOCKER_PULLED_REF}" "${BASE_REF}"
docker push "${BASE_REF}" >/dev/null
printf 'FROM %s\nLABEL io.pocker.test.child="true"\n' "${BASE_REF}" >"${WORKDIR}/Dockerfile.child"
docker build -q -f "${WORKDIR}/Dockerfile.child" -t "${CHILD_REF}" "${WORKDIR}" >/dev/null
docker push "${CHILD_REF}" >/dev/null
docker image rm "${CHILD_REF}" "${BASE_REF}" "${DOCKER_PULLED_REF}" >/dev/null
docker pull "${BASE_REF}" >/dev/null

base_manifest="$(docker image inspect "${BASE_REF}" --format '{{.Id}}')"
base_layer_blobs="$(
  curl -fsS \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
    -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
    "http://localhost:${REGISTRY_PORT}/v2/pocker/unexportable-base/manifests/${base_manifest}" |
    python3 -c 'import json, sys; print("\n".join(layer["digest"] for layer in json.load(sys.stdin)["layers"]))'
)"
while IFS= read -r blob; do
  remove_daemon_content "${blob}"
done <<<"${base_layer_blobs}"

docker save "${BASE_REF}" -o "${WORKDIR}/base.tar"
first_blob="$(head -n 1 <<<"${base_layer_blobs}")"
if tar -tf "${WORKDIR}/base.tar" | grep -q "${first_blob#sha256:}"; then
  echo "docker save still exported a layer blob removed from containerd" >&2
  exit 1
fi

set +e
unexportable_output="$(
  "${BIN}" \
    --cache-dir "${WORKDIR}/cache-unexportable" \
    pull \
    --plain-http \
    --no-animations \
    "${CHILD_REF}" 2>&1
)"
unexportable_status=$?
set -e
printf '%s\n' "${unexportable_output}"
if [[ "${unexportable_status}" -ne 0 ]]; then
  echo "pocker failed when Docker could not export reused layers (exit ${unexportable_status})" >&2
  exit "${unexportable_status}"
fi
if [[ "${unexportable_output}" != *"downloading them from the registry"* ]]; then
  echo "pocker did not fall back to the registry for unexportable layers" >&2
  exit 1
fi
if [[ "$(docker image inspect "${CHILD_REF}" --format '{{index .Config.Labels "io.pocker.test.child"}}')" != "true" ]]; then
  echo "pocker did not load the child image" >&2
  exit 1
fi
docker run --rm "${CHILD_REF}" true

echo "containerd image store smoke checks passed"
