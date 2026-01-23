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

# Use re-client's credentials helper tool to exchange LOAS for OAuth2 tokens.
readonly credshelper="${PREBUILT_RECLIENT_DIR}/credshelper"

# TODO(b/473907403): for infra builds, plumb remote service proxy overrides:
#   BAZEL_resultstore_socket_path -> RS_rs_service
#   BAZEL_rbe_socket_path -> RS_cas_service
# These will take precedence over values in .cfg files.
# All sub-invocations can share the same sockets.

# TODO: scan ninja options for flags that produce extra output files,
# such as traces.  Convert these into rsproxy invocation artifacts
# as --post_build_uploads.

rsproxy_options=()

# FX_BUILD_LOAS_TYPE is set by 'fx build' to either "restricted" or
# "unrestricted", and influences authentication method.
# Infra builds don't set this, but instead pass environment variables
# that will override the cfg values.
[[ "${FX_BUILD_LOAS_TYPE:-NOT_SET}" == "NOT_SET" ]] || {
  case "$FX_BUILD_LOAS_TYPE" in
    unrestricted)
      readonly CFG="$SCRIPT_DIR/fuchsia-resultstore-gcertauth.cfg"
      rsproxy_options+=(
        --cfg "$CFG"
        --credentials_helper "${credshelper}"
      )
      ;;
    *)
      readonly CFG="$SCRIPT_DIR/fuchsia-resultstore.cfg"
      rsproxy_options+=(
        --cfg "$CFG"
      )
      ;;
  esac
}

# Scan ninja arguments for important options.
ninja_args=("$@")
subbuild_dir=
prev_opt=""
for opt  # "$@"
do
  # handle --option arg
  if [[ -n "$prev_opt" ]]
  then
    eval "$prev_opt"=\$opt
    prev_opt=
    shift
    continue
  fi

  case "$opt" in
    -C) prev_opt=subbuild_dir ;;
  esac
  shift
done

wrap_options=(
  --rsproxy "$rsproxy"
)

[[ "${FX_BUILD_LOGDIR:-NOT_SET}" == "NOT_SET" ]] || {
  [[ -n "$subbuild_dir" ]] || {
    echo "Error: Expected a ninja -C subdir, but found none."
    exit 1
  }
  wrap_options+=( --log-dir "$FX_BUILD_LOGDIR/rsproxy_logs/$subbuild_dir"  )
}
# Otherwise, if FX_BUILD_LOGDIR isn't set, this is probably being invoked
# outside of 'fx build', so just fallback to using some temp dir.

# Ensure that the prebuilt python3 is in the PATH (needed in infra environment).
# rsproxy-wrap.sh uses python3 as an alternative means for mkfifo.
readonly py3_bindir="${PREBUILT_PYTHON3%/*}"  # dirname
export PATH="$py3_bindir:$PATH"

full_cmd=(
  "$proxy_wrap"
  "${wrap_options[@]}"
  --rsproxy_options
  "${rsproxy_options[@]}"
  --
  "$rsninja"
  "${ninja_args[@]}"
)

exec "${full_cmd[@]}"
