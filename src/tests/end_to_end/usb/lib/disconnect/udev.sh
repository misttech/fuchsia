#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Ensure the script runs under bash even if invoked via `sh udev.sh`
if [ -z "${BASH_VERSION:-}" ]; then
  exec bash "$0" "$@"
fi

# Sets up udev rules on a Linux host workstation to allow non-root users to
# toggle USB authorization (/sys/bus/usb/devices/<bus_id>/authorized) for
# Fuchsia/Android devices during local E2E testing (e.g., usb_virtual_disconnect_test).
#
# The covered USB Product IDs (PIDs) under Google's Vendor ID (VID) 18d1 are
# read from the list shared with linux_virtual_usb_hub.py:
#   //src/tests/end_to_end/usb/lib/fuchsia_usb_pids.json
# so a virtual disconnect works regardless of which USB function combination
# the device happens to be booted into.

set -uo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "[ERROR]: This script is only supported on Linux host workstations." >&2
  exit 1
fi

RULE_FILE="/etc/udev/rules.d/99-fuchsia-usb-authorize.rules"

# The USB Vendor ID and Product IDs that the rules below grant access to.
TARGET_VID="18d1"
# udev.sh lives at //src/tests/end_to_end/usb/lib/disconnect, six levels below
# the Fuchsia root. Resolve it so errors show an absolute path even when the
# script is invoked through a symlink or a relative path.
UDEV_FUCHSIA_DIR="$(realpath -m "$(dirname "$(realpath "${BASH_SOURCE[0]}")")/../../../../../..")"
PID_FILE="${UDEV_FUCHSIA_DIR}/src/tests/end_to_end/usb/lib/fuchsia_usb_pids.json"
PYTHON="${UDEV_FUCHSIA_DIR}/scripts/fuchsia-vendored-python"
if [[ ! -f "${PID_FILE}" || ! -x "${PYTHON}" ]]; then
  echo "[ERROR]: USB PID list or fuchsia-vendored-python not found under ${UDEV_FUCHSIA_DIR}." \
    "Run udev.sh from a Fuchsia checkout." >&2
  exit 1
fi
mapfile -t TARGET_PIDS < <("${PYTHON}" -c \
  'import json, sys; print("\n".join(e["pid"].lower() for e in json.load(open(sys.argv[1]))))' \
  "${PID_FILE}")
if [[ "${#TARGET_PIDS[@]}" -eq 0 ]]; then
  echo "[ERROR]: Failed to read USB PIDs from ${PID_FILE}." >&2
  exit 1
fi

# Returns 0 if the given USB Product ID is one that the udev rules cover.
#
# TARGET_VID is shared by every Google-branded peripheral (security keys,
# phones, Chromecasts, ...), so matching on the vendor ID alone would flag
# unrelated hardware as misconfigured and force a needless sudo prompt.
is_target_pid() {
  # sysfs reports lowercase hex, but normalize anyway to match the
  # case-insensitive comparison done in linux_virtual_usb_hub.py.
  local candidate="${1:-}"
  candidate="${candidate,,}"
  local pid
  for pid in "${TARGET_PIDS[@]}"; do
    if [[ "${candidate}" == "${pid}" ]]; then
      return 0
    fi
  done
  return 1
}

# Allow other scripts to source
# TARGET_VID, TARGET_PIDS, and is_target_pid() without running udev steps.
if [[ "${BASH_SOURCE[0]}" != "${0}" ]]; then
  return 0
fi

# The user whose access actually matters. The point of the udev rules is to let
# an unprivileged user toggle `authorized`; root can write it regardless, so
# testing root's access under `sudo ./udev.sh` would always report success and
# hide a missing or ineffective rule.
CHECK_USER="${SUDO_USER:-$(id -un)}"

if [[ "${EUID}" -eq 0 && -z "${SUDO_USER:-}" ]]; then
  echo "[WARNING]: Running as root without sudo, so the invoking user is" >&2
  echo "           unknown. Permission checks below reflect root's access and" >&2
  echo "           may pass even if the udev rules are not working. Re-run as" >&2
  echo "           a regular user (the script calls sudo where needed)." >&2
fi

# Returns 0 if CHECK_USER can write the given path.
is_writable_by_check_user() {
  local path="$1"
  if [[ "${CHECK_USER}" != "$(id -un)" ]]; then
    sudo -u "${CHECK_USER}" test -w "${path}"
  else
    [[ -w "${path}" ]]
  fi
}

# Runs a command, echoing a banner and its combined output.
#
# By default a non-zero exit status aborts the whole script. Pass `--soft` as
# the first argument to report the failure and continue instead.
run_cmd() {
  local fatal=1
  if [[ "${1:-}" == "--soft" ]]; then
    fatal=0
    shift
  fi
  echo ""
  echo "========================================================================"
  echo "[EXECUTING]: $*"
  echo "------------------------------------------------------------------------"
  local output
  local status=0
  if output=$("$@" 2>&1); then
    if [[ -n "${output}" ]]; then
      echo "${output}"
    else
      echo "(no command output)"
    fi
    echo "[RESULT]: SUCCESS"
  else
    status=$?
    if [[ -n "${output}" ]]; then
      echo "${output}"
    fi
    echo "[RESULT]: FAILED (exit code: ${status})" >&2
    if [[ "${fatal}" -eq 1 ]]; then
      exit "${status}"
    fi
  fi
  return "${status}"
}

# udev matches alternatives separated by `|`, so a single rule covers every
# PID in the shared list. `$kernel` is escaped so udev, not bash, expands it.
PID_ALTERNATIVES="$(IFS='|'; echo "${TARGET_PIDS[*]}")"
read -r -d '' EXPECTED_RULES << EOF || true
# Google USB VID (${TARGET_VID}) rules for Fuchsia / Fastboot virtual USB disconnect testing
# PIDs from //src/tests/end_to_end/usb/lib/fuchsia_usb_pids.json
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ATTR{idVendor}=="${TARGET_VID}", ATTR{idProduct}=="${PID_ALTERNATIVES}", RUN+="/bin/chmod a+w /sys/bus/usb/devices/\$kernel/authorized"
EOF

needs_sudo_setup=0
# Compare rule lines only, so comment-only changes do not force a sudo re-run.
if [[ ! -f "${RULE_FILE}" ]] || [[ "$(grep -v '^#' "${RULE_FILE}" 2>/dev/null)" != "$(grep -v '^#' <<< "${EXPECTED_RULES}")" ]]; then
  needs_sudo_setup=1
else
  # Rule file exists and matches; also verify any currently connected target device has write permission
  for dev in /sys/bus/usb/devices/*; do
    [[ -f "${dev}/idVendor" ]] || continue
    [[ "$(cat "${dev}/idVendor" 2>/dev/null)" == "${TARGET_VID}" ]] || continue
    is_target_pid "$(cat "${dev}/idProduct" 2>/dev/null)" || continue
    if ! is_writable_by_check_user "${dev}/authorized"; then
      needs_sudo_setup=1
      break
    fi
  done
fi

if [[ "${needs_sudo_setup}" -eq 0 ]]; then
  echo "========================================================================"
  echo "[PRE-CHECK]: ${RULE_FILE} is already installed and active."
  echo "             Skipping sudo installation and udevadm reload."
  echo "========================================================================"
else
  echo "========================================================================"
  echo "[EXECUTING]: sudo tee ${RULE_FILE} << 'EOF' ... EOF"
  echo "------------------------------------------------------------------------"
  if output=$(printf '%s\n' "${EXPECTED_RULES}" | sudo tee "${RULE_FILE}" 2>&1); then
    echo "${output}"
    echo "[RESULT]: SUCCESS (Wrote udev rules to ${RULE_FILE})"
  else
    status=$?
    echo "${output}"
    echo "[RESULT]: FAILED to write ${RULE_FILE} (exit code: ${status})" >&2
    exit "${status}"
  fi

  run_cmd sudo udevadm control --reload-rules
  # Replay `change` events so the rules apply to already-connected devices
  # without requiring a physical replug. Scoped to our VID so unrelated
  # hardware (audio, input, network) is left undisturbed.
  #
  # `--settle` waits for systemd-udevd to finish processing the synthetic
  # events. Without it the verification loop below can run before the RUN+=
  # chmod has executed and report a spurious failure.
  run_cmd sudo udevadm trigger --settle --subsystem-match=usb \
    --attr-match=idVendor="${TARGET_VID}"
fi

echo ""
echo "========================================================================"
echo "Verifying permissions on connected Fuchsia USB devices (VID ${TARGET_VID}, PID ${TARGET_PIDS[*]}):"
echo "------------------------------------------------------------------------"
found=0
all_writable=1
for dev in /sys/bus/usb/devices/*; do
  # USB interfaces (e.g. `1-6:1.0`) have no idVendor; only whole devices do.
  [[ -f "${dev}/idVendor" ]] || continue
  [[ "$(cat "${dev}/idVendor" 2>/dev/null)" == "${TARGET_VID}" ]] || continue
  pid=$(cat "${dev}/idProduct" 2>/dev/null || echo "unknown")
  # Skip other Google-branded peripherals sharing this VID; the rules above do
  # not cover them, so reporting them as failures would be misleading.
  is_target_pid "${pid}" || continue

  found=1
  dev_name=$(basename "${dev}")
  serial=$(cat "${dev}/serial" 2>/dev/null || echo "unknown")
  echo "Device ${dev_name} (VID: ${TARGET_VID}, PID: ${pid}, Serial: ${serial}):"
  # Informational only: a device unplugged mid-sweep must not abort the script.
  run_cmd --soft ls -l "${dev}/authorized"
  if is_writable_by_check_user "${dev}/authorized"; then
    echo "[VERIFIED]: User '${CHECK_USER}' HAS write permission to ${dev}/authorized"
  else
    echo "[WARNING]: User '${CHECK_USER}' does NOT have write permission to ${dev}/authorized" >&2
    all_writable=0
  fi
done

echo "========================================================================"
if [[ "${found}" -eq 0 ]]; then
  echo "[SUMMARY]: Rules installed successfully, but no supported Fuchsia USB device"
  echo "           (VID ${TARGET_VID}, PID ${TARGET_PIDS[*]}) is currently plugged in."
  echo "           Plug in your Fuchsia/Iris device and re-run this script to verify permissions."
elif [[ "${all_writable}" -eq 1 ]]; then
  echo "[SUMMARY]: ALL CHECKS PASSED! Your workstation is ready to run:"
  echo "           fx test --e2e usb_virtual_disconnect_test"
else
  echo "[SUMMARY]: Some devices are still not writable. Try unplugging and replugging the USB cable." >&2
  exit 1
fi
echo "========================================================================"
