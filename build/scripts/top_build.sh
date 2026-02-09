#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This is a standalone script that works like 'fx build'.
# Much of the logic in 'fx build' around invoking the build
# tools like ninja will be moved here.
# It is assumed that configuration steps like GN have already completed.

set -euo pipefail

readonly SCRIPT_NAME="${BASH_SOURCE[0]##*/}"  # basename
readonly SCRIPT_DIR="${BASH_SOURCE[0]%/*}"  # dirname

# TODO: remove dependence on FUCHSIA_DIR and fuchsia-specific conventions,
# apart from locations of essential scripts and tools.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

# Currently uses ninja to build, but could eventually be bazel.
readonly build_tool="$PREBUILT_NINJA"
readonly jq="$PREBUILT_JQ"
readonly profile_wrapper="${FUCHSIA_DIR}/build/profile/profile_wrap.sh"
readonly reproxy_wrapper="${FUCHSIA_DIR}/build/rbe/fuchsia-reproxy-wrap.sh"
readonly rsproxy_wrapper="${FUCHSIA_DIR}/build/resultstore/fuchsia-rsproxy-wrap.sh"

verbose=0
function debug() {
  [[ "$verbose" == 0 ]] || echo "[$SCRIPT_NAME]: $*"
}

function error() {
  echo "[$SCRIPT_NAME] Error: $*"
  exit 1
}

function usage() {
  cat <<EOF
usage: $SCRIPT_NAME [options] ...

options:
  -h | --help : print help and exit
  -v | --verbose : run verbosely

  --build-dir DIR : (required) build output dir (depth=2, e.g. "out/foo")
      When building with ninja, the -C argument is interpreted as the build dir.
  --log-dir DIR : where to produce logs for various build tools.
      Defaults to using a timestamp-based directory name under out/_build_logs.

  --loas-type : LOAS type used to choose authentication method
      values: restricted, unrestricted, auto, skip

  --profile[=0] : enable system profiling during build

  --rbe[=0] : enable support for remote execution
  --reproxy-cfg : additional reproxy configs to merge and use

  --resultstore[=0] : uploading build metadata and results to ResultStore
  --pre-build-uploads : configure-time invocation artifacts to upload
  --post-build-uploads : end-of-build invocation artifacts to upload

  All unrecognized options are forwarded directly to $build_tool.
EOF
}

### Defaults.
collect_system_profile=0  # fx build-profile
enable_resultstore=0
needs_reproxy_rbe=0

### Configuration.
# Parse command-line arguments.
build_dir=
log_dir=
loas_type=
reproxy_cfgs=()
pre_build_uploads=()
post_build_uploads=()
build_tool_args=()
prev_opt=
prev_opt_append=
for opt  # "$@"
do
  # handle --option arg
  if [[ -n "$prev_opt" ]]
  then
    eval "$prev_opt"=\$opt
    prev_opt=
    shift
    continue
  fi
  if [[ -n "$prev_opt_append" ]]
  then
    eval "$prev_opt_append"+=\(\$opt\)
    prev_opt_append=
    shift
    continue
  fi

  # Extract optarg from --opt=optarg
  optarg=
  case "$opt" in
    -*=*) optarg="${opt#*=}" ;;  # remove-prefix, shortest-match
  esac

  case "$opt" in
    -h | --help) usage; exit ;;
    -v | --verbose) verbose=1 ;;

    --build-dir=*) build_dir="$optarg" ;;
    --build-dir) prev_opt=build_dir ;;

    --log-dir=*) log_dir="$optarg" ;;
    --log-dir) prev_opt=log_dir ;;

    --loas-type=*) loas_type="$optarg" ;;
    --loas-type) prev_opt=loas_type ;;

    --profile=*) collect_system_profile="$optarg" ;;
    --profile) collect_system_profile=1 ;;

    --rbe=*) needs_reproxy_rbe="$optarg" ;;
    --rbe) needs_reproxy_rbe=1 ;;
    --reproxy-cfg=*) reproxy_cfgs+=( "$optarg" ) ;;
    --reproxy-cfg) prev_opt_append=reproxy_cfgs ;;

    --resultstore=*) enable_resultstore="$optarg" ;;
    --resultstore) enable_resultstore=1 ;;
    --pre-build-uploads=*) pre_build_uploads="$optarg" ;;
    --pre-build-uploads) prev_opt=pre_build_uploads ;;
    --post-build-uploads=*) post_build_uploads="$optarg" ;;
    --post-build-uploads) prev_opt=post_build_uploads ;;

    # Stop option processing.
    --) shift; break ;;

    # Forward unknown options to $build_tool.
    *) build_tool_args+=( "$opt" ) ;;
  esac
  shift
done

[[ -z "$prev_opt" ]] || {
  die "Missing --${prev_opt} argument."
}
[[ -z "$prev_opt_append" ]] || {
  die "Missing --${prev_opt_append} argument."
}

build_tool_args+=( "$@" )

[[ -n "$build_dir" ]] || {
  case "$build_tool" in
    *ninja*)
      # Assume build_dir from ninja -C DIR.
      prev_opt=
      for "$opt" in "${build_tool_args[@]}"
      do
        if [[ -n "$prev_opt" ]]
        then
          eval "$prev_opt"=\$opt
          prev_opt=
          continue
        fi
        case "$opt" in
          -C) prev_opt=build_dir ;;
        esac
      done
      ;;
  esac
  debug "Inferred build-dir: $build_dir"
}
[[ -n "$build_dir" ]] || error "Missing required --build-dir option."

readonly build_dir_basename="${build_dir##*/}"  # basename

[[ -n "$log_dir" ]] || {
  # Then choose our own log dir, based on $build_dir.
  readonly out_dir_root="${FUCHSIA_DIR}/out"
  readonly build_logs_root="$out_dir_root/_build_logs"
  readonly log_dir_base="$build_logs_root/$build_dir_basename"
  mkdir -p "$log_dir_base"
  readonly timestamp="$(date +%Y%m%d-%H%M%S)"
  log_dir="$(mktemp -d "${log_dir_base}/build.${timestamp}.XXXXXXXX")"
  debug "Using newly created log dir: $log_dir"
}

loas_type_arg=()
[[ -z "$loas_type" ]] || loas_type_arg+=( --loas-type "$loas_type" )

### Composition.
# Stack prefix wrappers based on enabled features.

maybe_fint_build_wrap=()
# TODO: support fint build wrapper (for infra)

maybe_profile_wrap=()
if [[ "$collect_system_profile" == 1 ]]
then
  debug "Profiling enabled."
  readonly profile_log_dir="$log_dir/build_profile"
  readonly vmstat_log="${profile_dir}/vmstat.log"
  readonly ifconfig_log="${profile_dir}/ifconfig.log"
  maybe_profile_wrap=(
    "$profile_wrapper"
    --vmstat-log "$vmstat_log"
    --ifconfig-log "$ifconfig_log"
    --
  )
  post_build_uploads+=(
    # trace files
    "$vmstat_log.json"
    "$ifconfig_log.json"
  )
fi

maybe_rbe_wrap=()
if [[ "$needs_reproxy_rbe" == 1 ]]
then
  debug "RBE enabled."
  readonly reproxy_logdir="$log_dir/reproxy_logs"
  # reproxy works best when it uses a temp dir on the same physical device
  # as the build dir.
  # Reuse the timestamp-based part of $log_dir.
  readonly reproxy_tmpdir="$build_dir/.reproxy_tmpdirs/${log_dir##*/}"

  reproxy_cfg_args=()
  for f in "${reproxy_cfgs[@]}"
  do reproxy_cfg_args+=( --cfg "$f" )
  done

  reproxy_shutdown_opts=()
  if [[ "$enable_resultstore" == 1 ]]
  then
    # When resultstore is enabled, we need to wait for reproxy to fully
    # shutdown to guarantee that produces the logs and metrics that will
    # be uploaded as post-build artifacts by rsproxy.
    # Otherwise, allow reproxy to shutdown asynchronously.
    reproxy_shutdown_opts+=( --async_reproxy_termination )
  fi

  maybe_rbe_wrap=(
    "$reproxy_wrapper"
    --logdir "$reproxy_logdir"
    --tmpdir "$reproxy_tmpdir"
    "${loas_type_arg[@]}"
    "${reproxy_cfg_args[@]}"
    "${reproxy_shutdown_opts[@]}"
    --
  )
  post_build_uploads+=(
    "$reproxy_logdir/rbe_metrics.txt"
    # TODO: consider compressing the reproxy log
    "$reproxy_logdir/reproxy.rrpl"
  )
fi

maybe_resultstore_wrap=()
if [[ "$enable_resultstore" == 1 ]]
then
  debug "ResultStore enabled."
  readonly rsproxy_logdir="$log_dir/rsproxy_logs"

  rsproxy_options=()

  for f in "${pre_build_uploads[@]}"
  do rsproxy_options+=( --pre_build_uploads "$f" )
  done

  for f in "${post_build_uploads[@]}"
  do rsproxy_options+=( --post_build_uploads "$f" )
  done

  maybe_resultstore_wrap=(
    "$rsproxy_wrapper"
    "${loas_type_arg[@]}"
    --log-dir "$rsproxy_logdir"
    "${rsproxy_options[@]}"
    --
  )
fi

### Execution.
readonly full_cmd=(
  "${maybe_fint_build_wrap[@]}"
  "${maybe_resultstore_wrap[@]}"
  "${maybe_profile_wrap[@]}"
  "${maybe_rbe_wrap[@]}"
  "$build_tool"
  "${build_tool_args[@]}"
)

debug "full command: ${full_cmd[*]}"
exec "${full_cmd[@]}"

