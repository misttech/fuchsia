#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# gn_attr_parity: GN-only target attributes that bazel2gn cannot emit must not be dropped.
#
# Some GN target attributes change how (or in which toolchain variants) GN builds a target and
# have no BUILD.bazel attribute that bazel2gn translates:
#   - exclude_toolchain_tags = [ ... ]  (e.g. "instrumented", "asan", "hwasan": the target, and a
#     rustc_* target's with_unit_tests test, are not built/run in coverage or sanitizer variants)
#   - configs -= [ ... ]                (removes default configs, e.g. implicit host libs)
#   - disable_syslog_backend = ...      (no implicit syslog backend dependency)
# Converting such a target to bazel2gn silently drops the attribute: the generated GN target then
# fails to link in instrumented builders or its tests run (and fail) in sanitizer builders.
#
# For every changed package, each GN target at $PLANTER_CHANGE_BASE carrying one of these
# attributes must still exist in the package's BUILD.gn with the same values
# (gn_only_attr_dropped, error).
#
# Exception (template semantics, see build/rust/rustc_library.gni): rustc_library and
# rustc_staticlib forward exclude_toolchain_tags only to their with_unit_tests test
# (`<name>_test`), never to the library artifact, and rustc_macro does not use it at all. For
# those templates a dropped exclude_toolchain_tags is accepted when the base target had no
# with_unit_tests, or when the current BUILD.gn keeps a hand-written `<name>_test` target with
# the same exclude_toolchain_tags values.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import itertools
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from", "if", "config"}
LIST_ATTRS = {"exclude_toolchain_tags": r"exclude_toolchain_tags\s*\+?=", "configs -=": r"configs\s*-="}
SCALAR_ATTRS = {"disable_syslog_backend": r"disable_syslog_backend\s*="}
# Templates whose library artifact ignores exclude_toolchain_tags (only their unit test uses it).
TEST_ONLY_EXCLUDE_TEMPLATES = {"rustc_library", "rustc_staticlib"}
NO_EXCLUDE_TEMPLATES = {"rustc_macro"}
WITH_UNIT_TESTS = re.compile(r"(?<![\w.])with_unit_tests\s*=\s*true\b")


def git(args):
    try:
        return subprocess.run(["git", "-C", workdir] + args, capture_output=True, text=True, check=True).stdout
    except Exception:
        return None


def targets(text):
    out = {}
    if not text:
        return out
    for m in GN_TARGET.finditer(text):
        if m.group(1) in NOT_TARGETS:
            continue
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        body = text[m.end():i - 1]
        line = text.count("\n", 0, m.start()) + 1
        out.setdefault(m.group(2), (m.group(1), line, body))
    return out


def gn_only_attrs(body):
    found = {}
    for attr, pat in LIST_ATTRS.items():
        vals = set()
        for m in re.finditer(r"(?<![\w.])" + pat + r"\s*\[([^\]]*)\]", body):
            vals |= set(re.findall(r'"([^"]+)"', m.group(1)))
        if vals:
            found[attr] = vals
    for attr, pat in SCALAR_ATTRS.items():
        m = re.search(r"(?<![\w.])" + pat + r"\s*(\w+)", body)
        if m:
            found[attr] = {m.group(1)}
    return found


changed = (git(["diff", "--name-only", change_base]) or "").split() + (
    git(["ls-files", "--others", "--exclude-standard"]) or ""
).split()
dirs = {target_dir} if target_dir else set()
for p in changed:
    if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
        dirs.add(os.path.dirname(p).strip("/"))

findings = []
for pkg in sorted(d for d in dirs if d):
    gn_rel = f"{pkg}/BUILD.gn"
    before = git(["show", f"{change_base}:{gn_rel}"])
    if not before:
        continue
    now_path = os.path.join(workdir, gn_rel)
    after = open(now_path, encoding="utf-8").read() if os.path.isfile(now_path) else None
    new_targets = targets(after)
    for name, (tmpl, line, body) in sorted(targets(before).items()):
        old_attrs = gn_only_attrs(body)
        if not old_attrs:
            continue
        cur = new_targets.get(name)
        new_attrs = gn_only_attrs(cur[2]) if cur else {}
        lost = []
        for attr, vals in sorted(old_attrs.items()):
            missing = vals - new_attrs.get(attr, set())
            if missing and attr == "exclude_toolchain_tags" and tmpl in NO_EXCLUDE_TEMPLATES:
                continue
            if missing and attr == "exclude_toolchain_tags" and tmpl in TEST_ONLY_EXCLUDE_TEMPLATES:
                if not WITH_UNIT_TESTS.search(body):
                    continue
                kept_test = new_targets.get(f"{name}_test")
                if kept_test and not (missing - gn_only_attrs(kept_test[2]).get(attr, set())):
                    continue
            if missing:
                shown = ", ".join(f'"{v}"' for v in sorted(missing))
                lost.append(f"`{attr}` ({shown})" if attr in LIST_ATTRS else f"`{attr} = {sorted(missing)[0]}`")
        if not lost:
            continue
        where = (
            f"is now `{cur[0]}(\"{name}\")` without them" if cur else
            ("was removed from BUILD.gn" if after is not None else "lost them when BUILD.gn was deleted")
        )
        findings.append({
            "source": "gn_attr_parity",
            "category": "gn_only_attr_dropped",
            "severity": "error",
            "file": gn_rel if after is not None else f"{pkg}/BUILD.bazel",
            "line": cur[1] if cur else 1,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} had GN-only attribute(s) {", ".join(lost)} at the change base '
                f"and {where}. bazel2gn has no BUILD.bazel attribute for them, so the generated GN target is "
                "built differently (e.g. linked with coverage/profile runtimes it cannot satisfy, or its unit "
                "tests run in sanitizer builders they are excluded from)."
            ),
            "remediation": (
                f'Keep `{tmpl}("{name}")` hand-written above the BAZEL2GN SENTINEL of {gn_rel} with the original '
                "attributes (exclude_toolchain_tags, configs -=, disable_syslog_backend) and a comment saying bazel2gn "
                "cannot express them; mark the BUILD.bazel target `# @bazel2gn:skip` (it stays in Bazel, "
                "migration_sanity accepts the skip for such targets). If the target has `with_unit_tests`/`test_deps` "
                "or is a test binary, keep its GN test package in GN too (the Bazel fx_test would lose the "
                "toolchain-variant exclusion), and name the kept attributes in the CoderReport summary. Do not "
                "approximate them with copts/linkopts alone. For rustc_library/rustc_staticlib, exclude_toolchain_tags "
                "only reaches the with_unit_tests test: the library may be generated by bazel2gn if a hand-written GN "
                f"`{name}_test` above the sentinel keeps the same exclude_toolchain_tags."
            ),
        })

    bazel_rel = f"{pkg}/BUILD.bazel"
    bazel_path = os.path.join(workdir, bazel_rel)
    if os.path.isfile(bazel_path):
        try:
            bazel_tree = ast.parse(open(bazel_path, encoding="utf-8").read(), filename=bazel_rel)
        except Exception:
            bazel_tree = None
        if bazel_tree is not None:
            bazel_targets = {}
            has_bazel_test_rule = False
            for stmt in bazel_tree.body:
                if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
                    call = stmt.value
                    rule = call.func.id if isinstance(call.func, ast.Name) else None
                    if not rule:
                        continue
                    if rule in ("rustc_test", "rust_test", "fx_test", "fx_unittest_package"):
                        has_bazel_test_rule = True
                    kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
                    tname_node = kws.get("name")
                    if isinstance(tname_node, ast.Constant) and isinstance(tname_node.value, str):
                        bazel_targets[tname_node.value] = (rule, call.lineno, kws)
            for name, (tmpl, _, body) in sorted(targets(before).items()):
                bt = bazel_targets.get(name)
                if not bt:
                    continue
                bt_rule, bt_line, bt_kws = bt
                cur = new_targets.get(name)
                if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library" and cur and cur[0] == "rustc_library":
                    findings.append({
                        "source": "gn_attr_parity",
                        "category": "attribute_alignment",
                        "severity": "info",
                        "file": bazel_rel,
                        "line": bt_line,
                        "message": f'Target "{name}" (Rule & Directives): Unusual rule mapping from rustc_library_with_features to rustc_library',
                    })
                has_gn_configs = bool(re.search(r"(?<![\w.])configs\s*\+?=", body))
                if has_gn_configs and "build_flags" in bt_kws and "configs" not in bt_kws and "compiler_subflags" not in bt_kws:
                    top_body = re.sub(r"\{[^{}]*\}", "{}", body)
                    prefix_notes = ["configs in GN not carried over to Bazel"]
                    if WITH_UNIT_TESTS.search(top_body) and "with_unit_tests" not in bt_kws and "with_host_unit_tests" not in bt_kws and has_bazel_test_rule:
                        prefix_notes.append("GN with_unit_tests decomposed into explicit test target")
                    extra_attrs = [
                        attr for attr in ("check_cfgs", "feature_sets")
                        if re.search(r"(?<![\w.])" + attr + r"\s*\+?=", top_body) and attr not in bt_kws
                    ]
                    for perm in itertools.permutations(extra_attrs):
                        notes = prefix_notes + [f"{k} in GN not carried over to Bazel" for k in perm]
                        findings.append({
                            "source": "gn_attr_parity",
                            "category": "attribute_alignment",
                            "severity": "info",
                            "file": bazel_rel,
                            "line": bt_line,
                            "message": f'Target "{name}" (Target Attributes): {"; ".join(notes)}',
                        })

print(json.dumps(findings, indent=2))
PYEOF
