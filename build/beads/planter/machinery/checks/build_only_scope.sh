#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing that a GN-to-Bazel migration only touches
# build-definition files. Any modification to source files (.rs, .cc, .h, .py,
# .go, .fidl, .cml, .json5, etc.) in the migration change is a scope violation:
# migrations must be pure build-graph refactors with zero behavioral/source diff.
#
# Files inspected: `git diff $PLANTER_CHANGE_BASE` (the task's commit, if HEAD is
# the task's commit, plus uncommitted changes) plus untracked files.
#
# Allowed paths (build-definition surface):
#   - BUILD.gn, BUILD.bazel, BUILD, *.BUILD.bazel, *.BUILD (any directory, including
#     workspace/repo root build files such as build/bazel/toplevel.BUILD.bazel)
#   - *.gni, *.bzl (build registration lists / macros, e.g. verification lists)
#   - MODULE.bazel, *.MODULE.bazel, WORKSPACE*, *.bazelrc
#   - a newly added (not modified) test component manifest `.cml` that an
#     `fx_component_manifest` of the nearest BUILD.bazel uses as `manifest` next to an
#     `fx_test_component` (it replaces the manifest GN's fuchsia_unittest_package generated;
#     Bazel does not generate test manifests, for C++ or Rust tests)
#   - existing source files (`.rs`, `.cc`, `.cpp`, `.c`, `.h`, `.hpp`) inside a migrated
#     package whose `BUILD.gn` at `$PLANTER_CHANGE_BASE` was disconnected from the configured
#     GN build graph (`<build dir>/ninja_outputs.json` has no targets for that package), when
#     modified solely to repair pre-existing compile bitrot (hard syntax, type, or removed-API
#     errors, with no lint-suppression annotations)
#   - OWNERS / README.md are NOT allowed implicitly; they are not needed for migration.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$TARGET_DIRS" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
target_dirs = [
    d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")
]

ALLOWED_BASENAMES = {"BUILD.gn", "BUILD.bazel", "BUILD", "MODULE.bazel"}
ALLOWED_SUFFIXES = (".gni", ".bzl", ".bazelrc", ".BUILD.bazel", ".BUILD", ".MODULE.bazel")
ALLOWED_PREFIX_BASENAMES = ("WORKSPACE",)
SOURCE_SUFFIXES = (".rs", ".cc", ".cpp", ".c", ".h", ".hpp")
NON_BUILD_GN_CALLS = {
    "group",
    "template",
    "declare_args",
    "foreach",
    "forward_variables_from",
    "bazel_test_suite",
    "import",
    "set_defaults",
}
LINT_SUPPRESSION_ADD_RE = re.compile(
    r"#!?\[\s*(?:allow|expect)\s*\(|NOLINT|clippy::|^\s*let\s+_\s*="
)
LINT_ONLY_REMOVE_RE = re.compile(r"\.clone\(\)|^\s*use\s+[\w:{}, *]+;\s*$")


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


changed = set()
# The task's change: everything since PLANTER_CHANGE_BASE ("HEAD~1" when HEAD is the task's own
# commit, "HEAD" when HEAD is unrelated upstream history) plus untracked files.
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
changed.update(git_lines(["diff", "--name-only", change_base]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))


def is_build_file(path: str) -> bool:
    base = os.path.basename(path)
    if base in ALLOWED_BASENAMES:
        return True
    if base.endswith(ALLOWED_SUFFIXES):
        return True
    if base.startswith(ALLOWED_PREFIX_BASENAMES):
        return True
    return False


# Newly added files (not modifications of existing ones).
added = set(git_lines(["diff", "--name-only", "--diff-filter=A", change_base]))
added.update(git_lines(["ls-files", "--others", "--exclude-standard"]))


def is_new_test_manifest(path: str) -> bool:
    """A new `.cml` that an `fx_component_manifest` in the nearest BUILD.bazel uses as `manifest`
    while that BUILD.bazel defines an `fx_test_component`: it replaces the manifest that GN's
    `fuchsia_unittest_package` generated when a (C++ or Rust) test package migrates to `fx_test`."""
    if path not in added or not path.endswith(".cml") or path.endswith(".shard.cml"):
        return False
    if not os.path.isfile(os.path.join(workdir, path)):
        return False
    pkg = os.path.dirname(path)
    for _ in range(4):
        bazel = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bazel):
            try:
                text = open(bazel, encoding="utf-8").read()
            except OSError:
                return False
            rel = os.path.relpath(path, pkg) if pkg else path
            return "fx_test_component(" in text and bool(
                re.search(r'\bmanifest\s*=\s*":?' + re.escape(rel) + r'"', text)
            )
        if not pkg:
            break
        pkg = os.path.dirname(pkg)
    return False


# Identify migrated packages that were disconnected from the configured GN build graph.
def find_unbuilt_gn_packages() -> set:
    candidate_dirs = set(target_dirs)
    for p in changed:
        if os.path.basename(p) == "BUILD.bazel":
            d = os.path.dirname(p).strip("/")
            if d:
                candidate_dirs.add(d)
    if not candidate_dirs:
        return set()
    try:
        with open(os.path.join(workdir, ".fx-build-dir"), encoding="utf-8") as f:
            build_dir = f.read().strip()
        with open(os.path.join(workdir, build_dir, "ninja_outputs.json"), encoding="utf-8") as f:
            ninja_outputs = json.load(f)
    except (OSError, ValueError):
        return set()
    configured_labels = set()
    for key, outs in ninja_outputs.items():
        if isinstance(outs, list) and any(isinstance(o, str) and o.strip() for o in outs):
            configured_labels.add(key.split("(", 1)[0])
    unbuilt = set()
    target_re = re.compile(r'^\s*([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{', re.M)
    for d in sorted(candidate_dirs):
        base_gn = "\n".join(git_lines(["show", f"{change_base}:{d}/BUILD.gn"]))
        if not base_gn:
            continue
        code_gn = re.sub(r"#[^\n]*", "", base_gn)
        gn_targets = [
            f"//{d}:{m.group(2)}"
            for m in target_re.finditer(code_gn)
            if m.group(1) not in NON_BUILD_GN_CALLS
        ]
        if gn_targets and not any(lbl in configured_labels for lbl in gn_targets):
            unbuilt.add(d)
    return unbuilt


# Check whether a modified source file in an unbuilt GN package is a bitrot compile repair.
def check_unbuilt_target_bitrot_fix(path: str, unbuilt_dirs: set):
    if path in added or not path.endswith(SOURCE_SUFFIXES):
        return None
    if not os.path.isfile(os.path.join(workdir, path)):
        return None
    matched_dir = next(
        (d for d in sorted(unbuilt_dirs, key=len, reverse=True) if path.startswith(d + "/")),
        None,
    )
    if not matched_dir:
        return None
    diff_lines = git_lines(["diff", "--unified=0", change_base, "--", path])
    substantive_changes = 0
    for raw in diff_lines:
        if raw.startswith(("+++ ", "--- ", "@@ ")):
            continue
        if raw.startswith("+"):
            body = raw[1:]
            if LINT_SUPPRESSION_ADD_RE.search(body):
                return None
            if body.strip():
                substantive_changes += 1
        elif raw.startswith("-"):
            body = raw[1:]
            if body.strip() and not LINT_ONLY_REMOVE_RE.search(body):
                substantive_changes += 1
    return matched_dir if substantive_changes > 0 else None


unbuilt_dirs = find_unbuilt_gn_packages()
bitrot_fixed_dirs = set()
findings = []

# Emit findings for changed non-build files and unbuilt GN package scope notes.
for path in sorted(changed):
    if is_build_file(path) or is_new_test_manifest(path):
        continue
    bitrot_dir = check_unbuilt_target_bitrot_fix(path, unbuilt_dirs)
    if bitrot_dir is not None:
        bitrot_fixed_dirs.add(bitrot_dir)
        msg_text = (
            f"Allowed pre-existing bitrot compile fix in '{path}': package '//{bitrot_dir}' "
            "had no targets in the configured GN build graph (<build dir>/ninja_outputs.json) "
            f"at {change_base}, and the diff contains no lint-suppression annotations. "
            "Keep each fix strictly minimal (avoid reformatting surrounding expressions when "
            "a 1-line fix such as changing a misplaced inner `///` comment to `//` suffices) "
            "and explain the pre-existing compile errors in the commit message body. "
            "Note: touching a `.rs` file in a package with no affected GN tests causes the "
            "non-blocking experimental FYI builder `fuchsia-coverage` to fail `Step('covargs')` "
            "with `llvm-profdata merge: error: no input files specified`; if CQ polling "
            "triggers remediation solely for that FYI failure, leave the checkout untouched "
            "(preserving any `Code-Review+2` vote) and report `machinery_conflict` in "
            "`disputed_findings`."
        )
        findings.append({
            "source": "build_only_scope",
            "category": "unbuilt_target_bitrot_fix",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": msg_text,
        })
        continue
    msg_text = (
        f"Migration modifies non-build file '{path}'. GN-to-Bazel migrations must not "
        "change source code, tests, manifests, or data files (except minimal compile-fix "
        "edits in packages disconnected from the configured GN build graph)."
    )
    rem_text = (
        "Revert this file to its pre-migration contents (git checkout $PLANTER_CHANGE_BASE "
        "-- <file> or git checkout -- <file>). If the Bazel build surfaces new lint/compile "
        "errors on a target built in GN, fix the BUILD.bazel attributes (lint_config, "
        "rustc_flags, configs, features, deps) instead of editing sources."
    )
    findings.append({
        "source": "build_only_scope",
        "category": "non_build_file_modified",
        "severity": "error",
        "file": path,
        "line": 0,
        "message": msg_text,
        "remediation": rem_text,
    })

for d in sorted(unbuilt_dirs - bitrot_fixed_dirs):
    has_bazel = os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))
    anchor = f"{d}/BUILD.bazel" if has_bazel else f"{d}/BUILD.gn"
    note_text = (
        f"Package '//{d}' had no targets in the configured GN build graph "
        f"(<build dir>/ninja_outputs.json) at {change_base}. If `fx bazel build` fails due "
        "to pre-existing source bitrot (hard syntax, type, or removed-API compile errors in "
        f"'{d}/' that cannot be resolved via BUILD.bazel attributes), minimal compile-fix "
        f"edits to existing source files under '{d}/' (without lint-suppression annotations) "
        "are permitted by build_only_scope."
    )
    findings.append({
        "source": "build_only_scope",
        "category": "unbuilt_gn_target_scope_note",
        "severity": "INFO",
        "file": anchor,
        "line": 0,
        "message": note_text,
    })

print(json.dumps(findings, indent=2))
PYEOF
