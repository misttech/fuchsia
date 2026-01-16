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

# Get the HOST_PLATFORM for the prebuilt path.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

# rsclient install path is set in manifests/prebuilts
readonly PREBUILT_RSCLIENT_DIR="${FUCHSIA_DIR}/prebuilt/rsclient/$HOST_PLATFORM"
readonly proxy_wrap="$PREBUILT_RSCLIENT_DIR/bin/rsproxy-wrap.sh"
readonly rsproxy="$PREBUILT_RSCLIENT_DIR/bin/rsproxy"
readonly rsninja="$SCRIPT_DIR/rsninja.sh"

# TODO(b/473907403): for infra builds, plumb remote service proxy overrides:
#   BAZEL_resultstore_socket_path -> RS_rs_service
#   BAZEL_rbe_socket_path -> RS_cas_service
# These will take precedence over values in .cfg files.
# All sub-invocations can share the same sockets.

# TODO(b/465157948): inject build metadata that reflects any parent
# invocations coming from bazel or ninja, as well as the top-level
# FX_BUILD_UUID or BUILDBUCKET_ID.  Set SUBBUILDS_LINK for this invocation,
# and set PARENT_BUILD_LINK in the environment for sub-invocations.

# TODO: scan ninja options for flags that produce extra output files,
# such as traces.  Convert these into rsproxy invocation artifacts
# as post-build uploads.

# FX_BUILD_LOAS_TYPE is set by 'fx build' to either "restricted" or
# "unrestricted", and influences authentication method.
# Infra builds don't set this, but instead pass environment variables
# that will override the cfg values.
CFG="$SCRIPT_DIR/fuchsia-resultstore.cfg"
[[ "${FX_BUILD_LOAS_TYPE:-NOT_SET}" != "NOT_SET" ]] || {
  case "$FX_BUILD_LOAS_TYPE" in
    unrestricted) CFG="$SCRIPT_DIR/fuchsia-resultstore-gcertauth.cfg" ;;
  esac
}

rsproxy_options=(
  --cfg "$CFG"
)

# Give sub-ninja a new invocation id.
# This is different from FX_BUILD_UUID.
readonly invocation_id="$("${PREBUILT_PYTHON3}" -S -c 'import uuid; print(uuid.uuid4())')"

full_cmd=(
  env
  NINJA_BUILD_ID="$invocation_id"
  "$proxy_wrap"
  --rsproxy "$rsproxy"
  # TODO: point --log-dir under _build_logs.  Currently defaults to /tmp.
  --rsproxy_options
  "${rsproxy_options[@]}"
  --
  "$rsninja" "$@"
)
exec "${full_cmd[@]}"
