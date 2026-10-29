#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Checks how out of date Planter is before a new migration is started.
#
# Evaluates two components separately against PLANTER_MAX_STALE_HOURS:
# 1. The binary: if `planter version` reports that a newer binary is available
#    (planter changed after the binary was built) or the installed binary is a
#    legacy pre-stamp build, measures how old the binary's build baseline is
#    (using the CitC sync time only when built from a CitC workspace whose sync
#    precedes the build, and the build timestamp otherwise). If no newer binary
#    exists, binary staleness is 0.
# 2. The shared machinery (the prompts, checks and lessons in
#    //build/beads/planter of the shared machinery checkout, by default
#    ~/.planter/beads): the age of the oldest commit on origin/main that
#    changed //build/beads/planter and is not in the checkout yet. Because
#    `planter run` and `planter fleet` auto-sync a clean fast-forwardable
#    machinery checkout on startup, shared machinery only blocks new migrations
#    when it is behind origin/main by more than PLANTER_MAX_STALE_HOURS and has
#    uncommitted edits or local commits that may block automatic sync.
#
# Prints every warning Planter gives (lines starting with the warning sign that
# `planter version` prints, plus any actionable machinery staleness warning) so
# that the caller can relay them to the user, and a summary of both components.
#
# Exit status:
#   0  Planter is fresh enough to start new migrations.
#   2  The Planter binary (with a newer binary available) or non-fast-forward
#      shared machinery is out of date by more than PLANTER_MAX_STALE_HOURS.
#      Do not start new migrations; amending existing migrations is fine.
#   1  The check could not run (e.g. Planter is not installed).

set -euo pipefail

usage() {
  cat <<EOF
Usage: $(basename "$0") [--no-fetch]

Options:
  --no-fetch    Do not fetch origin/main into the shared machinery checkout
                before comparing it with origin/main.
  -h, --help    Show this help.

Environment:
  PLANTER_MAX_STALE_HOURS  How many hours Planter may be out of date before
                           new migrations are refused (default: 24).
  PLANTER_SHARED_DIR       Shared machinery checkout
                           (default: \$HOME/.planter/beads).
  FUCHSIA_DIR              Fuchsia checkout to run \`planter version\` in
                           (default: the current directory).
EOF
}

die() { echo "error: $*" >&2; exit 1; }

FETCH=1
while (( $# > 0 )); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --no-fetch) FETCH="" ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done

MAX_STALE_HOURS="${PLANTER_MAX_STALE_HOURS:-24}"
[[ "$MAX_STALE_HOURS" =~ ^[0-9]+$ ]] \
  || die "PLANTER_MAX_STALE_HOURS must be a whole number of hours, got '$MAX_STALE_HOURS'"
MAX_STALE_SECS=$(( MAX_STALE_HOURS * 3600 ))

PLANTER="$(command -v planter || echo "$HOME/.local/bin/planter")"
[[ -x "$PLANTER" ]] || die "planter not found (install it with publish_prebuilt.sh)"

SHARED_DIR="${PLANTER_SHARED_DIR:-$HOME/.planter/beads}"
NOW="$(date +%s)"

# Renders a number of seconds as e.g. "3d 4h" or "5h".
age_string() {
  local secs="$1"
  if (( secs >= 86400 )); then
    printf '%dd %dh' $(( secs / 86400 )) $(( secs % 86400 / 3600 ))
  else
    printf '%dh' $(( secs / 3600 ))
  fi
}

# --- Planter binary ---------------------------------------------------------

# Run from the Fuchsia checkout, so that Planter also warns if the checkout is
# out of date.
if ! version_out="$(cd "${FUCHSIA_DIR:-.}" && "$PLANTER" version 2>&1)"; then
  printf '%s\n' "$version_out" >&2
  die "'planter version' failed"
fi

# Planter's warnings: a line starting with the warning sign, followed by the
# indented command that fixes it.
warnings="$(printf '%s\n' "$version_out" | awk '
  /^⚠️/ { print; in_warning = 1; next }
  in_warning && /^[[:space:]]/ { print; next }
  { in_warning = 0 }')"

binary_stale_secs=0
binary_is_stale=""
newer_binary=""
binary_note="up to date"
first_line="$(head -n 1 <<<"$version_out")"

if grep -q 'This planter binary is out of date' <<<"$version_out"; then
  newer_binary=1
  # First line: "planter built <date> <time> <zone>[, synced <date> <time>
  # <zone> at cl/<N>] (machinery: ...)".
  built_ts=""
  synced_ts=""
  if [[ "$first_line" =~ built\ ([0-9]{4}-[0-9]{2}-[0-9]{2}\ [0-9]{2}:[0-9]{2}\ [A-Za-z0-9+-]+) ]]; then
    built_ts="${BASH_REMATCH[1]}"
  fi
  if [[ "$first_line" =~ synced\ ([0-9]{4}-[0-9]{2}-[0-9]{2}\ [0-9]{2}:[0-9]{2}\ [A-Za-z0-9+-]+) ]]; then
    synced_ts="${BASH_REMATCH[1]}"
  fi

  built_epoch=""
  synced_epoch=""
  if [[ -n "$built_ts" ]]; then
    built_epoch="$(date -d "$built_ts" +%s 2>/dev/null || true)"
  fi
  if [[ -n "$synced_ts" ]]; then
    synced_epoch="$(date -d "$synced_ts" +%s 2>/dev/null || true)"
  fi

  # Prefer `synced` only when the binary was built in a CitC workspace (which
  # has .citc/srcfs_workspace) and that workspace was not re-synced after the
  # binary was built. Prebuilts built from a read-only head snapshot have no
  # .citc/srcfs_workspace, so Planter's `synced` falls back to the previous
  # Planter CL submit time rather than when the binary was built.
  workspace_dir=""
  if [[ "$version_out" =~ [[:space:]]cd\ (/[^[:space:]\&]+)\ \&\&\ hg\ sync ]]; then
    workspace_dir="${BASH_REMATCH[1]}"
  fi
  is_citc=""
  if [[ -n "$workspace_dir" && ( -e "$workspace_dir/.citc/srcfs_workspace" || -e "$workspace_dir/../.citc/srcfs_workspace" ) ]]; then
    is_citc=1
  fi

  since=""
  since_epoch=""
  if [[ -n "$is_citc" && -n "$synced_epoch" && ( -z "$built_epoch" || "$synced_epoch" -le "$built_epoch" ) ]]; then
    since="$synced_ts"
    since_epoch="$synced_epoch"
  elif [[ -n "$built_epoch" ]]; then
    since="$built_ts"
    since_epoch="$built_epoch"
  elif [[ -n "$synced_epoch" ]]; then
    since="$synced_ts"
    since_epoch="$synced_epoch"
  fi

  if [[ -n "$since_epoch" ]]; then
    binary_stale_secs=$(( NOW - since_epoch ))
    (( binary_stale_secs >= 0 )) || binary_stale_secs=0
    if (( binary_stale_secs > MAX_STALE_SECS )); then
      binary_is_stale=1
    fi
    binary_note="out of date; built as of $since, $(age_string "$binary_stale_secs") ago"
  else
    binary_is_stale=1
    bin_mtime="$(stat -L -c %Y "$PLANTER" 2>/dev/null || true)"
    if [[ -n "$bin_mtime" ]] && (( NOW - bin_mtime > MAX_STALE_SECS )); then
      binary_stale_secs=$(( NOW - bin_mtime ))
    fi
    binary_note="out of date; could not parse build timestamp"
  fi
elif [[ "$first_line" =~ ^planter\ unstamped-dev(\ |$) ]]; then
  binary_note="unstamped development build; cannot tell whether it is up to date"
elif [[ ! "$first_line" =~ ^planter\ built\  ]]; then
  # Pre-nudge legacy binaries (such as `planter 0.2.0 (...)`) never emit
  # "This planter binary is out of date". Treat them as out of date.
  newer_binary=1
  binary_is_stale=1
  legacy_ver="${first_line%% (*}"
  bin_mtime="$(stat -L -c %Y "$PLANTER" 2>/dev/null || true)"
  if [[ -n "$bin_mtime" ]] && (( NOW - bin_mtime > MAX_STALE_SECS )); then
    binary_stale_secs=$(( NOW - bin_mtime ))
    binary_note="out of date; legacy binary ($legacy_ver) built $(age_string "$binary_stale_secs") ago"
  else
    binary_note="out of date; legacy binary ($legacy_ver) without build freshness stamp"
  fi
  warnings+="${warnings:+$'\n'}⚠️  This planter binary ($legacy_ver) predates build freshness stamps and is out of date. Rebuild Planter from head (e.g. with the Planter check script or publish_prebuilt.sh)."
fi

# --- Shared machinery -------------------------------------------------------

machinery_stale_secs=0
machinery_blocked=""
machinery_is_stale=""
machinery_note="up to date with origin/main"
if ! git -C "$SHARED_DIR" rev-parse --git-dir >/dev/null 2>&1; then
  machinery_note="no shared machinery checkout at $SHARED_DIR (run 'planter init-project')"
else
  if [[ -n "$FETCH" ]] && ! git -C "$SHARED_DIR" fetch -q origin main 2>/dev/null; then
    warnings+="${warnings:+$'\n'}⚠️  Could not fetch origin/main into the shared machinery checkout $SHARED_DIR; comparing it with the last fetched origin/main."
  fi
  missing_times="$(git -C "$SHARED_DIR" log --reverse --format=%ct \
    HEAD..origin/main -- build/beads/planter 2>/dev/null || true)"
  oldest_missing="${missing_times%%$'\n'*}"
  if [[ -n "$oldest_missing" ]]; then
    missing_count="$(grep -c . <<<"$missing_times")"
    machinery_stale_secs=$(( NOW - oldest_missing ))
    (( machinery_stale_secs >= 0 )) || machinery_stale_secs=0

    machinery_dirty="$(git -C "$SHARED_DIR" status --porcelain -- build/beads/planter 2>/dev/null || true)"
    machinery_ff=1
    git -C "$SHARED_DIR" merge-base --is-ancestor HEAD origin/main 2>/dev/null || machinery_ff=""

    if [[ -z "$machinery_dirty" && -n "$machinery_ff" ]]; then
      machinery_note="behind origin/main by $missing_count commit(s) to //build/beads/planter (oldest $(age_string "$machinery_stale_secs") ago); clean, will auto-sync on run"
    else
      machinery_blocked=1
      if (( machinery_stale_secs > MAX_STALE_SECS )); then
        machinery_is_stale=1
      fi
      machinery_note="behind origin/main by $missing_count commit(s) to //build/beads/planter (oldest $(age_string "$machinery_stale_secs") ago), with local changes that may block auto-sync"
      warnings+="${warnings:+$'\n'}⚠️  Shared machinery at $SHARED_DIR is behind origin/main by $missing_count commit(s) to //build/beads/planter (oldest $(age_string "$machinery_stale_secs") ago) and has local commits or uncommitted edits. Upload or sync them before starting new migrations:"$'\n'"    planter upload-machinery"
    fi
  fi
fi

# --- Summary ----------------------------------------------------------------

effective_machinery_secs=0
if [[ -n "$machinery_blocked" ]]; then
  effective_machinery_secs="$machinery_stale_secs"
fi
stale_secs=$(( binary_stale_secs > effective_machinery_secs ? binary_stale_secs : effective_machinery_secs ))

echo "Planter binary: $binary_note"
echo "Planter shared machinery: $machinery_note"
if [[ -n "$warnings" ]]; then
  echo
  echo "Planter warnings (relay these to the user verbatim):"
  printf '%s\n' "$warnings"
fi
echo
if [[ -n "$binary_is_stale" || -n "$machinery_is_stale" ]]; then
  stale_reasons=""
  if [[ -n "$binary_is_stale" ]]; then
    stale_reasons="a newer Planter binary is available"
  fi
  if [[ -n "$machinery_is_stale" ]]; then
    stale_reasons+="${stale_reasons:+ and }shared machinery has local changes blocking clean sync with origin/main"
  fi
  if (( stale_secs > MAX_STALE_SECS )); then
    stale_age="$(age_string "$stale_secs")"
  else
    stale_age="an unknown duration"
  fi
  echo "STALE: Planter is out of date by ${stale_age}, more than" \
       "the ${MAX_STALE_HOURS}h limit (${stale_reasons})." \
       "Do not start new migrations until Planter and its shared machinery are" \
       "updated. Amending existing migrations is fine."
  exit 2
fi
echo "FRESH: Planter is out of date by at most $(age_string "$stale_secs")" \
     "(limit ${MAX_STALE_HOURS}h)."
