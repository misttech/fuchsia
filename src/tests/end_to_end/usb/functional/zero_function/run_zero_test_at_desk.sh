#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
COMMON_SCRIPT="${SCRIPT_DIR}/../../lib/zero_function/run_zero_test_common.sh"
BASE_PKG="//src/tests/end_to_end/usb/functional/zero_function"

declare -A TEST_TARGETS=(
    ["zero_function_test"]="${BASE_PKG}:zero_function_test"
    ["scatter_gather_test"]="${BASE_PKG}:scatter_gather_test"
)

TEST_NAME=""
PASSTHROUGH_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --test-target)
            TEST_NAME="${2:-}"
            shift 2
            ;;
        --)
            PASSTHROUGH_ARGS+=("$@")
            break
            ;;
        *)
            PASSTHROUGH_ARGS+=("$1")
            shift
            ;;
    esac
done

if [[ -z "${TEST_NAME}" || -z "${TEST_TARGETS[${TEST_NAME}]+_}" ]]; then
    echo "[!] ERROR: --test-target <name> is required. Available tests: ${!TEST_TARGETS[*]}" >&2
    exit 1
fi

exec "${COMMON_SCRIPT}" \
    --caller "$0" \
    --test-name "${TEST_NAME}" \
    --test-target "${TEST_TARGETS[${TEST_NAME}]}" \
    "${PASSTHROUGH_ARGS[@]}"
