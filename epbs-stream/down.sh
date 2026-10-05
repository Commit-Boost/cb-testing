#!/usr/bin/env bash
# Remove the harness enclave and the packet-capture sidecar. Keeps RUN_DIR (evidence) and images.
set -uo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

docker container inspect "$CAPTURE_NAME" >/dev/null 2>&1 && docker rm -f "$CAPTURE_NAME" >/dev/null && echo "removed $CAPTURE_NAME"
kurtosis enclave rm -f "$ENCLAVE" 2>&1 | tail -1
kurtosis enclave ls
