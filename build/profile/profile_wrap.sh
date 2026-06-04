#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Collect system-wide profile information using system_profiler.py
# for the duration of a wrapped command, logging outputs to file.

readonly script="$0"
# assume script is always with path prefix, e.g. "./$script"
readonly script_dir="${script%/*}"
readonly script_basename="${script##*/}"

function msg() {
  echo >&2 "[$script_basename] $*"
}

function usage() {
  cat <<END
Usage: $script \
  --system-log system_logfile \
  [script_args] -- command...
END
}

system_logfile=
interval=1  # seconds
prev_opt=

for opt
do
  # handle --option arg
  if [[ -n "$prev_opt" ]]
  then
    eval "$prev_opt"=\$opt
    prev_opt=
    shift
    continue
  fi

  # Extract optarg from --opt=optarg
  optarg=
  case "$opt" in
    -*=*) optarg="${opt#*=}" ;;  # remove-prefix, shortest-match
  esac

  case "$opt" in
    --system-log) prev_opt=system_logfile ;;
    --system-log=*) system_logfile="$optarg" ;;
    -n) prev_opt=interval ;;
    -n=*) interval="$optarg" ;;
    --) shift ; break ;;
    *) echo "Unknown $0 option: $opt" ; usage ; exit 1 ;;
  esac
  shift
done

[[ -n "$system_logfile" ]] || {
  echo "--system-log is required."
  exit 1
}

# Everything else after '--' is the command to run.
cmd=("$@")

[[ "$#" > 0 ]] || { echo "Missing command to run (after --)." ; exit 1; }

shutdown_pids=()

if [[ -n "$system_logfile" ]]
then
  rm -f "$system_logfile"
  "${PREBUILT_PYTHON3:-python3}" -S -u "${script_dir}/system_profiler.py" \
    --interval "$interval" \
    --output "$system_logfile" \
    --pid "$$" \
    --metadata "FX_BUILD_UUID:${FX_BUILD_UUID:-}" &
  readonly system_profiler_pid=$!
  shutdown_pids+=( "$system_profiler_pid" )
fi

# Terminate system_profiler when main command is complete (or interrupted).
function shutdown() {
  if [[ "${#shutdown_pids[@]}" > 0 ]]
  then
    if [[ "${_interrupted:-0}" == "1" ]]; then
      msg "Stopping background profile collection..."
    fi
    kill "${shutdown_pids[@]}"
  fi
}
trap shutdown EXIT

# Wait for a command while ignoring signals to ensure the parent outlives the child.
# This prevents the shell from exiting prematurely and orphaning backgrounded
# subprocesses during a signal (like Ctrl-C).
#
# Because the command is run in the same process group, signals (like SIGINT)
# are broadcast by the TTY to both the shell and the child, so no manual
# signal forwarding is required here. Successive signals will continue to reach
# the child as long as it is alive.
function wait-ignoring-signals {
  local sig_count=0
  # Acknowledge signals but stay alive while waiting.
  function _signal_acknowledgement_handler {
    local sig="$1"
    _interrupted=1
    sig_count=$((sig_count + 1))
    if [[ $sig_count -eq 1 ]]; then
      msg "Received ${sig}. Waiting for command to shut down gracefully..."
    else
      msg "Received ${sig} again (${sig_count}). Still waiting for cleanup..."
    fi
  }
  trap '_signal_acknowledgement_handler SIGINT' INT
  trap '_signal_acknowledgement_handler SIGTERM' TERM
  trap '_signal_acknowledgement_handler SIGHUP' HUP

  # Run the command in a subshell that restores default signal dispositions.
  ( trap - INT TERM HUP ; exec "$@" )
  local status=$?

  trap - INT TERM HUP
  return "$status"
}

_interrupted=0
wait-ignoring-signals "${cmd[@]}"
exit "$?"
