#!/bin/bash
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -e

# Either update or check the content of //build/icu/jiri_generated/config.json
# NOTE: This is called by a Jiri hook!

_FUCHSIA_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../" >/dev/null 2>&1 && pwd)"
_DEFAULT_OUTPUT="${_FUCHSIA_DIR}/build/icu/jiri_generated/config.json"
_GET_REV_SCRIPT="${_FUCHSIA_DIR}/tools/devshell/lib/get_fuchsia_subdir_git_revision.sh"

die () {
  echo >&2 "ERROR: $*"
  return 1
}

function get_repo_rev() {
  local repo_path="$1"
  local commit_id="$("${_GET_REV_SCRIPT}" "${_FUCHSIA_DIR}" "${repo_path}")" || die "commit id not found in ${repo_path}"
  echo "${commit_id}"
}

function usage {
  cat <<EOF
Usage: ${_BASH_SOURCE[0]} [options]

Compute the ICU config.json file from the current state of the checkout.
Valid options:

  --output=FILE   Write result to a file. Default is ${_DEFAULT_OUTPUT}

  --mode=write    Write the generated config file to output file (default).

  --mode=print    Print the generated config file instead of writing it
                  to the output file.

  --mode=check    Check that the generated config file matches the current
                  output file content.

  --timestamp=FILE Touch FILE if this command runs successfully.
EOF
  return 1
}

icu_default_dir=
icu_latest_dir=
output_file="${_DEFAULT_OUTPUT}"
timestamp_file=
mode="write"
for OPT; do
  case "$OPT" in
    --output=*)
      output_file="${OPT#--*=}"
      ;;
    --icu-default-dir=*)
      icu_default_dir="${OPT#--*=}"
      ;;
    --icu-latest-dir=*)
      icu_latest_dir="${OPT#--*=}"
      ;;
    --mode=*)
      mode="${OPT#--*=}"
      ;;
    --timestamp=*)
      timestamp_file="${OPT#--*=}"
      ;;
    --help)
      usage
      ;;
    -*)
      die "Invalid option $OPT, see --help."
      ;;
    *)
      die "This script does not take arguments, see --help."
      ;;
  esac
done

if [[ -z "${icu_default_dir}" ]]; then
  icu_default_dir="third_party/icu/default"
fi
if [[ -z "${icu_latest_dir}" ]]; then
  icu_latest_dir="third_party/icu/latest"
fi

default_commit_id="$(get_repo_rev "${icu_default_dir}")"

latest_commit_id="$(get_repo_rev "${icu_latest_dir}")"

# Compute the new JSON content.
content=$(cat <<EOF
{
  "default": "${default_commit_id}",
  "latest": "${latest_commit_id}"
}
EOF
)

case "$mode" in
  write)
    printf "%s" "${content}" > "${output_file}"
    ;;
  print)
    printf "%s" "${content}"
    ;;
  check)
    cur_content="$(cat "${output_file}")" || die "Cannot read: ${output_file}"
    if [[ "${cur_content}" != "${content}" ]]; then
      echo >&2 "Output file ${output_file} not up-to-date!"
      diff -burN "${output_file}" <(printf %s "$content") >&2 || true
      cat >&2 <<EOF

This means the //third_party/icu/{default,latest} git HEADs have changed since
your last 'jiri update'. Please invoke the following script manually to fix this:

${BASH_SOURCE[0]}

EOF
      exit 1
    fi
    ;;
  *)
    die "Invalid --mode=${mode} value, must be one of: check, print, write"
    ;;
esac

if [[ -n "${timestamp_file}" ]]; then
  touch "${timestamp_file}"
fi
