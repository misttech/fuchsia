# shellcheck shell=bash
# shellcheck disable=SC2034  # unused variables used outside this file.
# Copyright 2019 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This file can be sourced multiple times, since it defines readonly
# variables, the second source will create Bash warnings, so use a
# guard variable to avoid this.
#
# NOTE: This file can be sourced with `set -u` in effect, so
# simply expanding _lib_platform_sh_sourced when not defined will
# result in an error.

if [[ -z "${_lib_platform_sh_sourced:-}" ]]; then

readonly _lib_platform_sh_sourced=true

# Use the OSTYPE and MACHTYPE Bash builtin variables to determine host
# machine type.
case "$OSTYPE" in
  linux*)
    readonly HOST_OS="linux"
    ;;
  darwin*)
    readonly HOST_OS="mac"
    ;;
  *)
    echo >&2 "Unknown operating system: $OSTYPE."
    exit 1
    ;;
esac

case "$MACHTYPE" in
  x86_64*)
    readonly HOST_CPU="x64"
    ;;
  aarch64*|arm64*)
    readonly HOST_CPU="arm64"
    ;;
  *)
    echo >&2 "Unknown architecture: $MACHTYPE."
    exit 1
    ;;
esac

readonly HOST_PLATFORM="${HOST_OS}-${HOST_CPU}"

readonly PREBUILT_3P_DIR="${FUCHSIA_DIR}/prebuilt/third_party"
readonly PREBUILT_TOOLS_DIR="${FUCHSIA_DIR}/prebuilt/tools"

readonly PREBUILT_AEMU_DIR="${PREBUILT_3P_DIR}/android/aemu/release-gfxstream/${HOST_PLATFORM}"
readonly PREBUILT_BAZEL_DIR="${PREBUILT_3P_DIR}/bazel/${HOST_PLATFORM}"
readonly PREBUILT_BAZEL="${PREBUILT_BAZEL_DIR}/bazel"
readonly PREBUILT_BINUTILS_DIR="${PREBUILT_3P_DIR}/binutils-gdb/${HOST_PLATFORM}"
readonly PREBUILT_BLACK_DIR="${PREBUILT_3P_DIR}/black/${HOST_PLATFORM}"
readonly PREBUILT_BUILDIFIER_DIR="${PREBUILT_3P_DIR}/buildifier/${HOST_PLATFORM}"
readonly PREBUILT_BUILDIFIER="${PREBUILT_BUILDIFIER_DIR}/buildifier"
readonly PREBUILT_BUILDOZER_DIR="${PREBUILT_3P_DIR}/buildozer/${HOST_PLATFORM}"
readonly PREBUILT_BUILDOZER="${PREBUILT_BUILDOZER_DIR}/buildozer"
readonly PREBUILT_CGPT_DIR="${PREBUILT_TOOLS_DIR}/cgpt/${HOST_PLATFORM}"
readonly PREBUILT_CLANG_DIR="${PREBUILT_3P_DIR}/clang/${HOST_PLATFORM}"
readonly PREBUILT_CMAKE_DIR="${PREBUILT_3P_DIR}/cmake/${HOST_PLATFORM}"
readonly PREBUILT_DART_DIR="${PREBUILT_3P_DIR}/dart/${HOST_PLATFORM}"
readonly PREBUILT_EDK2_DIR="${PREBUILT_3P_DIR}/edk2"
readonly PREBUILT_FUTILITY_DIR="${PREBUILT_TOOLS_DIR}/futility/${HOST_PLATFORM}"
readonly PREBUILT_GCC_DIR="${PREBUILT_3P_DIR}/gcc/${HOST_PLATFORM}"
readonly PREBUILT_GN_DIR="${PREBUILT_3P_DIR}/gn/${HOST_PLATFORM}"
readonly PREBUILT_GN="${PREBUILT_GN_DIR}/gn"
readonly PREBUILT_GO_DIR="${PREBUILT_3P_DIR}/go/${HOST_PLATFORM}"
readonly PREBUILT_GRPCWEBPROXY_DIR="${PREBUILT_3P_DIR}/grpcwebproxy/${HOST_PLATFORM}"
readonly PREBUILT_JQ_DIR="${PREBUILT_3P_DIR}/jq/${HOST_PLATFORM}/bin"
readonly PREBUILT_JQ="${PREBUILT_JQ_DIR}/jq"
readonly PREBUILT_NINJA_DIR="${PREBUILT_3P_DIR}/ninja/${HOST_PLATFORM}"
readonly PREBUILT_NINJA="${PREBUILT_NINJA_DIR}/ninja"
readonly PREBUILT_NINJATRACE_DIR="${PREBUILT_TOOLS_DIR}/ninjatrace/${HOST_PLATFORM}"
readonly PREBUILT_NINJATRACE="${PREBUILT_NINJATRACE_DIR}/ninjatrace"
readonly PREBUILT_PERFETTO_TRACE_PROCESSOR_SHELL_DIR="${PREBUILT_3P_DIR}/perfetto/trace_processor_shell/${HOST_PLATFORM}"
readonly PREBUILT_PERFETTO_TRACE_PROCESSOR_SHELL="${PREBUILT_PERFETTO_TRACE_PROCESSOR_SHELL_DIR}/trace_processor_shell"
readonly PREBUILT_PYTHON3_DIR="${PREBUILT_3P_DIR}/python3/${HOST_PLATFORM}"
readonly PREBUILT_PYTHON3="${PREBUILT_PYTHON3_DIR}/bin/python3"
readonly PREBUILT_QEMU_DIR="${PREBUILT_3P_DIR}/qemu/${HOST_PLATFORM}"
readonly PREBUILT_RECLIENT_DIR="${FUCHSIA_DIR}/prebuilt/third_party/reclient/${HOST_PLATFORM}"
readonly PREBUILT_RUST_BINDGEN_DIR="${PREBUILT_3P_DIR}/rust_bindgen/${HOST_PLATFORM}"
readonly PREBUILT_RUST_CARGO_OUTDATED_DIR="${PREBUILT_3P_DIR}/rust_cargo_outdated/${HOST_PLATFORM}"
readonly PREBUILT_RUST_DIR="${PREBUILT_3P_DIR}/rust/${HOST_PLATFORM}"
readonly PREBUILT_SHAC_DIR="${FUCHSIA_DIR}/prebuilt/tools/shac"
readonly PREBUILT_SHAC="${PREBUILT_SHAC_DIR}/shac"
readonly PREBUILT_VDL_DIR="${FUCHSIA_DIR}/prebuilt/vdl"

# Used by //scripts/hermetic-env for portable shebang lines.
PREBUILT_ALL_PATHS=
PREBUILT_ALL_PATHS+="${PREBUILT_AEMU_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_CLANG_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_CMAKE_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_DART_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_GO_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_GRPCWEBPROXY_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_PYTHON3_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_QEMU_DIR}/bin:"
PREBUILT_ALL_PATHS+="${PREBUILT_RUST_DIR}/bin"
readonly PREBUILT_ALL_PATHS

# Used by shell integrations to make vendored tools conveniently available.
PREBUILT_TOOL_PATHS=
PREBUILT_TOOL_PATHS+="${PREBUILT_BAZEL_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_BINUTILS_DIR}/bin:"
PREBUILT_TOOL_PATHS+="${PREBUILT_BLACK_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_BUILDIFIER_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_BUILDOZER_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_CGPT_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_EDK2_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_FUTILITY_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_GCC_DIR}/bin:"
PREBUILT_TOOL_PATHS+="${PREBUILT_GN_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_JQ_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_NINJA_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_NINJATRACE_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_PERFETTO_TRACE_PROCESSOR_SHELL_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_RECLIENT_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_RUST_BINDGEN_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_RUST_CARGO_OUTDATED_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_SHAC_DIR}:"
PREBUILT_TOOL_PATHS+="${PREBUILT_VDL_DIR}"

fi  # !_lib_platform_sh_sourced
