#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This is a wrapper script that orchestrates multiple feature-enabling
# wrappers for running builds.
# Features:
#   --rbe : enable RBE using reproxy
#   --resultstore : enable uploading build metadata to ResultStore service
#   --profile : enable system profiling during build

set -euo pipefail

readonly SCRIPT_NAME="${BASH_SOURCE[0]##*/}"  # basename
readonly SCRIPT_DIR="${BASH_SOURCE[0]%/*}"  # dirname

# TODO: remove dependence on FUCHSIA_DIR and fuchsia-specific conventions,
# apart from locations of essential scripts and tools.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

readonly profile_wrapper="${FUCHSIA_DIR}/build/profile/profile_wrap.sh"
readonly reproxy_wrapper="${FUCHSIA_DIR}/build/rbe/fuchsia-reproxy-wrap.sh"
readonly rsproxy_wrapper="${FUCHSIA_DIR}/build/resultstore/fuchsia-rsproxy-wrap.sh"

verbose=0
function debug() {
  [[ "$verbose" == 0 ]] || echo "[$SCRIPT_NAME]: $*"
}

function die() {
  echo "[$SCRIPT_NAME] Error: $*"
  exit 1
}

function usage() {
  cat <<EOF
usage: $SCRIPT_NAME [options] -- build-command...

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

  The wrapped build command follows --.
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
build_tool_args=()  # The wrapped command
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
[[ "${#build_tool_args[@]}" -ge 1 ]] || {
  die "Missing wrapped command after --."
}

# Determine if this is a build-like command that needs wrapper orchestration.
# Bazel's build-like commands include 'build', 'test', and 'run'.
# Non-build commands (like bazel info, help, query) should bypass wrappers.
# Also try to infer build_dir from build tool args (e.g. ninja -C).
is_build=0
inferred_build_dir=
state="START"
for arg in "${build_tool_args[@]}"; do
  case "$state" in
    START)
      case "$arg" in
        env | *=*) continue ;;  # skip environment variable prefixes
        */bazel | bazel) state="GOT_BAZEL_LOOKING_FOR_SUBCOMMAND" ;;
        *ninja*)
          is_build=1
          state="GOT_NINJA_LOOKING_FOR_ARGS"
          ;;
        *) state="DONE" ;;
      esac
      ;;
    GOT_BAZEL_LOOKING_FOR_SUBCOMMAND)
      case "$arg" in
        -*) continue ;;
        build|test|run)
          is_build=1
          state="DONE"
          ;;
        *) state="DONE" ;;
      esac
      ;;
    GOT_NINJA_LOOKING_FOR_ARGS)
      case "$arg" in
        -C) state="GOT_NINJA_EXPECT_C_DIR" ;;
        -C*)
          inferred_build_dir="${arg#-C}"
          ;;
        -n | --dry-run | -t | -t* | -h | --help | --version)
          is_build=0
          ;;
      esac
      ;;
    GOT_NINJA_EXPECT_C_DIR)
      inferred_build_dir="$arg"
      state="GOT_NINJA_LOOKING_FOR_ARGS"
      ;;
  esac
  [[ "$state" != "DONE" ]] || break
done

if [[ "$is_build" == 0 ]]; then
  debug "Bypassing wrapper orchestration for non-build command: ${build_tool_args[*]}"
  exec "${build_tool_args[@]}"
fi

[[ -n "$build_dir" ]] || {
  build_dir="$inferred_build_dir"
  [[ -z "$build_dir" ]] || debug "Inferred build-dir: $build_dir"
}
[[ -n "$build_dir" ]] || die "Missing required --build-dir option."

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
  mkdir -p "$profile_log_dir"
  readonly vmstat_log="${profile_log_dir}/vmstat.log"
  readonly ifconfig_log="${profile_log_dir}/ifconfig.log"
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
  mkdir -p "$reproxy_logdir"
  # reproxy works best when it uses a temp dir on the same physical device
  # as the build dir.
  # Reuse the timestamp-based part of $log_dir.
  readonly reproxy_tmpdir="$build_dir/.reproxy_tmpdirs/${log_dir##*/}"
  mkdir -p "$reproxy_tmpdir"

  reproxy_cfg_args=()
  for f in "${reproxy_cfgs[@]}"
  do reproxy_cfg_args+=( --cfg "$f" )
  done

  reproxy_shutdown_opts=()
  if [[ "$enable_resultstore" == 0 ]]
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

  "${build_tool_args[@]}"
)

debug "full command: ${full_cmd[*]}"
exec "${full_cmd[@]}"

