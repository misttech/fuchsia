# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This script needs to be safe and silent for users who don't have mise installed.
# Leave user shells alone, we only want to modify shells for coding agents.
if [[ -n "${ANTIGRAVITY_EDITOR_APP_ROOT}" || -n "${ANTIGRAVITY_AGENT}" ]]; then
  if which mise > /dev/null; then
    eval "$(mise activate)"
  fi
fi
