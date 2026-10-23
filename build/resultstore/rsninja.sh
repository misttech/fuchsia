#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

readonly SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"

# Get the HOST_PLATFORM for the prebuilt path.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

# rsclient install path is set in manifests/prebuilts
readonly PREBUILT_RSCLIENT_DIR="${FUCHSIA_DIR}/prebuilt/rsclient/$HOST_PLATFORM"
readonly wrapper="$PREBUILT_RSCLIENT_DIR/bin/rs-ninja.sh"
readonly ninja_bin="$PREBUILT_NINJA"

# The RSPROXY_FIFO check is redundant with the rs-ninja.sh wrapper script, but
# harmless. Checking here lets us bypass unnecessary setup.
if [[ -z "${RSPROXY_FIFO+x}" ]]; then
  exec "$ninja_bin" "$@"
fi

if [[ ! -x "$wrapper" ]]; then
  echo "Error: $wrapper is not executable, falling back to using $ninja_bin."
  exec "$ninja_bin" "$@"
fi

exec "${PREBUILT_PYTHON3}" -S "${SCRIPT_DIR}/rsninja.py" \
  --wrapper="$wrapper" \
  --ninja-bin="$ninja_bin" \
  -- "$@"
