#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check that a migration keeps the type of every GN target it
# converts, and in particular the link semantics of C/C++ libraries. A GN
# source_set links all of its objects into every dependent; a static_library
# (and a Bazel cc_library without `alwayslink = True`) only the objects that
# resolve undefined symbols. Switching silently drops static initializers and
# other unreferenced objects (or links objects that were left out before), which
# builds do not catch.
#
# For each directory of the change whose BUILD.gn existed at
# $PLANTER_CHANGE_BASE, every GN target is compared with what replaces it:
# - If the target still exists in BUILD.gn (bazel2gn, or a GN target kept by
#   hand), its GN type must not change. bazel2gn emits a cc_library or
#   fx_cc_library as `static_library()` unless it sets `alwayslink = True`,
#   which makes it a `source_set()`.
#   1. source_set_became_static_library (error): add `alwayslink = True`.
#   2. static_library_became_source_set (error): remove `alwayslink = True`.
#   3. gn_target_type_changed (error): any other type change (e.g. an
#      sdk_source_set, or a GN-only wrapper template replaced by the rules it
#      generates). Dispute it in the CoderReport if the new type is equivalent.
#      Exception: with `build/bazel/rules/testing/fx_test.bzl` in the checkout, a
#      GN test package (or host test) may become a `bazel_test_suite` of the same
#      name exporting its migrated Bazel tests (C++ and Rust alike: tests move to
#      Bazel and bazel2gn does not translate them). A GN host binary or
#      `install_host_tools` target (`rustc_binary`, `go_binary`, `executable`,
#      `install_host_tools`) may likewise become a `bazel_host_tool` (or a
#      forwarding `group` in a `BUILD.gn` that defines a `bazel_host_tool`).
# - If the GN target is gone (BUILD.gn deleted in a full removal, or the target
#   removed) and BUILD.bazel defines a cc_library or fx_cc_library of the same
#   name, its `alwayslink` must match the GN type:
#   4. source_set_without_alwayslink (error): a former source_set lacks
#      `alwayslink = True`.
#   5. static_library_with_alwayslink (error): a former static_library sets
#      `alwayslink = True`.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
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
CC_RULE = re.compile(r'(?<![\w.])(cc_library|fx_cc_library)\s*\(')
BAZEL_NAME = re.compile(r'\bname\s*=\s*"([^"]+)"')
ALWAYSLINK = re.compile(r'\balwayslink\s*=\s*(True|False)\b')
# With fx_test in the checkout, a GN test package migrated to Bazel is replaced by a GN
# bazel_test_suite of the same name that exports the Bazel tests to tests.json.
FX_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl"))
GN_TEST_TEMPLATES = {
    "fuchsia_unittest_package",
    "fuchsia_test_package",
    "fuchsia_test",
    "python_host_test",
    "host_test",
    "rustc_test",
    "go_test",
    "test",
}
GN_HOST_TOOL_TEMPLATES = {
    "rustc_binary",
    "go_binary",
    "executable",
    "install_host_tools",
}


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


def old_text(rel):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir, "show", f"{change_base}:{rel}"], stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return None


def new_text(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def blank_comments(text):
    """Blanks out `#` comments (GN and Starlark) without moving offsets."""
    out, i, n, quote = [], 0, len(text), None
    while i < n:
        c = text[i]
        if quote:
            if c == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in "\"'":
            quote = c
        elif c == "#":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def line_of(code, offset):
    return code.count("\n", 0, offset) + 1


def gn_types(text):
    """Maps each GN target name to the set of templates defining it (several under if/else) and a line."""
    code = blank_comments(text)
    local_tmpls = {}
    for m in re.finditer(r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{', code):
        body = code[m.end() : call_end(code, m.end() - 1) - 1]
        tvars = {"target_name"} | {
            vm.group(1) for vm in re.finditer(r"\b([A-Za-z_]\w*)\s*=\s*target_name\b", body)
        }
        for im in re.finditer(r"\b([A-Za-z_]\w*)\(\s*([A-Za-z_]\w*|\$\{?target_name\}?)\s*\)\s*\{", body):
            if im.group(1) not in NOT_TARGETS and (im.group(2) in tvars or "target_name" in im.group(2)):
                local_tmpls.setdefault(m.group(1), set()).add(im.group(1))
    out = {}
    for m in GN_TARGET.finditer(code):
        tmpl, name = m.group(1), m.group(2)
        if tmpl in NOT_TARGETS:
            continue
        types, line = out.get(name, (set(), line_of(code, m.start())))
        types.update(local_tmpls.get(tmpl, {tmpl}))
        out[name] = (types, line)
    return out


def call_end(code, open_paren):
    """Index just past the `)` closing the `(` at code[open_paren] (comments are already blank)."""
    depth, quote, i = 0, None, open_paren
    while i < len(code):
        c = code[i]
        if quote:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in "\"'":
            quote = c
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return len(code)


def bazel_cc_libraries(text):
    """Maps each cc_library/fx_cc_library name in BUILD.bazel to (rule, alwayslink, line)."""
    code = blank_comments(text)
    out = {}
    for m in CC_RULE.finditer(code):
        body = code[m.end() - 1 : call_end(code, m.end() - 1)]
        name = BAZEL_NAME.search(body)
        if not name:
            continue
        al = ALWAYSLINK.search(body)
        out[name.group(1)] = (m.group(1), bool(al) and al.group(1) == "True", line_of(code, m.start()))
    return out


def type_change_finding(label, name, pkg, gn_rel, line, old_types, new_types):
    was, now = "/".join(sorted(old_types)), "/".join(sorted(new_types))
    f = {"source": "gn_target_type_parity", "severity": "ERROR", "file": gn_rel, "line": line}
    if "source_set" in old_types and "source_set" not in new_types and "static_library" in new_types:
        f.update(
            category="source_set_became_static_library",
            message=(
                f"`{label}` was a GN `source_set()` and is now a `static_library()`. A static_library only links "
                "the objects that resolve undefined symbols, so GN dependents silently lose static initializers "
                "and other unreferenced objects of this library."
            ),
            remediation=(
                f"Add `alwayslink = True` to the `{name}` cc_library/fx_cc_library in {pkg}/BUILD.bazel (bazel2gn "
                f"then emits `source_set()`) and run `fx bazel2gn -d {pkg}`."
            ),
        )
    elif "static_library" in old_types and "static_library" not in new_types and "source_set" in new_types:
        f.update(
            category="static_library_became_source_set",
            message=(
                f"`{label}` was a GN `static_library()` and is now a `source_set()`. A source_set links all of its "
                "objects into every dependent, including ones GN left out before (duplicate symbols, larger "
                "binaries, unwanted static initializers)."
            ),
            remediation=(
                f"Remove `alwayslink = True` from the `{name}` cc_library/fx_cc_library in {pkg}/BUILD.bazel and run "
                f"`fx bazel2gn -d {pkg}`."
            ),
        )
    else:
        f.update(
            category="gn_target_type_changed",
            message=(
                f"`{label}` changed GN type from `{was}` to `{now}`. A migration must keep what GN builds: a "
                "different template can change link semantics, toolchains, SDK publishing or test wiring."
            ),
            remediation=(
                f"Use the Bazel rule and attributes that bazel2gn converts back to `{was}` (keep a GN-only wrapper "
                f"template above the BAZEL2GN SENTINEL if bazel2gn has no equivalent) and run `fx bazel2gn -d {pkg}`. "
                "If the new type is equivalent, dispute this finding in the CoderReport and say why."
            ),
        )
    return f


def alwayslink_finding(label, name, pkg, bazel_rel, rule, alwayslink, line, old_types):
    if "source_set" in old_types and not alwayslink:
        return {
            "source": "gn_target_type_parity",
            "category": "source_set_without_alwayslink",
            "severity": "ERROR",
            "file": bazel_rel,
            "line": line,
            "message": (
                f"`{label}` replaces a GN `source_set()` but the `{rule}` does not set `alwayslink = True`. Without "
                "it, links only pull in the objects that resolve undefined symbols, so dependents silently lose "
                "static initializers and other unreferenced objects the source_set always linked."
            ),
            "remediation": f"Add `alwayslink = True` to `{name}` in {bazel_rel}.",
        }
    if "static_library" in old_types and alwayslink:
        return {
            "source": "gn_target_type_parity",
            "category": "static_library_with_alwayslink",
            "severity": "ERROR",
            "file": bazel_rel,
            "line": line,
            "message": (
                f"`{label}` replaces a GN `static_library()` but the `{rule}` sets `alwayslink = True`, which links "
                "all of its objects into every dependent, including ones GN left out before."
            ),
            "remediation": f"Remove `alwayslink = True` from `{name}` in {bazel_rel}.",
        }
    return None


EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
)

candidate_dirs = {target_dir} if target_dir else set()
changed_all = git_lines(["diff", "--name-only", change_base]) + git_lines(["ls-files", "--others", "--exclude-standard"])
changed_set = set(changed_all)
for p in changed_all:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not (target_dir and d == os.path.dirname(target_dir) and f"{d}/BUILD.bazel" not in changed_set)
        ):
            candidate_dirs.add(d)

findings = []
for pkg in sorted(candidate_dirs):
    gn_rel, bazel_rel = f"{pkg}/BUILD.gn", f"{pkg}/BUILD.bazel"
    before = old_text(gn_rel)
    if not before:
        continue
    after = new_text(gn_rel)
    old = gn_types(before)
    new = gn_types(after) if after is not None else {}
    bazel_text = new_text(bazel_rel)
    cc_libs = bazel_cc_libraries(bazel_text) if bazel_text else {}
    has_bazel_host_tool = any("bazel_host_tool" in t for t, _ in new.values())
    for name, (old_types, _) in sorted(old.items()):
        label = f"//{pkg}:{name}"
        if name in new:
            new_types, line = new[name]
            is_test_suite_conversion = (
                FX_TEST_AVAILABLE
                and new_types == {"bazel_test_suite"}
                and old_types <= GN_TEST_TEMPLATES
            )
            is_host_tool_conversion = (
                has_bazel_host_tool
                and new_types <= {"bazel_host_tool", "group"}
                and old_types <= GN_HOST_TOOL_TEMPLATES | {"group"}
            )
            if new_types != old_types and not (is_test_suite_conversion or is_host_tool_conversion):
                findings.append(type_change_finding(label, name, pkg, gn_rel, line, old_types, new_types))
        elif name in cc_libs:
            rule, alwayslink, line = cc_libs[name]
            f = alwayslink_finding(label, name, pkg, bazel_rel, rule, alwayslink, line, old_types)
            if f:
                findings.append(f)

print(json.dumps(findings, indent=2))
PYEOF
