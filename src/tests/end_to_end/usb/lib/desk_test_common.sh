#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Shared helper library for USB/ADB at-desk end-to-end runner scripts under
# //src/tests/end_to_end/usb/.
#
# Centralizes FUCHSIA_DIR / fx discovery, build-graph checks, and standard
# `fx test --e2e` command construction so individual runner scripts do not
# duplicate boilerplate or hardcode fragile relative-path depth counts to the
# checkout root.

# Directory containing this library (//src/tests/end_to_end/usb/lib).
USB_DESK_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"

# Populated by `usb_desk_build_fx_test_cmd`.
USB_DESK_TEST_CMD=()

# Resolves FUCHSIA_DIR and FX from this library's fixed location in the tree
# (5 levels below the Fuchsia root: lib -> usb -> end_to_end -> tests -> src)
# rather than from the current working directory (`git rev-parse --show-toplevel`
# is wrong in a jiri multi-repo checkout when invoked from a sub-project such
# as //vendor/google). Always invokes `fx` by path since `fx` is only on PATH
# if `scripts/fx-env.sh` was sourced.
usb_desk_init_env() {
  if [[ -z "${FUCHSIA_DIR:-}" ]]; then
    FUCHSIA_DIR="$(cd -- "${USB_DESK_LIB_DIR}/../../../../.." >/dev/null 2>&1 && pwd)"
  fi
  FX="${FUCHSIA_DIR}/scripts/fx"

  if [[ ! -x "${FX}" ]]; then
    echo "ERROR: could not locate the Fuchsia checkout root." >&2
    echo "  Derived FUCHSIA_DIR=${FUCHSIA_DIR}" >&2
    echo "  but ${FX} is not executable." >&2
    echo "  This library expects to live at //src/tests/end_to_end/usb/lib/." >&2
    exit 1
  fi
  export FUCHSIA_DIR FX
}

# Rejects a missing option value or a value that looks like another flag, so
# that e.g. `-t -n` reports an error instead of sending "-n" to fx as a device.
usb_desk_require_value() {
  local flag="$1"
  local count="$2"
  local value="${3-}"
  if [[ "${count}" -lt 2 || "${value}" == -* ]]; then
    echo "ERROR: ${flag} requires a value" >&2
    exit 1
  fi
}

# Ensures a test target is present in the current build graph (`tests.json`).
# If missing, adds it to the build configuration:
# - When `<test_group>` is supplied (a target-toolchain wrapper group that pulls
#   in `$host_toolchain`), runs `fx add-test <test_group>`.
# - When `<test_group>` is omitted, runs `fx add-host-test <test_target>` so
#   `is_host`-gated E2E targets are added to `host_labels` rather than failing
#   in the Fuchsia target toolchain.
#
# Usage:
#   usb_desk_ensure_test_in_build_graph <test_target> [test_group_to_add]
usb_desk_ensure_test_in_build_graph() {
  local test_target="$1"
  local test_group="${2:-}"
  local build_dir
  build_dir="$("${FX}" get-build-dir)"

  if [[ -f "${build_dir}/tests.json" ]] &&
    grep -qE "\"${test_target}([(\"]|$)" "${build_dir}/tests.json"; then
    return 0
  fi

  if [[ -n "${test_group}" ]]; then
    echo "[*] ${test_target} is not in the build graph; adding ${test_group}..."
    "${FX}" add-test "${test_group}"
  else
    echo "[*] ${test_target} is not in the build graph; adding host test..."
    "${FX}" add-host-test "${test_target}"
  fi
}

# Validates that all requested test names resolve in `fx test --dry --e2e`.
# Prints actionable `fx add-test` suggestions and exits non-zero if any are
# missing from the build configuration.
#
# Usage:
#   usb_desk_verify_tests_in_build <target_device_or_empty> <test_name>...
usb_desk_verify_tests_in_build() {
  local device="$1"
  shift
  local -a tests_to_run=("$@")
  if [[ ${#tests_to_run[@]} -eq 0 ]]; then
    echo "ERROR: usb_desk_verify_tests_in_build requires at least one test name." >&2
    return 1
  fi

  local -a fx_global_args=()
  if [[ -n "${device}" ]]; then
    fx_global_args+=(-t "${device}")
  fi

  echo "Checking build configuration..."
  local dry_run_out
  local gate_ok=true
  if ! dry_run_out=$("${FX}" ${fx_global_args[@]+"${fx_global_args[@]}"} \
      test --dry --e2e "${tests_to_run[@]}" 2>&1); then
    gate_ok=false
  fi

  # Strip ANSI styling before matching. fx disables color when stdout is not
  # a TTY, which is the case here, but do not depend on that. Use $'\033'
  # rather than \x1b, which is a GNU sed extension.
  local clean_out
  clean_out=$(printf '%s\n' "${dry_run_out}" |
    sed -e $'s/\033\\[[0-9;]*m//g')

  # `fx test --dry` returns exit status 0 as long as at least one test argument
  # matches, even when other arguments in the list do not match. Check the
  # output explicitly so partial matches still trigger the missing-test gate.
  if printf '%s\n' "${clean_out}" |
    grep -qF "Could not find any tests to run"; then
    gate_ok=false
  fi

  if [[ "${gate_ok}" == false ]]; then
    # Collect "<name>\t<suggestion>" for every 100.00% block. Matching the
    # whole line and taking field 1 avoids a substring of one test name
    # matching a different, longer suggested name.
    local parsed
    parsed=$(printf '%s\n' "${clean_out}" | awk '
      /^[[:space:]]*[^[:space:]]+ \(100\.00% similar\)[[:space:]]*$/ {
        name = $1
        suggestion = ""
        if ((getline line) > 0) {
          sub(/^[[:space:]]+/, "", line)
          sub(/[[:space:]]+$/, "", line)
          suggestion = line
        }
        print name "\t" suggestion
      }')

    local -a missing_tests=()
    local -a suggested_commands=()
    local candidate suggestion
    for candidate in "${tests_to_run[@]}"; do
      # -x for a whole-line match, -- so a name starting with "-" is not
      # parsed as an option.
      if ! printf '%s\n' "${parsed}" | cut -f1 |
        grep -qxF -- "${candidate}"; then
        continue
      fi
      missing_tests+=("${candidate}")
      suggestion=$(printf '%s\n' "${parsed}" |
        awk -F'\t' -v n="${candidate}" '$1 == n { print $2; exit }')
      # The line after a match is not always a command: for a target already
      # in the build it reads "Build includes: fuchsia-pkg://...". Only
      # forward it if it really is something the user can run.
      if [[ "${suggestion}" == fx\ * ]]; then
        suggested_commands+=("${suggestion}")
      fi
    done

    if [[ ${#missing_tests[@]} -gt 0 ]]; then
      echo "ERROR: these tests are not in your build configuration:" >&2
      printf '  - %s\n' "${missing_tests[@]}" >&2
      echo "" >&2
      if [[ ${#suggested_commands[@]} -gt 0 ]]; then
        echo "To add them, run:" >&2
        printf '%s\n' "${suggested_commands[@]}" | sort -u |
          sed -e 's/^/  /' >&2
        echo "" >&2
      fi
      echo "Or add all E2E USB tests at once:" >&2
      echo "  fx add-host-test //src/tests/end_to_end/usb:tests" >&2
    else
      echo "ERROR: 'fx test --dry' failed, but not because of a missing" >&2
      echo "test. Output was:" >&2
      printf '%s\n' "${dry_run_out}" >&2
    fi
    exit 1
  fi
}

# Populates the global array `USB_DESK_TEST_CMD` with the `fx test` invocation.
#
# Usage:
#   usb_desk_build_fx_test_cmd <target_device_or_empty> <no_temp_emu:true|false> \
#       <test_target_or_fx_test_arg>... [-- <extra_test_runner_args>...]
usb_desk_build_fx_test_cmd() {
  local target_device="$1"
  local no_temp_emu="$2"
  shift 2

  USB_DESK_TEST_CMD=("${FX}")
  if [[ -n "${target_device}" ]]; then
    USB_DESK_TEST_CMD+=(-t "${target_device}")
  fi
  USB_DESK_TEST_CMD+=(
    test
    -o
    --e2e
  )
  if [[ "${no_temp_emu}" == "true" ]]; then
    USB_DESK_TEST_CMD+=(--no-allow-temporary-emulator)
  fi
  USB_DESK_TEST_CMD+=("$@")
}

# Initialize FUCHSIA_DIR and FX when sourced.
usb_desk_init_env
