#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# rs-sub-ninja.sh is a drop-in replacement for ninja that should be
# used in sub-builds when resultstore is enabled.
# This combines just rsproxy-wrap.sh and rsninja.sh.
# The reason for this is that each ninja invocation must be paired
# with its own rsproxy process; rsproxy was only designed for the
# single-invocation use case.
# If you need to inject other wrappers between rsproxy-wrap.sh
# and ninja, then compose your own command from these components.

set -euo pipefail

readonly SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"

readonly fuchsia_rsproxy_wrap="$SCRIPT_DIR/fuchsia-rsproxy-wrap.sh"
readonly rsninja="$SCRIPT_DIR/rsninja.sh"

full_cmd=(
  "$fuchsia_rsproxy_wrap"
  --
  "$rsninja"
  "$@"
)

exec "${full_cmd[@]}"
