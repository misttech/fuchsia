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

_SRCDIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" >/dev/null 2>&1 && pwd)"
readonly _SRCDIR

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

readonly _GENERATE_INVOCATION_BAZELRC="${_SRCDIR}/scripts/generate_invocation_bazelrc.py"
readonly _INVOCATION_BAZELRC="${_BAZEL_WORKSPACE}/invocation.bazelrc"

# Exported explicitly to be used by repository rules to reference the
# Ninja output directory and binary.
export BAZEL_FUCHSIA_NINJA_OUTPUT_DIR="${_NINJA_BUILD_DIR}"
export BAZEL_FUCHSIA_NINJA_PREBUILT="${_NINJA_PREBUILT}"

# Ensure our prebuilt Python3 executable is in the PATH to run repository
# rules that invoke Python programs correctly in containers or jails that
# do not expose the system-installed one.
export PATH="${_PREBUILT_PYTHON_DIR}/bin:${PATH}"

# Provide a direct path to the prebuilt Python3 executable as well.
readonly PREBUILT_PYTHON3="${_PREBUILT_PYTHON_DIR}/bin/python3"

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

# This is for ResultStore.
# TODO: depend on GN arg bazel_upload_build_events = {"sponge", "resultstore"}
readonly RESULTSTORE_URL="http://go/fxbtx"
readonly RESULTSTORE_SUB_BUILDS_LINK="$RESULTSTORE_URL/?q=PARENT_BUILD_ID:$RESULTSTORE_invocation_id"

# A list of extra Bazel arguments that must appear after the
# command.
_BAZEL_EXTRA_ARGS=()

# The following step is sensitive to special environment variables:
#   * per-invocation build metadata
#       When uploading results to build event services like ResultStore or
#       Sponge, include extra metadata that links related invocations.
#   * proxy overrides sockets for remote services
#       For infra builds, connections to various remote services are tunneled
#       through local socket relays, launched by
#       [infra/infra]/cmd/buildproxywrap/main.go.
# These environment-sensitive modifications to config definitions
# manifest in the short-lived 'invocation.bazelrc'.
# Delete after use.
trap "rm -f ${_INVOCATION_BAZELRC}" EXIT
"${_GENERATE_INVOCATION_BAZELRC}" \
  --sub_builds_link="$RESULTSTORE_SUB_BUILDS_LINK" \
  > "${_INVOCATION_BAZELRC}"

# Save a copy of the invocation.bazelrc to the build log dir.
# Bazel invocations occur in the same workspace, so invocations
# are distinguished by timestamp.
[[ -z "$FX_BUILD_LOGDIR" ]] || {
  readonly logdir="$FX_BUILD_LOGDIR/bazel_logs"
  mkdir -p "$logdir"
  readonly date="$(date +%Y%m%d-%H%M%S)"
  cp "${_INVOCATION_BAZELRC}" "$logdir/invocation-$date.bazelrc"
}


# Non-remote configuration permits use of a disk-cache, below.
has_remote_config=
for arg in "${_BAZEL_DIRECT_ARGS[@]}"
do
  # Check for infra and non-infra config variations to allow for local testing.
  # "sponge" and "resultstore" come from 'build/bazel/remote_services.gni',
  # and are added in 'build/bazel/bazel_action.gni'.
  # TODO(https://fxbug.dev/445093719): This method doesn't work if the same
  # configs are indirectly enabled.
  case "$arg" in
    --config=remote | --config=remote_cache_only)  # Remote build execution service
      has_remote_config=true
      ;;
  esac
done

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

# We generate our own invocation id, to make it easier to propagate its value
# to sub-build invocations.
readonly RESULTSTORE_invocation_id="$("${PREBUILT_PYTHON3}" -S -c 'import uuid; print(uuid.uuid4())')"
_BAZEL_EXTRA_ARGS+=( --invocation_id="$RESULTSTORE_invocation_id" )

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

  # For ephemeral configuration that amends remote_services.bazelrc:
  --bazelrc="${_INVOCATION_BAZELRC}"
)

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
  )

# Setting $USER so `bazel` won't fail in environments with fake UIDs. Even if
# the USER is not actually used. See https://fxbug.dev/42063551#c9.
# In developer environments, use the real username so that authentication
# and credential helpers will work, e.g. go/bazel-google-sso.
_user="${USER:-unused-bazel-build-user}"

_bazel_env=(
    env USER="${_user}"

    # Inform sub-builds of this bazel invocation where to find related builds.
    # These environment variables are read by generate_invocation_bazelrc.py
    # and rsninja.sh, but do not impact actions from this bazel invocation.
    # LINT.IfChange(related_invocations_env_vars)
    RESULTSTORE_PARENT_BUILD_ID="$RESULTSTORE_invocation_id"
    RESULTSTORE_PARENT_BUILD_LINK="$RESULTSTORE_URL/$RESULTSTORE_invocation_id"
    RESULTSTORE_SIBLING_BUILDS_LINK="$RESULTSTORE_SUB_BUILDS_LINK"
    # LINT.ThenChange(
    #   //build/bazel/scripts/generate_invocation_bazelrc.py:related_invocations_env_vars,
    #   //build/bazel/scripts/rsninja.sh:related_invocations_env_vars
    # )
)

_bazel_command=(
    "${_bazel_env[@]}"
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
