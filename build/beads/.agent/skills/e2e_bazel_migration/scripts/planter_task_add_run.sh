#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Creates the Planter task that migrates the specified directory to
# Bazel with a commit message that follows
# //build/beads/references/migration/commit_message/guidelines.md.
# Then, execute the Planter task in two steps:
# 1. `planter run` (no --gerrit): Planter migrates the directory, runs the
#    checks and the review panel, and stops in UPLOADING.
# 2. `planter run --gerrit`: Planter commits and uploads the CL to Gerrit,
#    and stops in MONITORING_REVIEW.
#
# Planter turns:
# --title into the commit subject,
# --desc into the commit body; footer lines (e.g. `Bug:`,
# `Bazel-Migration-Target:`) in the last paragraph of --desc become footers.
# Planter itself adds the `Test:` footers, TAG: and Change-Id.

set -euo pipefail

usage() {
  cat <<EOF
Usage: $(basename "$0") --target-dir=<DIR> --title=<TITLE> [--desc=<DESC>]
         [--task-id=<ID>] [--add-only]

Required:
  --target-dir=<DIR>   Directory to migrate, relative to \$FUCHSIA_DIR
                       (e.g. src/connectivity/bluetooth/lib/bt-obex/objects),
                       or a comma-separated list of directories to migrate
                       together in one CL (e.g. src/lib/foo,src/lib/bar).
  --title=<TITLE>      Commit subject, at most 65 chars. Guideline form:
                       "[bazel_migration][<area>] <reference>", without
                       "Migrate".

Optional:
  --desc=<DESC>        Commit body. Footer lines in its last paragraph
                       (e.g. Bug:, Bazel-Migration-Target:) become
                       footers. Planter adds Test:, TAG: and Change-Id.
  --task-id=<ID>       Planter task ID. Defaults to all components of the
                       first --target-dir directory joined by "_" (e.g.
                       src_connectivity_bluetooth_lib_bt-obex_objects).
  --add-only           Only queue the Planter task; do not run or upload it.
                       Used to queue several tasks for \`planter fleet\`.
  -h, --help           Show this help.

The task is added with \`planter add-task --check-gerrit\`, so it fails if an
open Gerrit CL already claims one of the target directories.

Environment:
  FUCHSIA_DIR          Fuchsia checkout (default: \$HOME/fuchsia).
  DRY_RUN              If set, print the planter command instead of running it.
EOF
}

die() { echo "error: $*" >&2; exit 1; }

TARGET_DIR=""
TITLE=""
DESC=""
TASK_ID=""
ADD_ONLY=""

# Accepts both "--flag=value" and "--flag value".
while (( $# > 0 )); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --add-only) ADD_ONLY=1 ;;
    --target-dir=*) TARGET_DIR="${1#*=}" ;;
    --title=*) TITLE="${1#*=}" ;;
    --desc=*) DESC="${1#*=}" ;;
    --task-id=*) TASK_ID="${1#*=}" ;;
    --target-dir|--title|--desc|--task-id)
      (( $# >= 2 )) || die "$1 requires a value"
      case "$1" in
        --target-dir) TARGET_DIR="$2" ;;
        --title) TITLE="$2" ;;
        --desc) DESC="$2" ;;
        --task-id) TASK_ID="$2" ;;
      esac
      shift ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done

[[ -n "$TARGET_DIR" ]] || { usage >&2; die "--target-dir is required"; }
[[ -n "$TITLE" ]] || { usage >&2; die "--title is required"; }

# Normalize each target dir: trim surrounding whitespace and slashes (e.g. a
# leading "//" or a trailing "/"), then drop empty entries and duplicates. An empty entry
# would otherwise resolve to $FUCHSIA_DIR itself, whose BUILD.gn exists.
# `read` stops at the first newline, so turn newlines into separators first.
TARGET_DIR="${TARGET_DIR//$'\n'/,}"
IFS=',' read -r -a raw_dirs <<< "$TARGET_DIR"
TARGET_DIRS=()
for dir in "${raw_dirs[@]}"; do
  dir="${dir#"${dir%%[![:space:]/]*}"}"
  dir="${dir%"${dir##*[![:space:]/]}"}"
  [[ -n "$dir" ]] || continue
  for seen in "${TARGET_DIRS[@]}"; do
    [[ "$seen" != "$dir" ]] || continue 2
  done
  TARGET_DIRS+=("$dir")
done
(( ${#TARGET_DIRS[@]} > 0 )) || die "--target-dir has no directories"
TARGET_DIR="$(IFS=','; echo "${TARGET_DIRS[*]}")"

if [[ -z "$TASK_ID" ]]; then
  # All components of the first target dir joined by "_", as defined in
  # references/bazel_migration_planter.md. Only the first dir is used to keep
  # the ID short when several dirs are migrated together.
  TASK_ID="${TARGET_DIRS[0]//\//_}"
fi

FUCHSIA_DIR="${FUCHSIA_DIR:-$HOME/fuchsia}"
PLANTER="$(command -v planter || echo "$HOME/.local/bin/planter")"

(( ${#TITLE} <= 65 )) || die "subject is ${#TITLE} chars; Planter allows at most 65"
[[ -x "$PLANTER" ]] || die "planter not found (install it with publish_prebuilt.sh)"
[[ -f "$HOME/.planter/beads/build/beads/planter/machinery/prompts/coder.md" ]] \
  || die "run 'planter init-project' first"
for dir in "${TARGET_DIRS[@]}"; do
  [[ -f "$FUCHSIA_DIR/$dir/BUILD.gn" ]] \
    || die "no BUILD.gn in $FUCHSIA_DIR/$dir"
done

printf 'Commit message (Planter appends Test:, TAG: and Change-Id):\n\n%s\n\n' \
  "$TITLE"
[[ -z "$DESC" ]] || printf '%s\n\n' "$DESC"

# --check-gerrit (the default, passed explicitly): Planter refuses to add the
# task if an open Gerrit CL already claims one of the target directories.
add_cmd=("$PLANTER" add-task --workdir="$FUCHSIA_DIR" --id="$TASK_ID"
         --target-dir="$TARGET_DIR" --title="$TITLE" --check-gerrit)
[[ -z "$DESC" ]] || add_cmd+=(--desc="$DESC")
# Step 1, no --gerrit: Planter migrates the directory, runs the checks and the
# review panel, and stops in UPLOADING without committing or pushing.
run_cmd=("$PLANTER" run --repo-root="$FUCHSIA_DIR" --task="$TASK_ID")
# Step 2, --gerrit: Planter commits, uploads the CL and stops in
# MONITORING_REVIEW.
upload_cmd=("${run_cmd[@]}" --gerrit)
if [[ -n "${DRY_RUN:-}" ]]; then
  echo "DRY_RUN set; would run:"
  printf '%q ' "${add_cmd[@]}"
  echo
  [[ -z "$ADD_ONLY" ]] || exit 0
  printf '%q ' "${run_cmd[@]}"
  echo
  printf '%q ' "${upload_cmd[@]}"
  echo
  exit 0
fi

if [[ -n "$ADD_ONLY" ]]; then
  "${add_cmd[@]}"
  echo "Queued Planter task $TASK_ID; run it with 'planter fleet --task=$TASK_ID'."
  exit 0
fi

# `planter run` works on top of HEAD, so it needs a clean checkout with no
# unrelated local commits on top of origin/main.
if [[ -n "$(git -C "$FUCHSIA_DIR" status --porcelain)" ||
      "$(git -C "$FUCHSIA_DIR" rev-list --count origin/main..HEAD)" != 0 ]]; then
  echo "warning: $FUCHSIA_DIR is dirty or has local commits;" \
       "'planter run' may refuse to start or build on top of them." >&2
fi

# Prints "<status>/<phase>" of the task from `planter status`, or nothing if
# the task is not listed.
task_status() {
  "$PLANTER" status 2>/dev/null | awk -v id="$TASK_ID" '
    $1 == "-" && $2 == id {
      for (i = 3; i <= NF; i++) {
        if ($i ~ /^status=/) { status = substr($i, 8) }
        if ($i ~ /^phase=/) { phase = substr($i, 7) }
      }
      print status "/" phase
      exit
    }' || true
}

"${add_cmd[@]}"
# Run from the checkout: Planter prefers the Fuchsia checkout containing pwd.
(cd "$FUCHSIA_DIR" && "${run_cmd[@]}")

status="$(task_status)"
if [[ -n "$status" && "$status" != */UPLOADING ]]; then
  die "task $TASK_ID is $status after the migration, not ready to upload;" \
      "see 'planter inspect --task=$TASK_ID'"
fi

echo "=== Migration done; uploading the CL to Gerrit ==="
(cd "$FUCHSIA_DIR" && "${upload_cmd[@]}")

cat <<EOF

Planter uploaded the CL ($(task_status)). Next:
  planter status                              # shows the change number
EOF