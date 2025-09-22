#!/bin/bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# rsninja.sh (this script) is a drop-in replacement for a ninja binary.
#
# rs-ninja.sh (from rsclient package) enables ResultStore features in ninja
#   when it detects RSPROXY_FIFO in the environment (any intermediate
#   ninja wrappers should take care to pass-through this variable).
#   Otherwise, it falls back to the original ninja invocation.

# Using this script as a level of indirection allows the communication
# mechanism between rsproxy-wrap.sh and rs-ninja.sh (from rsclient) to
# change without breaking the build.

set -euo pipefail

readonly SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"

# Get the HOST_PLATFORM for the prebuilt path.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

# rsclient install path is set in manifests/prebuilts
readonly PREBUILT_RSCLIENT_DIR="prebuilt/rsclient/$HOST_PLATFORM"
readonly wrapper="$PREBUILT_RSCLIENT_DIR/bin/rs-ninja.sh"
readonly ninja_bin="$PREBUILT_NINJA"

if [[ -x "$wrapper" ]]; then
  exec "$wrapper" "$ninja_bin" "$@"
else
  exec "$ninja_bin" "$@"
fi

