#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# fuchsia-rsproxy-wrap.sh invokes rsclient's rsproxy-wrap.sh, but
# with fuchsia-specific configurations and features.

set -euo pipefail

readonly SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"
readonly SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"

# Get the HOST_PLATFORM for the prebuilt path.
# Sourcing platform.sh requires FUCHSIA_DIR to be set.
readonly FUCHSIA_DIR="$(readlink -f "$SCRIPT_DIR/../..")"
source "${FUCHSIA_DIR}/tools/devshell/lib/platform.sh"

readonly check_loas_script="${FUCHSIA_DIR}/build/rbe/check_loas_restrictions.sh"

# rsclient install path is set in manifests/prebuilts
readonly PREBUILT_RSCLIENT_DIR="${FUCHSIA_DIR}/prebuilt/rsclient/$HOST_PLATFORM"
readonly proxy_wrap="$PREBUILT_RSCLIENT_DIR/bin/rsproxy-wrap.sh"
readonly rsproxy="$PREBUILT_RSCLIENT_DIR/bin/rsproxy"

# Use re-client's credentials helper tool to exchange LOAS for OAuth2 tokens.
readonly credshelper="${PREBUILT_RECLIENT_DIR}/credshelper"

# default options:
if command -v gcert >/dev/null 2>&1; then
  # Detect LOAS type if it is not already passed in.
  loas_type=auto
else
  # Assume this in an infra environment, and do not attempt to
  # use any credential helpers.
  loas_type=skip
fi
verbose=0

function die() {
  echo "[$SCRIPT_NAME]: Error: $*" >&2
  exit 1
}

function debug_msg() {
  [[ "$verbose" == 0 ]] || {
    echo "[$SCRIPT_NAME]: $*"
  }
}

function usage() {
  cat <<EOF
usage: $0 [options] -- command ...

options:
  -h | --help: print help and exit
  --loas-type TYPE: {skip,auto,restricted,unrestricted}, default [$loas_type]
    'skip' will bypass any preflight authentication checks
    'auto' will attempt to detect as restricted or unrestricted.
  -v | --verbose: print debug messages

  Unrecognized options before -- will be forwarded to rsproxy.
EOF
}

# Parse options up to --, and treat the rest as the wrapped command.
override_proxy_options=()
got_ddash=0
prev_opt=
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

  # Extract optarg from --opt=optarg
  optarg=
  case "$opt" in
    -*=*) optarg="${opt#*=}" ;;  # remove-prefix, shortest-match
  esac

  case "$opt" in
    -h | --help) usage; exit ;;
    --loas-type=*) loas_type="$optarg" ;;
    --loas-type) prev_opt=loas_type ;;
    -v | --verbose) verbose=1 ;;

    --) got_ddash=1; shift; break ;;

    # Forward unknown options to rsproxy.
    *) override_proxy_options+=( "$opt" ) ;;
  esac
  shift
done

[[ -z "$prev_opt" ]] || {
  die "Missing --${prev_opt} argument."
}

wrapped_command=("$@")

[[ "$got_ddash" == 1 ]] || {
  die "Missing -- before the wrapped command."
}
[[ "${#wrapped_command[@]}" -ge 1 ]] || {
  die "The wrapped command must not be empty."
}

rsproxy_options=()

# rsproxy configuration:
#
### 'fx build'
# Select config based on LOAS type.
# FX_BUILD_LOAS_TYPE is set by 'fx build' to either "restricted" or
# "unrestricted", and influences authentication method.
# Infra builds don't set this, but instead pass environment variables
# that will override the cfg values.
loas_type="${FX_BUILD_LOAS_TYPE:-"$loas_type"}"
[[ "$loas_type" != "auto" ]] || {
  # Detect "restricted" or "unrestricted"
  loas_type="$("$check_loas_script" | tail -n 1)" || {
    die "Unable to infer LOAS certificate type"
  }
}
debug_msg "using LOAS type: $loas_type"
case "$loas_type" in
  unrestricted)
    readonly CFG="$SCRIPT_DIR/fuchsia-resultstore-gcertauth.cfg"
    rsproxy_options+=(
      --cfg "$CFG"
      --credentials_helper "${credshelper}"
    )
    ;;
  restricted)
    readonly CFG="$SCRIPT_DIR/fuchsia-resultstore.cfg"
    rsproxy_options+=(
      --cfg "$CFG"
    )
    ;;
  skip) : ;;

  *)
    echo "Error: unhandled LOAS type: $loas_type"
    exit 1
    ;;
esac

### infra builds
# Infra builds do not use .cfg files from the source tree;
# they set various RS_* environment variables to override
# the corresponding flags, e.g.:
#   * RS_rs_service
#   * RS_rs_instance
#   * RS_cas_service
#   * RS_cas_instance

# When rs_service points to a unix socket, TLS assumes a server name of
# "localhost", for which certs are invalid.  Fix this by using the
# real name of the service.  Same for cas_service.
# TODO: pass these from recipes as RS_* environment variables.
case "${RS_rs_service:-NOT_SET}" in
  unix://*)
    rsproxy_options+=( --rs_tls_server_name="resultstore.googleapis.com")
    ;;
esac
case "${RS_cas_service:-NOT_SET}" in
  unix://*)
    rsproxy_options+=( --cas_tls_server_name="remotebuildexecution.googleapis.com")
    ;;
esac

# Scan wrapped command arguments for important options.
# Note: this loops scans *all* arguments, which could potentially
# include other intermediate wrappers and non-ninja programs.
# TODO: scan more intelligently, based on -- separators.
subbuild_dir=
action_metrics=
dirty_sources=
chrome_trace=
prev_opt=""
for opt in "${wrapped_command[@]}"
do
  # handle --option arg
  if [[ -n "$prev_opt" ]]
  then
    eval "$prev_opt"=\$opt
    prev_opt=
    continue
  fi

  # Extract optarg from --opt=optarg
  optarg=
  case "$opt" in
    -*=*) optarg="${opt#*=}" ;;  # remove-prefix, shortest-match
  esac

  case "$opt" in
    # ninja options
    -C) prev_opt=subbuild_dir ;;

    --action_metrics_output=*) action_metrics="$optarg" ;;
    --action_metrics_output) prev_opt=action_metrics ;;

    --chrome_trace=*) chrome_trace="$optarg" ;;
    --chrome_trace) prev_opt=chrome_trace ;;

    --dirty_sources_list=*) dirty_sources="$optarg" ;;
    --dirty_sources_list) prev_opt=dirty_sources ;;
  esac
done

# Upload additional invocation artifacts, such as ninja outputs.
# Ninja output paths are relative to $subbuild_dir.
readonly ninja_outputs=(
  "$action_metrics"
  "$chrome_trace"
  "$dirty_sources"
)
for f in "${ninja_outputs[@]}"
do
  [[ -z "$f" ]] || {
    rsproxy_options+=( --post_build_uploads="$subbuild_dir/$f" )
  }
done

wrap_env=()
wrap_options=(
  --rsproxy "$rsproxy"
)

# Handle log dir.
if [[ "${FX_BUILD_LOGDIR:-NOT_SET}" != "NOT_SET" ]]
then
  [[ -n "$subbuild_dir" ]] || {
    die "Expected a ninja -C subdir, but found none."
  }
  wrap_options+=( --log-dir "$FX_BUILD_LOGDIR/rsproxy_logs/$subbuild_dir"  )
elif [[ "${RS_log_dir:-NOT_SET}" != "NOT_SET" ]]
then
  # Infra builds set this to a non-unique path, make it unique
  # using the basename of the sub-build dir.
  [[ -n "$subbuild_dir" ]] || {
    die "Expected a ninja -C subdir, but found none."
  }
  readonly subbuild_base="${subbuild_dir##*/}"  # basename
  # Override the environment variable, which take precedence over the flag.
  # This effectively preserves subdirectory structure of invocations.
  wrap_env+=( RS_log_dir="$RS_log_dir/$subbuild_base" )
fi
# Otherwise, if FX_BUILD_LOGDIR isn't set, this is probably being invoked
# outside of 'fx build', so just fallback to using some temp dir.

[[ "${GCE_METADATA_HOST:-NOT_SET}" == "NOT_SET" ]] || {
  # Workaround: avoid DNS lookup of "localhost"
  wrap_env+=( GCE_METADATA_HOST="${GCE_METADATA_HOST/localhost/127.0.0.1}" )
}


# Ensure that the prebuilt python3 is in the PATH (needed in infra environment).
# rsproxy-wrap.sh uses python3 as an alternative means for mkfifo and sleep.
readonly py3_bindir="${PREBUILT_PYTHON3%/*}"  # dirname
export PATH="$py3_bindir:$PATH"

full_cmd=(
  env
  "${wrap_env[@]}"
  "$proxy_wrap"
  "${wrap_options[@]}"
  --rsproxy_options
  "${rsproxy_options[@]}"
  "${override_proxy_options[@]}"
  --
  "${wrapped_command[@]}"
)

[[ "$verbose" == 0 ]] || {
  echo "[$SCRIPT_NAME] ---- env start ----"
  export SH_WRAPPER_TEST_DEBUG=1  # extra verbosity in rsproxy-wrap.sh
  env
  echo "[$SCRIPT_NAME] ---- env end ----"
}

debug_msg "full command: ${full_cmd[*]}"
exec "${full_cmd[@]}"
