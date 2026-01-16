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

# See also rs-sub-ninja.sh, which a suitable drop-in replacement
# that includes rsproxy-wrap.sh.
# Use rsninja.sh when there is a need to insert other wrappers
# between rsproxy-wrap.sh and ninja (like in `fx build`).

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
# harmless.  Checking here lets us bypass unnecessary setup.
if [[ -n "${RSPROXY_FIFO-}" ]] && [[ -x "$wrapper" ]]; then
  # Give sub-ninja a new invocation id.
  # This is different from FX_BUILD_UUID.
  if [[ -n "${FX_BUILD_UUID}" ]] && [[ "${PARENT_BUILD_ID:-NOT_SET}" == "NOT_SET" ]]
  then
    # This is a top-level ninja invocation.
    # Use FX_BUILD_UUID as the ninja invocation id, but not BUILDBUCKET_ID.
    readonly invocation_id="$FX_BUILD_UUID"
  else
    readonly invocation_id="$("${PREBUILT_PYTHON3}" -S -c 'import uuid; print(uuid.uuid4())')"
  fi

  # Set additional build metadata based on environment variables
  # set by parent invocations.
  metadata=()
  [[ "${FX_BUILD_UUID:-NOT_SET}" == "NOT_SET" ]] || {
    metadata+=( FX_BUILD_UUID="$FX_BUILD_UUID" )
  }
  [[ "${BUILDBUCKET_ID:-NOT_SET}" == "NOT_SET" ]] || {
    metadata+=( BUILDBUCKET_ID="$BUILDBUCKET_ID" )
  }

  # The following fields are passed from parent-to-child invocation.
  # If not set (expected for top-level invocations), just ignore.
  # See ninja_env below.
  [[ "${PARENT_BUILD_ID:-NOT_SET}" == "NOT_SET" ]] || {
    metadata+=( PARENT_BUILD_ID="$PARENT_BUILD_ID" )
  }
  if [[ "${PARENT_BUILD_LINK:-NOT_SET}" == "NOT_SET" ]]
  then
    if [[ "${BUILDBUCKET_ID:-NOT_SET}" != "NOT_SET" ]] then
      # Link this top-level build invocation to buildbucket.
      case "$BUILDBUCKET_ID" in
        */led/*)
          metadata+=( PARENT_BUILD_LINK="http://go/lucibuild/$BUILDBUCKET_ID/+/build.proto" ) ;;
        *)
          metadata+=( PARENT_BUILD_LINK="http://go/bbid/$BUILDBUCKET_ID" ) ;;
      esac
    fi
  else
    metadata+=( PARENT_BUILD_LINK="$PARENT_BUILD_LINK" )
  fi
  [[ "${SIBLING_BUILDS_LINK:-NOT_SET}" == "NOT_SET" ]] || {
    metadata+=( SIBLING_BUILDS_LINK="$SIBLING_BUILDS_LINK" )
  }

  readonly CFG="$SCRIPT_DIR/fuchsia-resultstore.cfg"
  readonly results_url="$(grep "^results_url=" "$CFG" | cut -d= -f2)"

  # If there are no sub-builds, this query will just return empty.
  readonly SUB_BUILDS_LINK="$results_url/?q=PARENT_BUILD_ID:$invocation_id"
  metadata+=( SUB_BUILDS_LINK="$SUB_BUILDS_LINK" )

  ninja_metadata_args=()
  for md in "${metadata[@]}"
  do ninja_metadata_args+=( --bes_metadata "$md" )
  done

  readonly ninja_env=(
    # For this ninja invocation only:
    NINJA_BUILD_ID="$invocation_id"

    # Replace the following environment variables with new values.
    # These variables are not seen by build tools like ninja and bazel directly,
    # but are used by their wrapper counterparts, like wrapper.bazel.sh.
    # Sub-builds of this invocation will see this invocation as the parent.
    PARENT_BUILD_ID="$invocation_id"
    PARENT_BUILD_LINK="$results_url/$invocation_id"
    # To sub-builds: "Your siblings are..."
    SIBLING_BUILDS_LINK="$SUB_BUILDS_LINK"
  )

  readonly full_cmd=(
    env "${ninja_env[@]}"
    "$wrapper"
    "$ninja_bin"
    "${ninja_metadata_args[@]}"
    "$@"
  )
  exec "${full_cmd[@]}"
else
  # Bypass to plain ninja invocation, no resultstore involvement.
  exec "$ninja_bin" "$@"
fi

