#!/bin/bash
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

function get_host_tools_dir {
  local -r build_dir="$(fx-build-dir-if-present && echo "${FUCHSIA_BUILD_DIR}")"
  local -r host_tools="${build_dir}/host-tools"
  if [[ -d "${build_dir}" && -d "${host_tools}"  ]]; then
    echo "${host_tools}"
  fi
}

function fx-ensure-prebuilt {
  if [[ $# -eq 0 ]]; then
    fx-error "fx-ensure-prebuilt requires at least one argument"
    return 1
  fi

  local item rel_path env_var
  local prebuilt_root="${FUCHSIA_DIR}/prebuilt"
  local packages_to_fetch=()

  for item in "$@"; do
    if [[ -z "${item}" ]]; then
      fx-error "Empty path passed to fx-ensure-prebuilt"
      return 1
    fi

    if [[ "${item}" == "${prebuilt_root}/"* ]]; then
      rel_path="prebuilt/${item#"${prebuilt_root}/"}"
    elif [[ "${item}" == //prebuilt/* ]]; then
      rel_path="prebuilt/${item#//prebuilt/}"
    elif [[ "${item}" == prebuilt/* ]]; then
      rel_path="${item}"
    else
      fx-error "Invalid prebuilt path '${item}': paths passed to fx-ensure-prebuilt must be in //prebuilt"
      return 1
    fi

    # Skip if already loaded earlier in this fx process tree or earlier in "$@"
    env_var="_FX_LOADED_DEP_${rel_path//[^a-zA-Z0-9_]/_}"
    if [[ -n "${!env_var:-}" ]]; then
      continue
    fi
    export "${env_var}=1"
    packages_to_fetch+=("${rel_path}")
  done

  if [[ ${#packages_to_fetch[@]} -eq 0 ]]; then
    return 0
  fi

  # TODO: FX_ON_DEMAND_PREBUILTS_LOG_FETCHES is a temporary placeholder used to
  # verify on-demand prebuilt resolution before `jiri fetch-package` is
  # implemented. Remove this variable and log file once `fx-ensure-prebuilt`
  # calls `jiri fetch-package` directly.
  if [[ -n "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES:-}" && "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES}" != "0" && "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES}" != "false" ]]; then
    local log_file="${FUCHSIA_DIR}/out/.on-demand-fetch-log"
    mkdir -p "${FUCHSIA_DIR}/out"
    echo "jiri fetch-package ${packages_to_fetch[*]}" >> "${log_file}"
  fi
  return 0
}

# Prints the value of each `#### <key>=<value>` metadata header line in the
# given file, one per line, with ${VAR} placeholders expanded.
#
# Placeholders are expanded from the environment. FUCHSIA_DIR, HOST_PLATFORM,
# and all PREBUILT_* variables from platform.sh (which are declared readonly
# without export) are exported first so they are available.
#
# Args:
#   $1: The metadata key, e.g. EXECUTABLE or PREBUILTS.
#   $2: The metadata file to read.
#   $3: How to handle unknown variables: "keep" to leave them unexpanded, or
#       "empty" to expand them to an empty string.
function _get_metadata_values {
  local -r key=$1
  local -r metadata_file=$2
  local -r unknown_vars=$3

  export FUCHSIA_DIR HOST_PLATFORM
  if [[ -n "${!PREBUILT_*}" ]]; then
    export "${!PREBUILT_@}"
  fi

  awk -F ' *= *' -v key="${key}" -v unknown_vars="${unknown_vars}" \
      -f - "${metadata_file}" <<'EOF'
    $0 ~ ("^#### +" key " *=") {
      # Build the output left-to-right so expanded values are never re-scanned.
      line = $2;
      out = "";
      while (match(line, /\$\{[A-Za-z0-9_]+\}/)) {
        var = substr(line, RSTART + 2, RLENGTH - 3);
        if (var in ENVIRON) {
          val = ENVIRON[var];
        } else if (unknown_vars == "keep") {
          val = substr(line, RSTART, RLENGTH);
        } else {
          val = "";
        }
        out = out substr(line, 1, RSTART - 1) val;
        line = substr(line, RSTART + RLENGTH);
      }
      print out line;
    }

    # Stop scanning at the first non-empty, non-comment line so we only inspect
    # the metadata header block and avoid scanning the body of large scripts or
    # binaries.
    !/^#/ && NF > 0 { exit }
EOF
}

function get_exec_from_metadata {
  local -r metadata_file=$1

  export HOST_TOOLS_DIR="$(get_host_tools_dir)"
  # Leave unknown variables unexpanded so that a typo fails loudly as a missing
  # executable, rather than silently expanding to an empty string and changing
  # which command runs.
  _get_metadata_values EXECUTABLE "${metadata_file}" keep
}

function get_prebuilts_from_metadata {
  local -r metadata_file=$1
  [[ -f "${metadata_file}" ]] || return 0

  # Unknown variables expand to empty and are dropped, since a prebuilt variable
  # may not be defined in every environment (e.g. trimmed-down test
  # environments). Split each value on whitespace (or commas/semicolons) and
  # print each prebuilt path on its own line.
  _get_metadata_values PREBUILTS "${metadata_file}" empty | tr -s '[:space:],;' '\n'
}

function _relative {
  cmd="$1"
  if [[ "${cmd}" == *"${FUCHSIA_DIR}"* ]]; then
    echo "//${cmd#${FUCHSIA_DIR}/}"
  else
    echo "${cmd}"
  fi
}

function find_executable {
  local cmd_name="$1"
  local cmd_path
  cmd_path="$(commands "${cmd_name}")"
  # no metadata, so let's try to find the file
  if [[ -z "${cmd_path}" ]]; then
    # no file in regular script directories, look in host_tools
    cmd_path="$(find_host_tools "${cmd_name}")"
  fi
  # If multiple commands match, use the first one
  local first_cmd_path="${cmd_path%%$'\n'*}"
  find_exec_from_path "${first_cmd_path}"
}

function find_exec_from_path {
  local cmd_path="$1"
  cmd_path="${cmd_path%.fx}"
  local fx_file_path="${cmd_path}.fx"
  local prebuilts_to_ensure=(
    $(get_prebuilts_from_metadata "${fx_file_path}")
    $(get_prebuilts_from_metadata "${cmd_path}")
  )

  local target_exec="${cmd_path}"
  local from_metadata=""
  if [[ -f "${fx_file_path}" ]]; then
    from_metadata="$(get_exec_from_metadata "${fx_file_path}")"
    if [[ -n "${from_metadata}" ]]; then
      target_exec="${from_metadata%% *}"
    fi
  fi

  if [[ "${target_exec}" == "${FUCHSIA_DIR}/prebuilt/"* ]]; then
    prebuilts_to_ensure+=("${target_exec}")
  fi
  if [[ ${#prebuilts_to_ensure[@]} -gt 0 ]]; then
    fx-ensure-prebuilt "${prebuilts_to_ensure[@]}" || return 1
  fi

  if [[ -n "${from_metadata}" ]]; then
    echo "${from_metadata}"
    if [[ -f "${cmd_path}" && ! "${cmd_path}" -ef "${from_metadata}" ]]; then
      fx-error "Invalid ${fx_file_path}: if both ${basename_exec} and EXECUTABLE metadata exist, they must point to the same file"
      return 1
    fi
  else
    echo "${cmd_path}"
    if [[ ! -x "${cmd_path}" ]]; then
      return 1
    fi
  fi
}

function find_execs_and_metadata {
  local cmd_name=$1
  shift

  if [[ -z "${cmd_name}" ]]; then
    cmd_name="*"
  fi
  local dirs=()
  for d in "$@"; do
    if [[ -d "${d}" ]]; then
      dirs+=( "${d}" )
    fi
  done
  if [[ ${#dirs[@]} -eq 0 ]]; then
    return 0
  fi

  # run find assuming "-executable" is supported
  cmds="$( find "${dirs[@]}" -maxdepth 1 -type f \
    \( -executable -name "${cmd_name}" \) -o \
    \( -name "${cmd_name}.fx" \) 2>/dev/null )"

  if [[ $? -ne 0 ]]; then
    # assume that the error was caused because versions of find older than 4.3.0
    # don't support -executable. Run with -perm +100 instead, which is not
    # supported in versions of find newer than 4.5.12, so it can't be used always.
    cmds="$( find "${dirs[@]}" -maxdepth 1 -type f \
      \( -perm +100 -name "${cmd_name}" \) -o \
      \( -name "${cmd_name}.fx" \) 2>/dev/null )"
    if [[ $? -ne 0 ]]; then
      {
        echo "ERROR: 'find' failed unexpectedly, please execute fx with '-x' and report a bug."
        echo "At least one of the commands below was expected to work:"
        echo 'find ' "${dirs[@]}" '-maxdepth 1 -type f \( -executable -name' "\"${cmd_name}\"" '\) -o \( -name ' "\"${cmd_name}.fx\"" '\)'
        echo 'find ' "${dirs[@]}" '-maxdepth 1 -type f \( -perm +100 -name' "\"${cmd_name}\"" '\) -o \( -name ' "\"${cmd_name}.fx\"" '\)'
      } >&2
      exit 1
    fi
  fi
  echo "${cmds}"
}


function find_host_tools {
  local cmd_name=$1
  local -r host_tools="$(get_host_tools_dir)"
  if [[ -z "${host_tools}" ]]; then
    return
  fi
  # get a list of non-host-tools commands and metadata files, separated by
  # semi-colon.
  cmds="$(commands | tr '\n' ';')"

  binaries=()
  # do not list a host tool if there's a subcommand script or an .fx metadata
  # file with the same name.
  for binary in $(find_execs_and_metadata "${cmd_name}" "${host_tools}"); do
    name="${binary##*/}"   # remove path, equivalent to basename but faster
    # only return the binary if no {binary} or {binary}.fx as other commands
    if [[ "${cmds}" != */${name}.fx\;* && "${cmds}" != */${name}\;* ]]; then
      binaries+=( "${binary}" )
    fi
  done
  echo "${binaries[@]}"
}


function commands {
  local cmd_name=${1:-}
  local dirs
  # handle "vendor VENDOR COMMAND"
  if [[ "${cmd_name}" == "vendor" && $# -eq 3 ]]; then
    vendor=$2
    cmd_name=$3
    dirs=("${FUCHSIA_DIR}"/vendor/${vendor}/scripts/devshell)
  # handle "vendor VENDOR"
  elif [[ "${cmd_name}" == "vendor" && $# -eq 2 ]]; then
    vendor=$2
    cmd_name=""
    dirs=("${FUCHSIA_DIR}"/vendor/${vendor}/scripts/devshell)
  else
    dirs=("${FUCHSIA_DIR}"/vendor/*/scripts/devshell "${FUCHSIA_DIR}"/tools/devshell{,/contrib})
  fi

  find_execs_and_metadata "${cmd_name}" "${dirs[@]}"
}
