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
#   - a `.cml` manifest relocated byte-identically from an out-of-package path (such as an
#     unmigrated ancestor/sibling `meta/` directory referenced by the pre-migration `BUILD.gn`)
#     into a migrated Bazel package whose `BUILD.bazel` references it directly (including
#     removal of the pre-migration `.cml` path once no external GN targets use it)
#   - existing source files (`.rs`, `.cc`, `.cpp`, `.c`, `.h`, `.hpp`) inside a migrated
#     package whose `BUILD.gn` at `$PLANTER_CHANGE_BASE` was disconnected from the configured
#     GN build graph (`<build dir>/ninja_outputs.json` has no targets for that package), when
#     modified solely to repair pre-existing compile bitrot (hard syntax, type, or removed-API
#     errors, with no lint-suppression annotations)
#   - an existing `<pkg>/README.md` in a migrated package directory when every modified
#     (`+`/`-`) line only updates a target label reference (`//<pkg>...`) to match the
#     migrated BUILD.gn / BUILD.bazel targets; other README.md or OWNERS edits are not allowed.

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
# commit, "HEAD" when HEAD is unrelated upstream history or a stacked parent commit) plus untracked files.
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
raw_tdirs = re.split(
    r"[\s,]+",
    (os.environ.get("PLANTER_TARGET_DIRS", "") or target_dir).strip(),
)
check_dirs = []
for d in raw_tdirs:
    nd = d.strip().strip("/")
    if nd:
        check_dirs.append(nd)
        if nd.startswith("build/secondary/"):
            check_dirs.append(nd[len("build/secondary/"):])
head_subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
if head_subj.startswith("[bazel_migration]"):
    head_sha = (git_lines(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]) or [""])[0]
    parent_sha = (git_lines(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"]) or [""])[0]
    base_sha = (
        git_lines(["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"]) or [""]
    )[0]
    head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"]) if parent_sha else []
    head_pkgs = {
        os.path.dirname(f)
        for f in head_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn", "BUILD")
    }
    sdir = (
        head_subj.split(" //", 1)[1].split(":", 1)[0].strip().rstrip(".").strip("/")
        if " //" in head_subj
        else ""
    )
    subj_matches = bool(sdir) and any(
        (td == sdir[4:] or td.endswith("/" + sdir[4:]))
        if sdir.startswith(".../")
        else (sdir == td)
        for td in check_dirs
    )
    is_task_head = not check_dirs or (
        not (sdir and sdir in head_pkgs and not subj_matches)
        and (
            (bool(head_pkgs & set(check_dirs)) or subj_matches)
            if head_pkgs
            else (
                any(f == td or f.startswith(td + "/") for td in check_dirs for f in head_files)
                or subj_matches
            )
        )
    )
    if parent_sha and is_task_head and (change_base == "HEAD" or base_sha == head_sha):
        change_base = "HEAD~1"
    elif parent_sha and is_task_head and base_sha and base_sha != parent_sha:
        rc = subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", base_sha, parent_sha],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode
        if rc == 0:
            change_base = "HEAD~1"
# Pass --no-renames so moved files deterministically appear in both added and deleted sets.
changed.update(git_lines(["diff", "--no-renames", "--name-only", change_base]))
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


# Newly added and deleted files (not in-place modifications of existing ones).
added = set(
    git_lines(["diff", "--no-renames", "--name-only", "--diff-filter=A", change_base])
)
added.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
deleted = set(
    git_lines(["diff", "--no-renames", "--name-only", "--diff-filter=D", change_base])
)
deleted.update(p for p in changed if not os.path.exists(os.path.join(workdir, p)))


def git_blob_bytes(ref: str, rel_path: str):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return None


def find_nearest_bazel_package(path: str):
    pkg = os.path.dirname(path)
    for _ in range(6):
        bazel = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bazel):
            try:
                with open(bazel, encoding="utf-8") as f:
                    return pkg, f.read()
            except OSError:
                return None, ""
        if not pkg:
            break
        pkg = os.path.dirname(pkg)
    return None, ""


# Identify .cml manifests relocated byte-identically from an out-of-package directory into
# their owning Bazel package (supporting both single-commit git mv and staged subpackage
# copies before outside_changes deletes the old path).
def find_relocated_component_manifests():
    new_to_old = {}
    old_to_new = {}
    refs = [change_base, "HEAD~1"] if change_base == "HEAD" else [change_base]
    for new_path in sorted(added):
        if not new_path.endswith(".cml"):
            continue
        full_new = os.path.join(workdir, new_path)
        if not os.path.isfile(full_new):
            continue
        pkg, bazel_text = find_nearest_bazel_package(new_path)
        if not pkg:
            continue
        rel_in_pkg = os.path.relpath(new_path, pkg)
        if not re.search(r'"(?::)?' + re.escape(rel_in_pkg) + r'"', bazel_text):
            continue
        try:
            with open(full_new, "rb") as f:
                new_bytes = f.read()
        except OSError:
            continue
        candidates = []
        for ref in refs:
            base_gn_raw = git_blob_bytes(ref, f"{pkg}/BUILD.gn")
            if base_gn_raw is not None:
                code_gn = re.sub(
                    r"#[^\n]*", "", base_gn_raw.decode("utf-8", errors="replace")
                )
                for raw_cml in re.findall(r'"([^"\n]+\.cml)"', code_gn):
                    cand_old = (
                        raw_cml[2:].lstrip("/")
                        if raw_cml.startswith("//")
                        else os.path.normpath(os.path.join(pkg, raw_cml)).replace("\\", "/")
                    )
                    if (
                        cand_old
                        and not cand_old.startswith("../")
                        and cand_old != new_path
                        and not cand_old.startswith(pkg + "/")
                    ):
                        candidates.append((ref, cand_old))
            for d_path in sorted(deleted):
                if (
                    d_path.endswith(".cml")
                    and d_path != new_path
                    and not d_path.startswith(pkg + "/")
                ):
                    candidates.append((ref, d_path))
        seen_cands = set()
        for ref, cand_old in candidates:
            if (ref, cand_old) in seen_cands:
                continue
            seen_cands.add((ref, cand_old))
            old_bytes = git_blob_bytes(ref, cand_old)
            if old_bytes is not None and old_bytes == new_bytes:
                new_to_old[new_path] = (cand_old, pkg)
                old_to_new.setdefault(cand_old, []).append((new_path, pkg))
                break
    return new_to_old, old_to_new


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
relocated_new_to_old, relocated_old_to_new = find_relocated_component_manifests()
bitrot_fixed_dirs = set()


# Check whether an existing README.md inside a migrated package directory only updates
# target label references (`//<pkg>...`) to match the migrated BUILD.gn/BUILD.bazel targets.
def is_package_readme_label_update(path: str) -> bool:
    if path in added or os.path.basename(path) != "README.md":
        return False
    pkg = os.path.dirname(path).strip("/")
    if not pkg or not os.path.isfile(os.path.join(workdir, pkg, "BUILD.bazel")):
        return False
    if f"{pkg}/BUILD.bazel" not in changed and f"{pkg}/BUILD.gn" not in changed:
        return False
    diff_lines = git_lines(["diff", "-U0", change_base, "--", path])
    mod_lines = [
        l[1:].strip()
        for l in diff_lines
        if (l.startswith("+") and not l.startswith("+++"))
        or (l.startswith("-") and not l.startswith("---"))
    ]
    return bool(mod_lines) and all(f"//{pkg}" in l for l in mod_lines)
findings = []

# Emit findings for changed non-build files, relocated manifests, and unbuilt GN package notes.
for path in sorted(changed):
    if (
        is_build_file(path)
        or is_new_test_manifest(path)
        or is_package_readme_label_update(path)
    ):
        continue
    if path in relocated_new_to_old:
        old_p, owner_pkg = relocated_new_to_old[path]
        findings.append({
            "source": "build_only_scope",
            "category": "relocated_component_manifest",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": (
                f"Allowed byte-identical component manifest relocation: '{path}' is an "
                f"unchanged copy of '{old_p}' moved into Bazel package '//{owner_pkg}' "
                "so its manifest target owns the file directly."
            ),
        })
        continue
    if path in deleted and path in relocated_old_to_new:
        dest_desc = ", ".join(
            f"'{np}' (//{op})" for np, op in relocated_old_to_new[path]
        )
        findings.append({
            "source": "build_only_scope",
            "category": "relocated_component_manifest",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": (
                f"Allowed removal of relocated component manifest '{path}', moved "
                f"byte-identically to {dest_desc}."
            ),
        })
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
