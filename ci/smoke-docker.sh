#!/usr/bin/env bash
set -euo pipefail

BIN="${1:-target/debug/pocker}"
PUBLIC_REF="registry.k8s.io/pause:3.9"
RESUME_REGISTRY_PORT="${RESUME_REGISTRY_PORT:-5002}"
AUTH_REGISTRY_PORT="${AUTH_REGISTRY_PORT:-5003}"
RESUME_REF="localhost:${RESUME_REGISTRY_PORT}/pocker/resume:latest"
PRIVATE_REF="localhost:${AUTH_REGISTRY_PORT}/pocker/private:latest"
AUTH_USER="smoke"
AUTH_PASSWORD="smoke-password"
WORKDIR="$(mktemp -d)"
RESUME_REGISTRY_NAME="pocker-smoke-registry-${RESUME_REGISTRY_PORT}"
AUTH_REGISTRY_NAME="pocker-smoke-registry-${AUTH_REGISTRY_PORT}"

cleanup() {
  docker rm -f "${RESUME_REGISTRY_NAME}" "${AUTH_REGISTRY_NAME}" >/dev/null 2>&1 || true
  docker run --rm -v "${WORKDIR}:/work" alpine sh -c 'rm -rf /work/*' >/dev/null 2>&1 || true
  rm -rf "${WORKDIR}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required tool: $1" >&2
    exit 1
  }
}

wait_for_registry() {
  local port="$1"
  for _ in $(seq 1 60); do
    local status
    status="$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:${port}/v2/" || true)"
    if [[ "${status}" == "200" || "${status}" == "401" ]]; then
      return 0
    fi
    sleep 1
  done
  echo "registry on port ${port} did not become ready" >&2
  return 1
}

run_pocker() {
  "${BIN}" "$@"
}

require_tool docker
require_tool curl
require_tool python3

mkdir -p "${WORKDIR}"

echo "smoke: public pull into Docker"
docker image rm -f "${PUBLIC_REF}" >/dev/null 2>&1 || true
run_pocker --cache-dir "${WORKDIR}/cache-public" pull "${PUBLIC_REF}"
docker image inspect "${PUBLIC_REF}" >/dev/null
run_pocker image inspect "${PUBLIC_REF}" >/dev/null

echo "smoke: local registry load mode"
docker image rm -f "${PUBLIC_REF}" >/dev/null 2>&1 || true
docker images --format '{{.Repository}}:{{.Tag}}' | grep 'pocker-cache' | xargs -r docker image rm -f >/dev/null 2>&1 || true
run_pocker --cache-dir "${WORKDIR}/cache-registry-load" pull --load-mode registry "${PUBLIC_REF}"
docker image inspect "${PUBLIC_REF}" >/dev/null
if docker images --format '{{.Repository}}:{{.Tag}}' | grep -q 'pocker-cache'; then
  echo "temporary local registry image tag was not cleaned up" >&2
  exit 1
fi

echo "smoke: compose config and service-filtered pull"
mkdir -p "${WORKDIR}/compose"
cat > "${WORKDIR}/compose/.env" <<EOF
PAUSE_REF=${PUBLIC_REF}
EOF
cat > "${WORKDIR}/compose/compose.base.yml" <<'EOF'
services:
  base:
    image: $PAUSE_REF
EOF
cat > "${WORKDIR}/compose/compose.include.yml" <<'EOF'
services:
  included:
    image: ${PAUSE_REF}
EOF
cat > "${WORKDIR}/compose/custom-compose.yml" <<'EOF'
include:
  - compose.include.yml

services:
  target:
    extends:
      file: compose.base.yml
      service: base

  duplicate:
    extends:
      service: target

  build_only:
    build: .
EOF

COMPOSE_FILE="${WORKDIR}/compose/custom-compose.yml"
CONFIG_IMAGES_OUTPUT="$(run_pocker compose -f "${COMPOSE_FILE}" config --images target)"
if [[ "${CONFIG_IMAGES_OUTPUT}" != "${PUBLIC_REF}" ]]; then
  echo "unexpected compose config --images output: ${CONFIG_IMAGES_OUTPUT}" >&2
  exit 1
fi
CONFIG_SERVICES_OUTPUT="$(run_pocker compose -f "${COMPOSE_FILE}" config --services target)"
if [[ "${CONFIG_SERVICES_OUTPUT}" != "target" ]]; then
  echo "unexpected compose config --services output: ${CONFIG_SERVICES_OUTPUT}" >&2
  exit 1
fi
CONFIG_ALL_IMAGES_OUTPUT="$(run_pocker compose -f "${COMPOSE_FILE}" config --images)"
if [[ "${CONFIG_ALL_IMAGES_OUTPUT}" != "${PUBLIC_REF}" ]]; then
  echo "unexpected deduped compose config --images output: ${CONFIG_ALL_IMAGES_OUTPUT}" >&2
  exit 1
fi
set +e
UNKNOWN_OUTPUT="$(run_pocker compose -f "${COMPOSE_FILE}" config --images missing 2>&1)"
UNKNOWN_STATUS=$?
set -e
if [[ "${UNKNOWN_STATUS}" -eq 0 ]]; then
  echo "expected unknown compose service to fail" >&2
  exit 1
fi
grep -q "compose service(s) not found: missing" <<<"${UNKNOWN_OUTPUT}" || {
  echo "missing unknown compose service error" >&2
  echo "${UNKNOWN_OUTPUT}" >&2
  exit 1
}
run_pocker --cache-dir "${WORKDIR}/cache-compose" compose -f "${COMPOSE_FILE}" pull --no-load target
find "${WORKDIR}/cache-compose/blobs/sha256" -type f | grep -q .

echo "smoke: compose parser oracle"
ORACLE_DIR="${WORKDIR}/compose-oracle"
mkdir -p "${ORACLE_DIR}/base" "${ORACLE_DIR}/include"
cat > "${ORACLE_DIR}/.env" <<'EOF'
REGISTRY=registry.example.com
TAG=1.2.3
EMPTY=
FROM_ENV_FILE=dotenv
EOF
cat > "${ORACLE_DIR}/base/base.yml" <<'EOF'
services:
  base:
    image: ${REGISTRY}/base:${BASE_TAG:-base-default}
  nested:
    image: ${REGISTRY}/nested:${NESTED_TAG:-${TAG:-fallback}}
EOF
cat > "${ORACLE_DIR}/include/included.yml" <<'EOF'
services:
  included:
    image: ${REGISTRY}/included:${TAG}
  included_build:
    build: .
EOF
cat > "${ORACLE_DIR}/compose.yml" <<'EOF'
include:
  - path: include/included.yml

x-image: &app_image ${REGISTRY}/app:${TAG}

services:
  app:
    image: *app_image
    labels:
      "$REGISTRY.key": should_not_interpolate_key
      interpolated.value: "$REGISTRY/value"

  child:
    extends:
      file: base/base.yml
      service: nested

  override_me:
    image: ${REGISTRY}/old:${TAG}

  build_only:
    build: .

  both:
    image: ${REGISTRY}/both:${FROM_ENV_FILE}
    build: .
EOF
cat > "${ORACLE_DIR}/compose.override.yml" <<'EOF'
services:
  override_me:
    image: ${REGISTRY}/new:${EMPTY:-override-default}
EOF
DOCKER_COMPOSE_CONFIG="$(
  cd "${ORACLE_DIR}" && docker compose -f compose.yml -f compose.override.yml config --format json
)"
POCKER_COMPOSE_CONFIG="$(
  run_pocker \
    compose \
    -f "${ORACLE_DIR}/compose.yml" \
    -f "${ORACLE_DIR}/compose.override.yml" \
    config \
    --format json
)"
python3 - "${DOCKER_COMPOSE_CONFIG}" "${POCKER_COMPOSE_CONFIG}" <<'PY'
import json
import sys

docker_config = json.loads(sys.argv[1])
pocker_config = json.loads(sys.argv[2])

docker_services = docker_config["services"]
pocker_services = {service["name"]: service for service in pocker_config["services"]}

expected_service_names = {
    "app",
    "child",
    "override_me",
    "build_only",
    "both",
    "included",
    "included_build",
}
if set(pocker_services) != expected_service_names:
    raise SystemExit(f"unexpected pocker services: {sorted(pocker_services)}")
if set(docker_services) != expected_service_names:
    raise SystemExit(f"unexpected docker services: {sorted(docker_services)}")

expected_images = {
    "app": "registry.example.com/app:1.2.3",
    "child": "registry.example.com/nested:1.2.3",
    "override_me": "registry.example.com/new:override-default",
    "both": "registry.example.com/both:dotenv",
    "included": "registry.example.com/included:1.2.3",
}
for service, image in expected_images.items():
    docker_image = docker_services[service].get("image")
    pocker_image = pocker_services[service].get("image")
    if docker_image != image:
        raise SystemExit(f"unexpected docker image for {service}: {docker_image}")
    if pocker_image != image:
        raise SystemExit(f"unexpected pocker image for {service}: {pocker_image}")
    if pocker_services[service].get("build_only"):
        raise SystemExit(f"pocker marked pullable service as build-only: {service}")

for service in ["build_only", "included_build"]:
    if pocker_services[service].get("image") is not None:
        raise SystemExit(f"pocker assigned image to build-only service: {service}")
    if not pocker_services[service].get("build_only"):
        raise SystemExit(f"pocker did not mark build-only service: {service}")

expected_image_list = list(expected_images.values())
if pocker_config["images"] != expected_image_list:
    raise SystemExit(f"unexpected pocker images: {pocker_config['images']}")
if pocker_config["skipped_build_only"] != ["build_only", "included_build"]:
    raise SystemExit(
        f"unexpected pocker build-only services: {pocker_config['skipped_build_only']}"
    )

labels = docker_services["app"].get("labels", {})
if "$REGISTRY.key" not in labels and "$$REGISTRY.key" not in labels:
    raise SystemExit(f"docker unexpectedly interpolated label key: {labels}")
pocker_labels = pocker_services["app"].get("labels", {})
if pocker_labels.get("$REGISTRY.key") != "should_not_interpolate_key":
    raise SystemExit(f"pocker unexpectedly interpolated label key: {pocker_labels}")
if pocker_labels.get("interpolated.value") != "registry.example.com/value":
    raise SystemExit(f"pocker did not interpolate label value: {pocker_labels}")
PY

echo "smoke: multi-platform pull selects linux/arm64 config"
run_pocker --cache-dir "${WORKDIR}/cache-platform" pull --no-load --platform linux/arm64 "${PUBLIC_REF}"
python3 - "${WORKDIR}/cache-platform" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]) / "blobs" / "sha256"
for path in root.glob("*"):
    try:
        data = json.loads(path.read_text())
    except Exception:
        continue
    if data.get("architecture") == "arm64" and data.get("os") == "linux":
        sys.exit(0)
raise SystemExit("missing linux/arm64 image config in cache")
PY

echo "smoke: image save/load round-trip"
ARCHIVE_PATH="${WORKDIR}/pause.tar"
run_pocker image save "${PUBLIC_REF}" --output "${ARCHIVE_PATH}"
docker image rm -f "${PUBLIC_REF}" >/dev/null
run_pocker image load --input "${ARCHIVE_PATH}"
docker image inspect "${PUBLIC_REF}" >/dev/null

echo "smoke: resumable download resumes from partial cache"
docker run -d --name "${RESUME_REGISTRY_NAME}" -p "${RESUME_REGISTRY_PORT}:5000" registry:2 >/dev/null
wait_for_registry "${RESUME_REGISTRY_PORT}"
mkdir -p "${WORKDIR}/resume-image"
dd if=/dev/urandom of="${WORKDIR}/resume-image/payload.bin" bs=1M count=512 status=none
cat > "${WORKDIR}/resume-image/Dockerfile" <<'EOF'
FROM alpine:3.20
COPY payload.bin /payload.bin
EOF
docker build -t "${RESUME_REF}" "${WORKDIR}/resume-image" >/dev/null
docker push "${RESUME_REF}" >/dev/null
docker image rm -f "${RESUME_REF}" >/dev/null 2>&1 || true

set +e
timeout --signal=KILL 5s "${BIN}" --cache-dir "${WORKDIR}/cache-resume" pull --no-load --plain-http --max-parallel-downloads 1 "${RESUME_REF}"
STATUS=$?
set -e
if [[ "${STATUS}" -ne 124 && "${STATUS}" -ne 137 && "${STATUS}" -ne 1 ]]; then
  echo "expected interrupted pull during resume smoke test, got status ${STATUS}" >&2
  exit 1
fi
PARTIAL_PATH="$(find "${WORKDIR}/cache-resume/partials/sha256" -type f -name '*.part' -size +10485760c | head -n 1 || true)"
if [[ -z "${PARTIAL_PATH}" ]]; then
  echo "failed to observe a partial download before timeout interruption" >&2
  exit 1
fi
if [[ ! -s "${PARTIAL_PATH}" ]]; then
  echo "expected non-empty partial layer after interrupt" >&2
  exit 1
fi
run_pocker --cache-dir "${WORKDIR}/cache-resume" pull --no-load --plain-http --max-parallel-downloads 1 "${RESUME_REF}"
find "${WORKDIR}/cache-resume/blobs/sha256" -type f | grep -q .

echo "smoke: private registry auth via stdin and Docker config"
mkdir -p "${WORKDIR}/auth-registry-data"
docker run -d \
  --name "${AUTH_REGISTRY_NAME}" \
  -p "${AUTH_REGISTRY_PORT}:5000" \
  -v "${WORKDIR}/auth-registry-data:/var/lib/registry" \
  registry:2 >/dev/null
wait_for_registry "${AUTH_REGISTRY_PORT}"
docker tag "${PUBLIC_REF}" "${PRIVATE_REF}"
docker push "${PRIVATE_REF}" >/dev/null
docker rm -f "${AUTH_REGISTRY_NAME}" >/dev/null

docker run --rm --entrypoint htpasswd httpd:2 -Bbn "${AUTH_USER}" "${AUTH_PASSWORD}" > "${WORKDIR}/htpasswd"
docker run -d \
  --name "${AUTH_REGISTRY_NAME}" \
  -p "${AUTH_REGISTRY_PORT}:5000" \
  -v "${WORKDIR}/auth-registry-data:/var/lib/registry" \
  -v "${WORKDIR}/htpasswd:/auth/htpasswd:ro" \
  -e REGISTRY_AUTH=htpasswd \
  -e REGISTRY_AUTH_HTPASSWD_REALM="Registry Realm" \
  -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd \
  registry:2 >/dev/null
wait_for_registry "${AUTH_REGISTRY_PORT}"

mkdir -p "${WORKDIR}/docker-config-auth"
AUTH_B64="$(printf "%s:%s" "${AUTH_USER}" "${AUTH_PASSWORD}" | base64 | tr -d '\n')"
cat > "${WORKDIR}/docker-config-auth/config.json" <<EOF
{
  "auths": {
    "localhost:${AUTH_REGISTRY_PORT}": {
      "auth": "${AUTH_B64}"
    }
  }
}
EOF

printf "%s" "${AUTH_PASSWORD}" | \
  run_pocker --cache-dir "${WORKDIR}/cache-private-stdin" pull --no-load --plain-http --username "${AUTH_USER}" --password-stdin "${PRIVATE_REF}"
DOCKER_CONFIG="${WORKDIR}/docker-config-auth" \
  run_pocker --cache-dir "${WORKDIR}/cache-private-config" pull --no-load --plain-http "${PRIVATE_REF}"

echo "smoke: docker workflow checks passed"
