#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Launcher script for running Bazel in the Fuchsia Bazel SDK test workspace.
# Requires a configuration file that defines read-only parameters.
# Can be invoked from any directory, e.g.:
#   ${0##*/} test //:tests
#   ${0##*/} test :archivist_tests --test_output=streamed
#   ${0##*/} query //fuchsia/...

set -e

function die {
  echo >&2 "ERROR: $*"
  exit 1
}

# Read the configuration file. This should define the following variables:
#
# _WORKSPACE_DIR: Bazel workspace directory path.
# _FUCHSIA_SOURCE_DIR: Fuchsia source directory path.
# _BAZEL_BIN: Bazel binary path.
# _OUTPUT_BASE: Bazel output base directory path.
# _OUTPUT_USER_ROOT: Bazel output user root directory path (may be empty).
# _PYTHON_PREBUILT_DIR: Prebuilt Python directory path.
# _PYTHON_VERSION_FILE: Prebuilt Python version file path (may be empty).
# _CLANG_VERSION_FILE: Prebuilt Clang version file path (may be empty).
# _DOWNLOADER_CONFIG_FILE: Downloader configuration file path.
# _BAZEL_VENDOR_DIR: Bazel vendor directory path.
# _BAZEL_REGISTRY_DIR: Bazel registry directory path.
# _EXPLICIT_FUCHSIA_SDK: Path to explicit @fuchsia_sdk repository (may be empty).
# _STARTUP_FLAGS: Bash array of Bazel startup flags.
# _REPO_OVERRIDE_FLAGS: Bash array of Bazel repository override flags.
# _CONFIG_ARGS: Bash array of default Bazel --config arguments.
#
# All paths should be absolute.
#
readonly _CONFIG_FILE="${BASH_SOURCE[0]}.config"
[[ -f "${_CONFIG_FILE}" ]] || die "Missing launcher configuration file: ${_CONFIG_FILE}"

# shellcheck source=/dev/null
source "${_CONFIG_FILE}"

[[ -n "${_WORKSPACE_DIR}" ]] || die "Missing _WORKSPACE_DIR config variable"
[[ -d "${_WORKSPACE_DIR}" ]] || die "Missing test workspace directory: ${_WORKSPACE_DIR}"

[[ -n "${_FUCHSIA_SOURCE_DIR}" ]] || die "Missing _FUCHSIA_SOURCE_DIR config variable"
[[ -d "${_FUCHSIA_SOURCE_DIR}" ]] || die "Missing Fuchsia source directory: ${_FUCHSIA_SOURCE_DIR}"

[[ -n "${_BAZEL_BIN}" ]] || die "Missing _BAZEL_BIN config variable"
[[ -f "${_BAZEL_BIN}" ]] || die "Missing Bazel binary: ${_BAZEL_BIN}"

[[ -n "${_OUTPUT_BASE}" ]] || die "Missing _OUTPUT_BASE config variable"

[[ -v _OUTPUT_USER_ROOT ]] || die "Missing _OUTPUT_USER_ROOT config variable"

[[ -n "${_PYTHON_PREBUILT_DIR}" ]] || die "Missing _PYTHON_PREBUILT_DIR config variable"
[[ -d "${_PYTHON_PREBUILT_DIR}" ]] || die "Missing prebuilt Python directory: ${_PYTHON_PREBUILT_DIR}"

[[ -v _PYTHON_VERSION_FILE ]] || die "Missing _PYTHON_VERSION_FILE config variable"
[[ -v _CLANG_VERSION_FILE ]] || die "Missing _CLANG_VERSION_FILE config variable"

[[ -n "${_DOWNLOADER_CONFIG_FILE}" ]] || die "Missing _DOWNLOADER_CONFIG_FILE config variable"
[[ -f "${_DOWNLOADER_CONFIG_FILE}" ]] || die "Missing downloader config file: ${_DOWNLOADER_CONFIG_FILE}"

[[ -n "${_BAZEL_VENDOR_DIR}" ]] || die "Missing _BAZEL_VENDOR_DIR config variable"
[[ -d "${_BAZEL_VENDOR_DIR}" ]] || die "Missing Bazel vendor directory: ${_BAZEL_VENDOR_DIR}"

[[ -n "${_BAZEL_REGISTRY_DIR}" ]] || die "Missing _BAZEL_REGISTRY_DIR config variable"
[[ -d "${_BAZEL_REGISTRY_DIR}" ]] || die "Missing Bazel registry directory: ${_BAZEL_REGISTRY_DIR}"

[[ -v _EXPLICIT_FUCHSIA_SDK ]] || die "Missing _EXPLICIT_FUCHSIA_SDK config variable"

declare -p _STARTUP_FLAGS >/dev/null 2>&1 || die "Missing _STARTUP_FLAGS config variable"
declare -p _REPO_OVERRIDE_FLAGS >/dev/null 2>&1 || die "Missing _REPO_OVERRIDE_FLAGS config variable"
declare -p _CONFIG_ARGS >/dev/null 2>&1 || die "Missing _CONFIG_ARGS config variable"

if [[ "$#" -eq 0 ]]; then
  die "This launcher script requires arguments.
Use '${0##*/} test //:tests' to launch the SDK test suite."
fi

export BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1
export PATH="${_PYTHON_PREBUILT_DIR}/bin:${PATH}"
export USER="${USER:-unused-bazel-build-user}"

if [[ -n "${_EXPLICIT_FUCHSIA_SDK}" && ! -d "${_EXPLICIT_FUCHSIA_SDK}" ]]; then
  _MSG="Missing @fuchsia_sdk repository directory: ${_EXPLICIT_FUCHSIA_SDK}"
  if [[ -z "${LOCAL_FUCHSIA_SDK_DIRECTORY:-}" ]]; then
    _MSG="${_MSG}
Please run 'fx build //build/bazel/bazel_sdk:in_tree_fuchsia_sdk' (or 'fx bazel query --config=quiet @fuchsia_sdk//:BUILD.bazel') to populate it."
  fi
  die "${_MSG}"
fi

mkdir -p "${_OUTPUT_BASE}"
if [[ -n "${_OUTPUT_USER_ROOT}" ]]; then
  mkdir -p "${_OUTPUT_USER_ROOT}"
fi

# Auto-detect authentication method when invoked directly outside `fx build`.
if [[ -z "${FX_BUILD_RBE_AUTH_TYPE:-}" ]]; then
  if [[ -x "${_FUCHSIA_SOURCE_DIR}/build/auth/select_auth_method.py" ]]; then
    FX_BUILD_RBE_AUTH_TYPE="$("${_PYTHON_PREBUILT_DIR}/bin/python3" -S "${_FUCHSIA_SOURCE_DIR}/build/auth/select_auth_method.py" 2>/dev/null | tail -n 1 || true)"
    export FX_BUILD_RBE_AUTH_TYPE
    export FX_BUILD_RS_AUTH_TYPE="${FX_BUILD_RS_AUTH_TYPE:-"${FX_BUILD_RBE_AUTH_TYPE}"}"
  fi
fi

if [[ -n "${_PYTHON_VERSION_FILE}" ]]; then
  [[ -e "${_PYTHON_VERSION_FILE}" ]] || die "Missing prebuilt Python version file: ${_PYTHON_VERSION_FILE}"
  export LOCAL_PREBUILT_PYTHON_VERSION_FILE="${_PYTHON_VERSION_FILE}"
fi

if [[ -n "${_CLANG_VERSION_FILE}" ]]; then
  [[ -e "${_CLANG_VERSION_FILE}" ]] || die "Missing prebuilt Clang version file: ${_CLANG_VERSION_FILE}"
  export LOCAL_FUCHSIA_CLANG_VERSION_FILE="${_CLANG_VERSION_FILE}"
fi

_BAZEL_COMMAND=
_BAZEL_PRE_COMMAND_ARGS=()
_BAZEL_POST_COMMAND_ARGS=()
_BAZEL_REST_ARGS=()

while [[ "$#" -gt 0 ]]; do
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

_BAZEL_STARTUP_ARGS=(
  "${_STARTUP_FLAGS[@]}"
)
if [[ -n "${_OUTPUT_USER_ROOT}" ]]; then
  _BAZEL_STARTUP_ARGS+=("--output_user_root=${_OUTPUT_USER_ROOT}")
fi
_BAZEL_STARTUP_ARGS+=("--output_base=${_OUTPUT_BASE}")

# Generate ephemeral invocation configuration (metadata, proxies, auth).
readonly _INVOCATION_BAZELRC="$(mktemp "${_OUTPUT_BASE}/invocation.bazelrc.XXXXXX")"
trap 'rm -f "${_INVOCATION_BAZELRC}"' EXIT

"${_PYTHON_PREBUILT_DIR}/bin/python3" -S \
  "${_FUCHSIA_SOURCE_DIR}/build/bazel/scripts/generate_invocation_bazelrc.py" \
  > "${_INVOCATION_BAZELRC}"

_BAZEL_STARTUP_ARGS+=("--bazelrc=${_INVOCATION_BAZELRC}")

if [[ "${_BAZEL_COMMAND}" == "vendor" ]]; then
  _DOWNLOADER_ARG="--downloader_config=/dev/null"
  _REGISTRY_ARG="--registry=https://bcr.bazel.build/"
else
  _DOWNLOADER_ARG="--downloader_config=${_DOWNLOADER_CONFIG_FILE}"
  _REGISTRY_ARG="--registry=file://${_BAZEL_REGISTRY_DIR}"
fi

_BAZEL_COMMON_ARGS=(
  "${_DOWNLOADER_ARG}"
  --enable_bzlmod=true
  --incompatible_use_plus_in_repo_names
  "--vendor_dir=${_BAZEL_VENDOR_DIR}"
  "${_REGISTRY_ARG}"
  "${_REPO_OVERRIDE_FLAGS[@]}"
)

_BAZEL_CONFIG_ARGS=(
  "${_CONFIG_ARGS[@]}"
)

_ALL_CONFIG_ARGS=("${_BAZEL_CONFIG_ARGS[@]}" "${_BAZEL_POST_COMMAND_ARGS[@]}")

function _has_config() {
  local needle="--config=$1"
  local arg
  for arg in "${_ALL_CONFIG_ARGS[@]}"; do
    if [[ "${arg}" == "${needle}" ]]; then
      return 0
    fi
  done
  return 1
}

_JOBS=""
if _has_config "remote"; then
  _CPUS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo "")"
  if [[ -n "${_CPUS}" ]]; then
    _JOBS=$(( 10 * _CPUS ))
  fi
else
  if [[ -n "${FUCHSIA_BAZEL_DISK_CACHE:-}" ]]; then
    _BAZEL_CONFIG_ARGS+=("--disk_cache=${FUCHSIA_BAZEL_DISK_CACHE}")
    if [[ -n "${FUCHSIA_BAZEL_DISK_CACHE_SIZE:-}" ]]; then
      _BAZEL_CONFIG_ARGS+=("--experimental_disk_cache_gc_max_size=${FUCHSIA_BAZEL_DISK_CACHE_SIZE}")
    fi
  fi
fi

if [[ -n "${FUCHSIA_BAZEL_JOB_COUNT:-}" ]]; then
  _JOBS="${FUCHSIA_BAZEL_JOB_COUNT}"
fi

# In Corp environments with valid gcert credentials, use the credential helper
# to automatically exchange LOAS for OAuth (Google Cloud Platform) tokens.
# Note: --config=gcertauth is generated into _INVOCATION_BAZELRC when
# FX_BUILD_RBE_AUTH_TYPE or FX_BUILD_RS_AUTH_TYPE is "loas".
if [[ "${FX_BUILD_RBE_AUTH_TYPE:-}" == "loas" || "${FX_BUILD_RS_AUTH_TYPE:-}" == "loas" ]]; then
  unset GOOGLE_APPLICATION_CREDENTIALS
fi

if [[ -n "${_JOBS}" ]]; then
  _BAZEL_CONFIG_ARGS+=("--jobs=${_JOBS}")
fi

_FINAL_CMD=(
  "${_BAZEL_BIN}"
  "${_BAZEL_STARTUP_ARGS[@]}"
  "${_BAZEL_PRE_COMMAND_ARGS[@]}"
)

case "${_BAZEL_COMMAND}" in
  "" | info | shutdown | clean | version | help)
    if [[ -n "${_BAZEL_COMMAND}" ]]; then
      _FINAL_CMD+=("${_BAZEL_COMMAND}")
    fi
    _FINAL_CMD+=(
      "${_BAZEL_POST_COMMAND_ARGS[@]}"
      "${_BAZEL_REST_ARGS[@]}"
    )
    ;;
  query | vendor | mod | fetch | sync)
    _FINAL_CMD+=(
      "${_BAZEL_COMMAND}"
      "${_BAZEL_COMMON_ARGS[@]}"
      "${_BAZEL_POST_COMMAND_ARGS[@]}"
      "${_BAZEL_REST_ARGS[@]}"
    )
    ;;
  *)
    _FINAL_CMD+=(
      "${_BAZEL_COMMAND}"
      "${_BAZEL_COMMON_ARGS[@]}"
      "${_BAZEL_CONFIG_ARGS[@]}"
      "${_BAZEL_POST_COMMAND_ARGS[@]}"
      "${_BAZEL_REST_ARGS[@]}"
    )
    ;;
esac

if [[ "${FUCHSIA_BAZEL_PRINT_COMMANDS:-}" == "1" ]]; then
  echo >&2 "RUN_COMMAND: (cd ${_WORKSPACE_DIR@Q} && ${_FINAL_CMD[*]@Q})"
fi

# Do not 'exec' here so that the shell process remains alive until the command
# exits, ensuring that the EXIT trap triggers to clean up _INVOCATION_BAZELRC.
cd "${_WORKSPACE_DIR}" && "${_FINAL_CMD[@]}"
