#!/bin/bash
# Copyright 2017 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

devshell_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
FUCHSIA_DIR="$(dirname $(dirname $(dirname "${devshell_lib_dir}")))"

if [[ -d "${FUCHSIA_DIR}/.jiri_root/bin" ]]; then
  rm -f "${FUCHSIA_DIR}/.jiri_root/bin/fx"
  ln -s "../../scripts/fx" "${FUCHSIA_DIR}/.jiri_root/bin/fx"

  rm -f "${FUCHSIA_DIR}/.jiri_root/bin/ffx"
  ln -s "../../src/developer/ffx/scripts/ffx" "${FUCHSIA_DIR}/.jiri_root/bin/ffx"

  rm -f "${FUCHSIA_DIR}/.jiri_root/bin/hermetic-env"
  ln -s "../../scripts/hermetic-env" "${FUCHSIA_DIR}/.jiri_root/bin/hermetic-env"

  rm -f "${FUCHSIA_DIR}/.jiri_root/bin/fuchsia-vendored-python"
  ln -s "../../scripts/fuchsia-vendored-python" "${FUCHSIA_DIR}/.jiri_root/bin/fuchsia-vendored-python"
fi

# In infrastructure checkouts (or checkouts initialized with an external jiri),
# jiri is provisioned outside .jiri_root/bin, so .jiri_root/bin/jiri is not
# populated or updated by default. Copy the invoking jiri binary (or jiri from
# PATH) into .jiri_root/bin/jiri so build actions running inside nsjail can
# invoke it.
if [[ -d "${FUCHSIA_DIR}/.jiri_root/bin" ]]; then
  # Skip when .jiri_root/bin/jiri is already a valid symlink (e.g. CartFS
  # workspaces which invoke this hook via Python rather than jiri). For regular
  # files or missing/dangling paths, check whether .jiri_root/bin/jiri needs to
  # be populated or synced with an external jiri that self-updated in place.
  if [[ ! -L "${FUCHSIA_DIR}/.jiri_root/bin/jiri" || ! -f "${FUCHSIA_DIR}/.jiri_root/bin/jiri" || -n "${INFRA_RECIPES:-}" ]]; then
    jiri_bin=""
    # jiri executes hooks as direct child processes (${PPID}), and is not on
    # PATH on LUCI bots (while also prepending .jiri_root/bin to PATH itself).
    # Inspect the parent process first via /proc (Linux) and ps (macOS/POSIX)
    # before falling back to PATH lookup.
    for candidate in \
      "$(readlink -f "/proc/${PPID}/exe" 2>/dev/null || true)" \
      "$(ps -p "${PPID}" -o comm= 2>/dev/null || true)" \
      "$(ps -p "${PPID}" -o args= 2>/dev/null | awk '{print $1}')" \
      "$(command -v jiri 2>/dev/null || true)"; do
      if [[ "$(basename "${candidate}")" == "jiri" && -f "${candidate}" && -x "${candidate}" ]]; then
        jiri_bin="${candidate}"
        break
      fi
    done
    if [[ -n "${jiri_bin}" ]]; then
      # Only copy when the binary is missing or its contents differ, so we do
      # not bump the mtime of //.jiri_root/bin/jiri on no-op hook runs and
      # trigger unnecessary Ninja rebuilds for targets listing it in inputs.
      if [[ ! -f "${FUCHSIA_DIR}/.jiri_root/bin/jiri" ]] || ! cmp -s "${jiri_bin}" "${FUCHSIA_DIR}/.jiri_root/bin/jiri"; then
        # Stage to a temporary file and atomically rename with mv -f. This
        # avoids ETXTBSY if .jiri_root/bin/jiri is currently running and avoids
        # EACCES when replacing a read-only (0555) CIPD binary. Guard cp and
        # chmod since set -e is not enabled in this script.
        if cp -f "${jiri_bin}" "${FUCHSIA_DIR}/.jiri_root/bin/jiri.tmp.$$" &&
           chmod +x "${FUCHSIA_DIR}/.jiri_root/bin/jiri.tmp.$$"; then
          mv -f "${FUCHSIA_DIR}/.jiri_root/bin/jiri.tmp.$$" "${FUCHSIA_DIR}/.jiri_root/bin/jiri"
        else
          rm -f "${FUCHSIA_DIR}/.jiri_root/bin/jiri.tmp.$$"
        fi
      fi
    fi
  fi
fi
