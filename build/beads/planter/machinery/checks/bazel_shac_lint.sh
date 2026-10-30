#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Deterministic check mirroring the CQ static-checks shac Bazel lint rules
# (bazel_bzl_require_load_visibility, bazel_disallowed_workspace_root_package_labels,
# bazel_target_visibility, bazel_load_visibility, ...). It runs the checkout's own
# scripts/shac/bazel_lint/main.py for every registered rule on each Bazel file
# (BUILD.bazel, BUILD, *.bzl, *.bazel) the change adds or modifies, so these
# failures surface in seconds even when the full shac run in build_verification
# is skipped. Prints a JSON array of findings on stdout.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"
lint_dir = os.path.join(workdir, "scripts", "shac", "bazel_lint")
main_py = os.path.join(lint_dir, "main.py")

REMEDIATION_BY_RULE = {
    "bazel_bzl_require_load_visibility": (
        "Add a top-level load visibility() call after the load() statements of the .bzl\n"
        "file: visibility(\"private\") when only BUILD files of its own package load it,\n"
        "otherwise the narrowest list of loading packages or subtrees."
    ),
    "bazel_disallowed_workspace_root_package_labels": (
        "Never reference a file in a subdirectory through the root package ('//:<dir>/<file>').\n"
        "Use the label of the package that owns the file ('//<dir>:<file>'), adding a\n"
        "BUILD.bazel with exports_files([...], visibility = [...]) or a filegroup there\n"
        "(in a separate CL below the migration when the directory is outside its scope)."
    ),
    "bazel_target_visibility": (
        "Replace \"//visibility:public\" with the visibility find_rdeps recommends\n"
        "(per_target_recommended_visibility); test entry points need no visibility."
    ),
}
DEFAULT_REMEDIATION = (
    "Fix the reported Bazel lint finding in the file (the same rule fails CQ\n"
    "static-checks); run `fx host-tool shac check --only <rule> <file>` to confirm."
)


# Collect Bazel files added or modified by the change (tracked and untracked).
def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


def is_bazel_file(path):
    base = os.path.basename(path)
    # shac skips vendored Bazel repositories and upstream third-party sources.
    if path.startswith("third_party/bazel_vendor/"):
        return False
    if path.startswith("third_party/") and "/src/" in path:
        return False
    return base in ("BUILD.bazel", "BUILD") or base.endswith(".bzl") or base.endswith(".bazel")


def changed_bazel_files():
    changed = set(git_lines(["diff", "--name-only", "--diff-filter=AMR", change_base]))
    changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
    return sorted(
        p for p in changed if is_bazel_file(p) and os.path.isfile(os.path.join(workdir, p))
    )


# Discover the rule names main.py registers (its --check choices).
def registered_rules():
    sys.path.insert(0, lint_dir)
    try:
        import importlib

        importlib.import_module("checks")
        from framework import REGISTRY

        return sorted(REGISTRY.keys())
    except Exception:
        return []


# Run one rule on the files; returns main.py's JSON findings (empty on tool error).
def run_rule(rule, files):
    try:
        res = subprocess.run(
            [sys.executable, main_py, "--check", rule, "--root", workdir, "--json"] + files,
            cwd=workdir, capture_output=True, text=True, timeout=120,
        )
    except Exception:
        return []
    if res.returncode not in (0, 42):
        return []
    try:
        data = json.loads(res.stdout or "[]")
    except ValueError:
        return []
    return data if isinstance(data, list) else []


findings = []
files = changed_bazel_files()


# main.py rejects a whole invocation when one file has a kind the rule does not
# apply to, so run each rule separately on .bzl files, BUILD files and others.
def file_kind(path):
    base = os.path.basename(path)
    if base.endswith(".bzl"):
        return "bzl"
    return "build" if base in ("BUILD.bazel", "BUILD") else "other"


groups = {}
for p in files:
    groups.setdefault(file_kind(p), []).append(p)

if files and os.path.isfile(main_py):
    for rule in registered_rules():
        rule_findings = []
        for group in groups.values():
            rule_findings.extend(run_rule(rule, group))
        for f in rule_findings:
            level = f.get("level", "error")
            if level not in ("error", "warning"):
                continue
            message = f.get("message", "")
            if f.get("url"):
                message += f" See {f['url']}."
            findings.append({
                "source": "bazel_shac_lint",
                "category": rule,
                "severity": level,
                "file": f.get("filepath", ""),
                "line": f.get("line", 1) or 1,
                "message": f"[shac {rule}] {message}",
                "remediation": REMEDIATION_BY_RULE.get(rule, DEFAULT_REMEDIATION),
            })

print(json.dumps(findings, indent=2))
PYEOF
