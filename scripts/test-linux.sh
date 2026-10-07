#!/usr/bin/env bash
# Run scinit's test suite on Linux inside rootless podman.
#
#   scripts/test-linux.sh                       # all tests
#   scripts/test-linux.sh --test integration_test linux::
#
# Arguments are passed through to `cargo test`. Builds the test image
# (tests/container/Containerfile) first; the crate registry and target dir live
# in build cache mounts, so only what changed recompiles.
#
# Environment:
#   SCINIT_TEST_IMAGE       image tag (default: scinit-test:latest)
#   SCINIT_PODMAN_RUN_ARGS  extra `podman run` flags
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image="${SCINIT_TEST_IMAGE:-scinit-test:latest}"

log() { printf '\n==> %s\n' "$*"; }

log "Building $image"
podman build \
    -f "$repo/tests/container/Containerfile" \
    --ignorefile "$repo/tests/container/containerignore" \
    -t "$image" \
    "$repo"

log "cargo test $*"
# The linux.rs PID-1 tests run scinit as PID 1 of a nested PID namespace
# (`unshare --pid --mount-proc`). That needs SYS_ADMIN (scoped to the
# rootless user namespace), an unmasked /proc, and no SELinux label, which
# otherwise denies mounting the fresh /proc. SCINIT_REQUIRE_PID1 turns
# their "can't create a namespace" skip into a failure.
# shellcheck disable=SC2086
exec podman run --rm --init=false \
    --cap-add SYS_ADMIN \
    --security-opt unmask=ALL \
    --security-opt label=disable \
    -e SCINIT_REQUIRE_PID1=1 \
    ${SCINIT_PODMAN_RUN_ARGS:-} "$image" cargo test "$@"
