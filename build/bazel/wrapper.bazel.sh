#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A wrapper script used to invoke Bazel in the Fuchsia build's workspace.
# This requires a file named "bazel.sh.config" to be located in the same
# directory containing specific variable definitions (see below).
#
# Do not use directly, the //build/regenerator script will copy this
# file to a specific location in the build directory and generate the
# appropriate configuration file.
_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
readonly _SCRIPT_DIR

function die {
  echo >&2 "ERROR: $*"
  exit 1
}

# Read the configuration file. This should define the following variables:
#
# _BAZEL_BIN: Path to Bazel launcher script.
# _BAZEL_LOG_DIR: Path to directory where some workspace logs will be written.
# _BAZEL_OUTPUT_BASE: Bazel output base directory path.
# _BAZEL_OUTPUT_USER_ROOT: Bazel output user root directory path.
# _BAZEL_WORKSPACE: Bazel workspace directory path.
# _NINJA_BUILD_DIR: Ninja build directory path.
# _PREBUILT_NINJA: Prebuilt Ninja binary path.
# _PREBUILT_PYTHON_DIR: Prebuilt python binary path.
#
# All paths should be absolute.
#
# LINT.IfChange(bazel.sh.config)
readonly _SCRIPT_ARGS_FILE="${_SCRIPT_DIR}/bazel.sh.config"
# LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:bazel.sh.config)
[[ -f "${_SCRIPT_ARGS_FILE}" ]] || die "Missing Bazel script config file: ${_SCRIPT_ARGS_FILE}"

# shellcheck source=/dev/null
source "${_SCRIPT_ARGS_FILE}"

[[ -n "${_BAZEL_WORKSPACE}" ]] || die "Missing _BAZEL_WORKSPACE config variable"
[[ -d "${_BAZEL_WORKSPACE}" ]] || die "_BAZEL_WORKSPACE should be a directory: ${_BAZEL_WORKSPACE}"

# These directories are created on demand.
[[ -n "${_BAZEL_OUTPUT_BASE}" ]] || die "Missing _BAZEL_OUTPUT_BASE config variable"
[[ -n "${_BAZEL_OUTPUT_USER_ROOT}" ]] || die "Missing _BAZEL_OUTPUT_USER_ROOT config variable"
[[ -n "${_BAZEL_LOG_DIR}" ]] || die "Missing _BAZEL_LOG_DIR config variable"

[[ -n "${_BAZEL_BIN}" ]] || die "Missing _BAZEL_BIN config variable"
[[ -f "${_BAZEL_BIN}" ]] || die "_BAZEL_BIN should be a file: ${_BAZEL_BIN}"

[[ -n "${_NINJA_BUILD_DIR}" ]] || die "Missing _NINJA_BUILD_DIR config variable"
[[ -d "${_NINJA_BUILD_DIR}" ]] || die "_NINJA_BUILD_DIR should be a directory: ${_NINJA_BUILD_DIR}"

[[ -n "${_PREBUILT_NINJA}" ]] || die "Missing _PREBUILT_NINJA config variable"
[[ -f "${_PREBUILT_NINJA}" ]] || die "_PREBUILT_NINJA should be a file: ${_PREBUILT_NINJA}"

[[ -n "${_PREBUILT_PYTHON_DIR}" ]] || die "Missing _PREBUILT_PYTHON_DIR config variable"
[[ -d "${_PREBUILT_PYTHON_DIR}" ]] || die "_PREBUILT_PYTHON_DIR should be a directory: ${_PREBUILT_PYTHON_DIR}"

readonly _REMOTE_SERVICES_BAZELRC="${_NINJA_BUILD_DIR}/regenerator_outputs/remote_services.bazelrc"

# Exported explicitly to be used by repository rules to reference the
# Ninja output directory and binary.
export BAZEL_FUCHSIA_NINJA_OUTPUT_DIR="${_NINJA_BUILD_DIR}"
export BAZEL_FUCHSIA_NINJA_PREBUILT="${_NINJA_PREBUILT}"

# Ensure our prebuilt Python3 executable is in the PATH to run repository
# rules that invoke Python programs correctly in containers or jails that
# do not expose the system-installed one.
export PATH="${_PREBUILT_PYTHON_DIR}/bin:${PATH}"

# An undocumented, but widely used, environment variable that tells Bazel to
# not auto-detect the host C++ installation. This makes workspace setup faster
# and ensures this can be used on containers where GCC or Clang are not
# installed (Bazel would complain otherwise with an error).
export BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1

# Implement log rotation (up to 3 old files)
# $1: log file name (e.g. "path/to/workspace-events.log")
logrotate3 () {
  local i
  local prev_log="$1.3"
  local cur_log
  for i in "2" "1"; do
    rm -f "${prev_log}"
    cur_log="$1.$i"
    [[ -f "${cur_log}" ]] && mv "${cur_log}" "${prev_log}"
    prev_log="${cur_log}"
  done
  cur_log="$1"
  [[ -f "${cur_log}" ]] && mv "${cur_log}" "${prev_log}"
}

# Rotate the workspace events log. Note that this file is created
# through an option set in the .bazelrc file, not the command-line below.
mkdir -p "${_BAZEL_LOG_DIR}"
logrotate3 "${_BAZEL_LOG_DIR}/workspace-events.log"

# Determines the command used in this invocation, and separate arguments
# that follow a -- from the rest, so we can inject extra arguments
# *before* it, as this is crucial to properly wrap invocations such
# as `bazel run <config_args> -- <target> <cmd_args>`.
#
# This sets the following global variable:
#
# _BAZEL_COMMAND: The bazel command (e.g. "version", "info" or "build")
# _BAZEL_PRE_COMMAND_ARGS: All options that appear before the command
# _BAZEL_POST_COMMAND_ARGS: All options that appear after the command,
#     up to -- if provided.
# _BAZEL_REST_ARGS: Empty list, or "--" followed by other arguments that
#     follow it.
#
function parse_bazel_command () {
  _BAZEL_COMMAND=
  _BAZEL_PRE_COMMAND_ARGS=()
  _BAZEL_POST_COMMAND_ARGS=()
  _BAZEL_REST_ARGS=()

  while [[ "${#@}" -gt 0 ]]; do
    case "$1" in
      --)
       _BAZEL_REST_ARGS=("$@")
       break
       ;;
      -*)
       if [[ -z "${_BAZEL_COMMAND}" ]]; then
         _BAZEL_PRE_COMMAND_ARGS+=("$1")
       else
         _BAZEL_POST_COMMAND_ARGS+=("$1")
       fi
       ;;
      *)
       if [[ -z "${_BAZEL_COMMAND}" ]]; then
         _BAZEL_COMMAND="$1"
       else
         _BAZEL_POST_COMMAND_ARGS+=("$1")
       fi
       ;;
    esac
    shift
  done
}

parse_bazel_command "$@"

# The original invocation without -- and the args that follow it.
_BAZEL_DIRECT_ARGS=(
    "${_BAZEL_PRE_COMMAND_ARGS[@]}"
    "${_BAZEL_COMMAND}"
    "${_BAZEL_POST_COMMAND_ARGS[@]}"
)

# Make bazel_command_does_configuration non-empty when the current
# Bazel command requires analysis / build configurations.
bazel_command_does_configuration=
case "${_BAZEL_COMMAND}" in
  cquery | aquery | build | run | test)
      bazel_command_does_configuration=true
      ;;
esac

# A list of extra Bazel arguments that must appear after the
# command.
_BAZEL_EXTRA_ARGS=()

# For infra builds, connections to various remote services are tunneled
# through local socket relays, launched by [infra/infra]/cmd/buildproxywrap/main.go.
# Detect manually provided config options that involve the proxies.
# TODO(https://fxbug.dev/445093719): This method doesn't work if the same configs
# are indirectly enabled.
has_remote_config=
siblings_link_template=
proxy_overrides=()
for arg in "${_BAZEL_DIRECT_ARGS[@]}"
do
  # Check for infra and non-infra config variations to allow for local testing.
  case "$arg" in
    --config=sponge | --config=sponge_infra) # Sponge build event service
      [[ "${BAZEL_sponge_socket_path-NOT_SET}" == "NOT_SET" ]] ||
        proxy_overrides+=( "--bes_proxy=unix://$BAZEL_sponge_socket_path" )
        siblings_link_template="http://sponge/invocations/"
      ;;
    --config=resultstore | --config=resultstore_infra) # Resultstore build event service
      [[ "${BAZEL_resultstore_socket_path-NOT_SET}" == "NOT_SET" ]] ||
        proxy_overrides+=( "--bes_proxy=unix://$BAZEL_resultstore_socket_path" )
        # Note: go/fxbtx uses project=rbe-fuchsia-prod
        siblings_link_template="http://go/fxbtx/"
      ;;
    --config=remote | --config=remote_cache_only)  # Remote build execution service
      has_remote_config=true
      [[ "${BAZEL_rbe_socket_path-NOT_SET}" == "NOT_SET" ]] ||
        proxy_overrides+=( "--remote_proxy=unix://$BAZEL_rbe_socket_path" )
      ;;
  esac
done

# Propagate some build metadata from the environment.
# Some of these values are set by infra.
build_metadata=()
[[ "${BUILDBUCKET_ID-NOT_SET}" == "NOT_SET" ]] || {
  build_metadata+=(
    "BUILDBUCKET_ID=$BUILDBUCKET_ID"
    "SIBLING_BUILDS_LINK=${siblings_link_template}?q=BUILDBUCKET_ID:$BUILDBUCKET_ID"
  )
  case "$BUILDBUCKET_ID" in
    */led/*)
      build_metadata+=(
        "PARENT_BUILD_LINK=http://go/lucibuild/$BUILDBUCKET_ID/+/build.proto"
      )
      ;;
    *)
      build_metadata+=("PARENT_BUILD_LINK=http://go/bbid/$BUILDBUCKET_ID")
      ;;
  esac
}

[[ "${BUILDBUCKET_BUILDER-NOT_SET}" == "NOT_SET" ]] ||
  build_metadata+=( "BUILDBUCKET_BUILDER=$BUILDBUCKET_BUILDER" )

# Developers' builds will have one uuid per `fx build` invocation
# that can be used to correlate multiple bazel sub-builds.
[[ "${FX_BUILD_UUID-NOT_SET}" == "NOT_SET" ]] ||
  build_metadata+=(
    "FX_BUILD_UUID=$FX_BUILD_UUID"
    "SIBLING_BUILDS_LINK=${siblings_link_template}?q=FX_BUILD_UUID:$FX_BUILD_UUID"
  )
  # search for siblings

# In Corp environments with valid gcert credentials, use the credential helper
# to automatically exchange LOAS for OAuth (Google Cloud Platform) tokens.
# This requires less interaction from the user than 'gcloud auth ...'.
use_gcert_auth=()
[[ "$FX_BUILD_LOAS_TYPE" == "unrestricted" ]] && {
  use_gcert_auth=(--config=gcertauth)
  # Don't set this, otherwise bazel will look for it
  # (and fail if it doesn't exist).
  unset GOOGLE_APPLICATION_CREDENTIALS
}

_BAZEL_PRE_COMMAND_ARGS+=(
  # Do not parse $HOME/.bazelrc
  --nohome_rc

  # --nosystem_rc prevent parsing /etc/bazel.bazelrc, but prints a WARNING
  # to stderr on each invocation which is annoyingly noisy. Uncomment the
  # line below once this is fixed. See https://fxbug.dev/445090005.
  # --nosystem_rc

  # Use the output base and user root specific to the current
  # Fuchsia build directory, instead of default locations that
  # are under $HOME/.bazel/
  --output_base="${_BAZEL_OUTPUT_BASE}"
  --output_user_root="${_BAZEL_OUTPUT_USER_ROOT}"

  # Parse the .bazelrc defining configs related to remote builds.
  # TODO(digit): Import this from the workspace's .bazelrc file.
  --bazelrc="${_REMOTE_SERVICES_BAZELRC}"
)

# Add build metadata.
for metadata in "${build_metadata[@]}"; do
  _BAZEL_EXTRA_ARGS+=("--build_metadata=${metadata}")
done

# Use a shared disk cache if FUCHSIA_BAZEL_DISK_CACHE is set and
# --config=remote is not used. Bazel documentation states that --disk_cache
# is compatible with remote caching, but RBE documentation says otherwise
# so err on the side of caution. This path must be absolute.
#
# This is useful when several checkouts are used on the same machine,
# or when performing repeated clean builds are performed frequently. Note that
# Bazel itself never cleans up the disk cache, as this is left to the user.
[[ -n "${FUCHSIA_BAZEL_DISK_CACHE}" && -z "${has_remote_config}" && -n "${_BAZEL_COMMAND}" ]] &&
  _BAZEL_EXTRA_ARGS+=(--disk_cache="${FUCHSIA_BAZEL_DISK_CACHE}")

# Add remote related arguments for configuration-sensitive commands.
# It is likely that this is only required for commands that build artifacts
# (i.e. build, run, test but not cquery and aquery) but err on the side of
# caution.
[[ -n "${bazel_command_does_configuration}" ]] &&
  _BAZEL_EXTRA_ARGS+=(
    "${use_gcert_auth[@]}"
    "${proxy_overrides[@]}"
  )

# Setting $USER so `bazel` won't fail in environments with fake UIDs. Even if
# the USER is not actually used. See https://fxbug.dev/42063551#c9.
# In developer environments, use the real username so that authentication
# and credential helpers will work, e.g. go/bazel-google-sso.
_user="${USER:-unused-bazel-build-user}"

_bazel_command=(
    env USER="${_user}"
    "${_BAZEL_BIN}"
    "${_BAZEL_PRE_COMMAND_ARGS[@]}"
)
# When "${BAZEL_COMMAND} is empty, do not add it nor _BAZEL_EXTRA_ARGS to _bazel_command,
[[ -n "${_BAZEL_COMMAND}" ]] && _bazel_command+=("${_BAZEL_COMMAND}" "${_BAZEL_EXTRA_ARGS[@]}")

_bazel_command+=(
    "${_BAZEL_POST_COMMAND_ARGS[@]}"
    "${_BAZEL_REST_ARGS[@]}"
)

# Save the final invocation to a log file.
logrotate3 "${_BAZEL_LOG_DIR}/bazel_invocation"
echo "${_bazel_command[*]}" >> "${_BAZEL_LOG_DIR}/bazel_invocation"

cd "${_BAZEL_WORKSPACE}" && "${_bazel_command[@]}"
