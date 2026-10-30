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
#      forwarding `group` in a `BUILD.gn` that defines a `bazel_host_tool`), a
#      GN `fuchsia_package` or `fuchsia_package_with_single_component` migrated
#      to `fx_package` in `BUILD.bazel` may become a `bazel_fuchsia_package` of
#      the same name for remaining GN dependents, and a
#      `bazel_input_file` or `bazel_input_directory` whose generator moved to
#      Bazel may become an empty `group` for soft transitions (while keeping
#      `bazel_input_file` for a Bazel-migrated/`bazel_host_tool` generator or
#      leaving `@gn_targets//.../<pkg>` references under `build/bazel/` is an
#      error). A pure single-target forwarding `group("<outer>")` whose sole
#      same-package convertible target `:<inner>` has no external callers may
#      also fold `:<inner>` directly into `<tmpl>("<outer>")`. A
#      `fuchsia_test_component` without `test_type` and a `testonly`
#      `fuchsia_component` may convert in either direction (helper components
#      become `fuchsia_component` via `fx_component(testonly = True)`, while
#      test roots in `fx_package(test_components = [...])` become
#      `fuchsia_test_component` via `fx_test_component`), because
#      `//build/components/fuchsia_test_component.gni` only adds
#      `testonly = true` and `test_type` metadata to `fuchsia_component`.
#      A GN `cc_test_executable` may become the testonly `executable` that
#      bazel2gn emits for `fx_cc_binary(testonly = True)`, but:
#   6. cc_test_executable_lost_lto_config (error): the `executable` lacks the
#      `//build/config/lto:no-fat-lto-objects-linking` config that
#      `cc_test_executable` adds unless `disable_lto = false`; set it with
#      `build_flags` on the `fx_cc_binary`.
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
GN_TEMPLATE = re.compile(r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{')
GN_INNER_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*([^)]+?)\s*\)\s*\{')
TARGET_NAME_ARGS = {
    "target_name",
    '"$target_name"',
    '"${target_name}"',
    "invoker.target_name",
    '"$invoker.target_name"',
    '"${invoker.target_name}"',
}
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from", "if", "config"}
CC_RULE = re.compile(r'(?<![\w.])(cc_library|fx_cc_library)\s*\(')
BAZEL_NAME = re.compile(r'\bname\s*=\s*"([^"]+)"')
ALWAYSLINK = re.compile(r'\balwayslink\s*=\s*(True|False)\b')
# With fx_test in the checkout, a GN test package migrated to Bazel is replaced by a GN
# bazel_test_suite of the same name that exports the Bazel tests to tests.json.
FX_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl"))
# With fx_cc_test in the checkout, bazel2gn can generate GN test() from BUILD.bazel.
FX_CC_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/cc/fx_cc_test.bzl"))
GN_TEST_TEMPLATES = {
    "fuchsia_unittest_package",
    "fuchsia_test_package",
    "fuchsia_unittest_component",
    "fuchsia_test_component",
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
    "python_binary",
    "sdk_executable_host_tool",
    "sdk_go_binary_host_tool",
    "sdk_rustc_binary_host_tool",
    "sdk_host_tool",
    "executable",
    "install_host_tools",
    "ffx_tool",
}
GN_BAZEL_INPUT_TEMPLATES = {
    "bazel_input_file",
    "bazel_input_directory",
}
GN_PACKAGE_TEMPLATES = {
    "fuchsia_package",
    "fuchsia_package_with_single_component",
}
GN_CONVERTIBLE_SYMMETRY_TEMPLATES = {
    "static_library",
    "source_set",
    "shared_library",
    "library_headers",
    "rustc_library",
    "rustc_macro",
    "go_library",
    "python_library",
    "fidl",
    "ffx_plugin",
    "ffx_tool",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
}


def is_testonly_target(gn_text, name):
    if not gn_text:
        return False
    code = blank_comments(gn_text)
    pat = re.compile(r'\b[A-Za-z_]\w*\(\s*"' + re.escape(name) + r'"\s*\)\s*\{')
    for m in pat.finditer(code):
        body = code[m.end() - 1 : call_end(code, m.end() - 1)]
        if re.search(r"\btestonly\s*=\s*true\b", body):
            return True
    return False


# Check whether any build file outside `pkg` still references `//<pkg>:<target_name>`.
def has_external_target_refs(pkg, target_name):
    pkg_base = os.path.basename(pkg)
    try:
        out = subprocess.check_output(
            [
                "git",
                "-C",
                workdir,
                "grep",
                "-l",
                "-F",
                f"{pkg_base}:{target_name}",
                "--",
                "BUILD.gn",
                "BUILD.bazel",
                "*.gni",
                "*.bzl",
                "**/BUILD.gn",
                "**/BUILD.bazel",
            ],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return False
    pat = re.compile(
        r'"@?(?://'
        + re.escape(pkg)
        + r"|(?:[^\"\s:]*/)*"
        + re.escape(pkg_base)
        + r"):"
        + re.escape(target_name)
        + r'(?:["(])'
    )
    for rel in out.splitlines():
        rel = rel.strip().strip("/")
        if not rel or rel == pkg or rel.startswith(pkg + "/"):
            continue
        txt = new_text(rel) or ""
        if pat.search(blank_comments(txt)):
            return True
    return False


# Identify pure single-target forwarding `group("<outer>")` targets in `before` whose
# same-package convertible target `:<inner>` was folded directly into `<outer>`.
def find_folded_forwarding_groups(gn_text, old_map, new_map, bazel_names, pkg):
    folded_from_group = {}
    folded_into_group = {}
    if not gn_text:
        return folded_from_group, folded_into_group
    code = blank_comments(gn_text)
    allowed_group_attrs = {"public_deps", "deps", "visibility", "testonly"}
    convertible = GN_CONVERTIBLE_SYMMETRY_TEMPLATES | {"rustc_library_with_features"}
    for m in GN_TARGET.finditer(code):
        tmpl, outer = m.group(1), m.group(2)
        if tmpl != "group" or old_map.get(outer, (set(), 0))[0] != {"group"}:
            continue
        body = code[m.end() - 1 : call_end(code, m.end() - 1)][1:-1]
        attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", body))
        if not attrs or not (attrs <= allowed_group_attrs):
            continue
        deps = []
        for dm in re.finditer(
            r"(?<![\w.])(?:public_deps|deps)\s*\+?=\s*\[([^\]]*)\]", body
        ):
            deps.extend(re.findall(r'"([^"]+)"', dm.group(1)))
        if len(deps) != 1 or "(" in deps[0]:
            continue
        dep = deps[0].strip()
        if dep.startswith(":"):
            inner = dep[1:]
        elif dep.startswith(f"//{pkg}:"):
            inner = dep[len(f"//{pkg}:") :]
        else:
            continue
        if inner == outer or inner not in old_map:
            continue
        inner_types = old_map[inner][0]
        if not (inner_types <= convertible):
            continue
        if is_testonly_target(gn_text, outer) != is_testonly_target(gn_text, inner):
            continue
        if inner in new_map or inner in bazel_names:
            continue
        if outer not in new_map and outer not in bazel_names:
            continue
        if has_external_target_refs(pkg, inner):
            continue
        folded_from_group[outer] = inner
        folded_into_group[inner] = outer
    return folded_from_group, folded_into_group


def bazel_input_generator_migrated(gn_text, name, pkg, bazel_names, new_map):
    if not gn_text:
        return False
    code = blank_comments(gn_text)
    pat = re.compile(r'\b(?:bazel_input_file|bazel_input_directory)\(\s*"' + re.escape(name) + r'"\s*\)\s*\{')
    for m in pat.finditer(code):
        body = code[m.end() - 1 : call_end(code, m.end() - 1)]
        gm = re.search(r'\bgenerator\s*=\s*"([^"]+)"', body)
        if not gm:
            continue
        gen = gm.group(1).split("(")[0].strip()
        if gen.startswith(":"):
            gname = gen[1:]
        elif gen.startswith(f"//{pkg}:"):
            gname = gen[len(f"//{pkg}:") :]
        elif gen == f"//{pkg}":
            gname = os.path.basename(pkg)
        else:
            continue
        if gname in bazel_names or "bazel_host_tool" in new_map.get(gname, (set(), 0))[0]:
            return True
    return False


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


def call_end(code, open_paren):
    """Index just past the `)` or `}` closing the `(` or `{` at code[open_paren] (comments are already blank)."""
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


def gn_target_body(text, tmpl, name):
    """Returns the `{...}` body of `tmpl("name")` in GN `text` (comments blanked), or None."""
    if not text:
        return None
    code = blank_comments(text)
    m = re.search(r"\b" + re.escape(tmpl) + r'\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', code)
    if not m:
        return None
    return code[m.end() - 1 : call_end(code, m.end() - 1)]


# Check whether a GN component target body (or its enclosing BUILD.gn file scope) sets testonly.
def is_gn_component_testonly(gn_text, comp_body):
    if not comp_body or re.search(r"\btestonly\s*=\s*false\b", comp_body):
        return False
    if re.search(r"\btestonly\s*=\s*true\b", comp_body):
        return True
    if not gn_text:
        return False
    code = blank_comments(gn_text)
    top_chars = []
    depth, quote, i = 0, None, 0
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
        elif c == "{":
            depth += 1
        elif c == "}":
            depth = max(0, depth - 1)
        elif depth == 0:
            top_chars.append(c)
        i += 1
    return re.search(r"(?m)^\s*testonly\s*=\s*true\b", "".join(top_chars)) is not None


# Check bidirectional equivalence between `fuchsia_test_component` without `test_type`
# and `testonly` `fuchsia_component` (//build/components/fuchsia_test_component.gni).
def equivalent_test_component_templates(before_gn, after_gn, orig_name, name, old_types, new_types):
    if old_types == {"fuchsia_test_component"} and new_types == {"fuchsia_component"}:
        old_body = gn_target_body(before_gn, "fuchsia_test_component", orig_name)
        new_body = gn_target_body(after_gn, "fuchsia_component", name)
        if (
            old_body is not None
            and new_body is not None
            and not re.search(r"\btest_type\s*=", old_body)
            and is_gn_component_testonly(after_gn, new_body)
        ):
            return ("fuchsia_test_component", "fuchsia_component")
    elif old_types == {"fuchsia_component"} and new_types == {"fuchsia_test_component"}:
        old_body = gn_target_body(before_gn, "fuchsia_component", orig_name)
        new_body = gn_target_body(after_gn, "fuchsia_test_component", name)
        if (
            old_body is not None
            and new_body is not None
            and is_gn_component_testonly(before_gn, old_body)
            and not re.search(r"\btest_type\s*=", new_body)
        ):
            return ("fuchsia_component", "fuchsia_test_component")
    return None


def gn_types(text):
    """Maps each GN target name to the set of templates defining it (resolving file-local single-target templates)."""
    code = blank_comments(text)
    local_templates = {}
    tmpl_spans = []
    for m in GN_TEMPLATE.finditer(code):
        end = call_end(code, m.end() - 1)
        tmpl_spans.append((m.start(), end))
        body = code[m.end() : end - 1]
        target_name_tmpls = set()
        has_non_test_side_target = False
        for im in GN_INNER_TARGET.finditer(body):
            inner_tmpl, arg = im.group(1), im.group(2).strip()
            if inner_tmpl in NOT_TARGETS:
                continue
            if arg in TARGET_NAME_ARGS:
                target_name_tmpls.add(inner_tmpl)
            elif inner_tmpl not in GN_TEST_TEMPLATES:
                has_non_test_side_target = True
        if target_name_tmpls and not has_non_test_side_target:
            local_templates[m.group(1)] = target_name_tmpls
    for _ in range(len(local_templates)):
        for k, vals in list(local_templates.items()):
            expanded = set()
            for v in vals:
                expanded |= local_templates.get(v, {v})
            local_templates[k] = expanded
    out = {}
    for m in GN_TARGET.finditer(code):
        if any(s <= m.start() < e for s, e in tmpl_spans):
            continue
        tmpl, name = m.group(1), m.group(2)
        if tmpl in NOT_TARGETS:
            continue
        types, line = out.get(name, (set(), line_of(code, m.start())))
        types |= local_templates.get(tmpl, {tmpl})
        out[name] = (types, line)
    return out


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


# Map each BUILD.bazel target name to the 1-based line number of its rule call.
def bazel_target_lines(text):
    if not text:
        return {}
    code = blank_comments(text)
    out = {}
    for m in re.finditer(r"(?<![\w.])([A-Za-z_]\w*)\s*\(", code):
        body = code[m.end() - 1 : call_end(code, m.end() - 1)]
        name = BAZEL_NAME.search(body)
        if name and name.group(1) not in out:
            out[name.group(1)] = line_of(code, m.start())
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
    elif FX_CC_TEST_AVAILABLE and "test" in old_types and "test" not in new_types and "executable" in new_types:
        f.update(
            category="test_became_executable",
            message=(
                f"`{label}` was a GN C++ `test()` and is now an `executable()`, usually because BUILD.bazel uses "
                "`fx_cc_binary(testonly = True)`. GN `test()` disables FatLTO linking, installs the binary to "
                "`test/<output_name>` in packages and adds a `host_test()` on host; `executable()` does none of these."
            ),
            remediation=(
                f"Replace the `{name}` `fx_cc_binary(testonly = True)` in {pkg}/BUILD.bazel with `fx_cc_test` (load "
                "`//build/bazel/rules/cc:fx_cc_test.bzl`, drop `testonly`) so bazel2gn generates `test()`, and run "
                f"`fx bazel2gn -d {pkg}`. Do not dispute this finding."
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


# Check whether a pre-existing BUILD.bazel outside the migrated target directories was
# only modified to register tests in test_suite(), export files via exports_files(), or
# update target visibility allowlists, without dual-build migrating that package.
_REG_ONLY_PKG_CACHE = {}


def is_registration_only_bazel_pkg(pkg_dir):
    pkg_dir = pkg_dir.strip("/")
    if pkg_dir in _REG_ONLY_PKG_CACHE:
        return _REG_ONLY_PKG_CACHE[pkg_dir]
    target_pkgs = {
        d.strip().lstrip("/").rstrip("/")
        for d in os.environ.get("PLANTER_TARGET_DIRS", "").split()
        if d.strip()
    }
    if target_dir:
        target_pkgs.add(target_dir)
    if not pkg_dir or pkg_dir in target_pkgs:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    gn_rel = f"{pkg_dir}/BUILD.gn"
    gn_cur = new_text(gn_rel)
    if gn_cur and "BAZEL2GN SENTINEL" in gn_cur:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    bazel_rel = f"{pkg_dir}/BUILD.bazel"
    cur_src = new_text(bazel_rel)
    if not cur_src:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    prev_src = None
    for ref in dict.fromkeys([change_base, "HEAD", "HEAD~1"]):
        if not ref:
            continue
        try:
            cand = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if cand == cur_src and ref != "HEAD~1":
                continue
            prev_src = cand
            break
        except Exception:
            continue
    if prev_src is None:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    try:
        import ast

        prev_tree = ast.parse(prev_src, filename=bazel_rel)
        cur_tree = ast.parse(cur_src, filename=bazel_rel)
    except Exception:
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False

    def strip_vis_dump(stmt):
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            return None
        call = stmt.value
        kept = [(kw.arg, ast.dump(kw.value)) for kw in call.keywords if kw.arg != "visibility"]
        return (ast.dump(call.func), tuple(ast.dump(a) for a in call.args), tuple(kept))

    prev_dumps = {ast.dump(s) for s in prev_tree.body}
    prev_stripped = {d for d in (strip_vis_dump(s) for s in prev_tree.body) if d is not None}
    for stmt in cur_tree.body:
        if ast.dump(stmt) in prev_dumps:
            continue
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            _REG_ONLY_PKG_CACHE[pkg_dir] = False
            return False
        call = stmt.value
        fn = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
        )
        if (
            fn in ("test_suite", "exports_files", "stamp_group", "filegroup")
            or strip_vis_dump(stmt) in prev_stripped
        ):
            continue
        _REG_ONLY_PKG_CACHE[pkg_dir] = False
        return False
    _REG_ONLY_PKG_CACHE[pkg_dir] = True
    return True


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
        if d and is_registration_only_bazel_pkg(d):
            continue
        if (
            d
            and d not in ("tools", "src", "sdk", "build")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not (target_dir and d == os.path.dirname(target_dir) and f"{d}/BUILD.bazel" not in changed_set)
        ):
            candidate_dirs.add(d)

# 1-based line of the rule call in BUILD.bazel that defines `name` (0 if not found).
def bazel_target_line(bazel_text, name):
    if not bazel_text:
        return 0
    pat = re.compile(r'^(\w+)\(\s*\n\s*name\s*=\s*"' + re.escape(name) + r'"', re.M)
    m = pat.search(bazel_text)
    return bazel_text.count("\n", 0, m.start()) + 1 if m else 0


# Record INFO template_parity coverage so planter's deduplicator (same
# category/file/line/message key) suppresses review_diff's ERROR for it.
def append_template_parity_info(
    findings, bazel_text, bazel_rel, gn_rel, name, gn_line, old_tmpl, new_tmpl, old_name=None
):
    orig = old_name or name
    bz_line = bazel_target_line(bazel_text, name)
    if bz_line:
        findings.append(
            {
                "source": "gn_target_type_parity",
                "category": "template_parity",
                "severity": "INFO",
                "file": bazel_rel,
                "line": bz_line,
                "message": (
                    f'Target "{name}" (Rule & Directives): bazel2gn generated '
                    f'{new_tmpl}("{name}") below the sentinel ({gn_rel}:{gn_line}), but '
                    f'the original GN target was {old_tmpl}("{orig}"); '
                    "the migration must keep the GN template"
                ),
            }
        )


NO_FAT_LTO_LINKING = "//build/config/lto:no-fat-lto-objects-linking"


# GN `cc_test_executable()` (//build/testing/cc_test_executable.gni), and GN `test()` which wraps
# it, add `configs += [ NO_FAT_LTO_LINKING ]` unless `disable_lto = false`. A bazel2gn-generated
# `executable()` only gets it from `build_flags` on the Bazel `fx_cc_binary`, so report a
# conversion that drops it.
def append_lost_lto_config_error(
    findings, before, after, gn_rel, label, name, line, was_tmpl, old_name=None
):
    old_body = gn_target_body(before, was_tmpl, old_name or name)
    if old_body is None or re.search(r"\bdisable_lto\s*=\s*false\b", old_body):
        return
    new_body = gn_target_body(after, "executable", name)
    if new_body is None or NO_FAT_LTO_LINKING in new_body:
        return
    findings.append(
        {
            "source": "gn_target_type_parity",
            "category": "cc_test_executable_lost_lto_config",
            "severity": "ERROR",
            "file": gn_rel,
            "line": line,
            "message": (
                f"`{label}` was GN `{was_tmpl}`, which links with `{NO_FAT_LTO_LINKING}`\n"
                "unless it sets `disable_lto = false`, but the bazel2gn-generated `executable`\n"
                "does not, so the migration changes how the test links in GN and Bazel."
            ),
            "remediation": (
                f'Add `build_flags = ["{NO_FAT_LTO_LINKING}"]` to the Bazel\n'
                "`fx_cc_binary(testonly = True)`; bazel2gn turns it into `configs +=`."
            ),
        }
    )


# Check whether a pre-migration GN target was a unittest/test component that maps to
# fx_test_component -> fuchsia_test_component when packaged via fx_package(test_components).
def is_test_root_component_in_base(gn_text, name, old_types):
    if old_types == {"fuchsia_unittest_component"}:
        return True
    if old_types != {"fuchsia_component"} or not gn_text:
        return False
    if is_testonly_target(gn_text, name):
        return True
    code = blank_comments(gn_text)
    return bool(
        re.search(
            r"\btest_components\s*\+?=\s*\[[^\]]*\"(?::|//[^\":]+:)"
            + re.escape(name)
            + r"\"",
            code,
        )
    )


# Emit INFO template_parity coverage when a renamed Bazel target (`new_name` not in `old_map`)
# unwraps or converts a removed pre-migration GN test/component target (`old_name` not in `new_map`)
# that diffengine pairs by component_name, manifest, package_name, or binary_name.
def record_renamed_unwrap_template_parity(
    findings, before, after, old_map, new_map, bazel_text, bazel_names, bazel_rel, gn_rel
):
    if not bazel_text or not before or not after:
        return
    removed_old = [
        (old_name, old_types)
        for old_name, (old_types, _) in sorted(old_map.items())
        if old_name not in new_map
    ]
    if not removed_old:
        return
    for new_name, (new_types, new_line) in sorted(new_map.items()):
        if new_name in old_map or new_name not in bazel_names:
            continue
        if new_types == {"fuchsia_test_component"}:
            for old_name, old_types in removed_old:
                if is_test_root_component_in_base(before, old_name, old_types):
                    append_template_parity_info(
                        findings,
                        bazel_text,
                        bazel_rel,
                        gn_rel,
                        new_name,
                        new_line,
                        next(iter(old_types)),
                        "fuchsia_test_component",
                        old_name=old_name,
                    )
        elif new_types == {"fuchsia_test_package"}:
            for old_name, old_types in removed_old:
                if old_types == {"fuchsia_unittest_package"}:
                    append_template_parity_info(
                        findings,
                        bazel_text,
                        bazel_rel,
                        gn_rel,
                        new_name,
                        new_line,
                        "fuchsia_unittest_package",
                        "fuchsia_test_package",
                        old_name=old_name,
                    )
        elif new_types == {"fuchsia_component"} and is_testonly_target(after, new_name):
            for old_name, old_types in removed_old:
                if old_types == {"fuchsia_test_component"}:
                    old_body = gn_target_body(before, "fuchsia_test_component", old_name)
                    if old_body is not None and not re.search(r"\btest_type\s*=", old_body):
                        append_template_parity_info(
                            findings,
                            bazel_text,
                            bazel_rel,
                            gn_rel,
                            new_name,
                            new_line,
                            "fuchsia_test_component",
                            "fuchsia_component",
                            old_name=old_name,
                        )
        elif new_types == {"executable"} and is_testonly_target(after, new_name):
            allowed_exec_tmpls = (
                ({"cc_test_executable"},)
                if FX_CC_TEST_AVAILABLE
                else ({"cc_test_executable"}, {"test"})
            )
            for old_name, old_types in removed_old:
                if old_types in allowed_exec_tmpls:
                    append_template_parity_info(
                        findings,
                        bazel_text,
                        bazel_rel,
                        gn_rel,
                        new_name,
                        new_line,
                        next(iter(old_types)),
                        "executable",
                        old_name=old_name,
                    )
                    append_lost_lto_config_error(
                        findings,
                        before,
                        after,
                        gn_rel,
                        f"//{os.path.dirname(gn_rel)}:{new_name}",
                        new_name,
                        new_line,
                        next(iter(old_types)),
                        old_name=old_name,
                    )


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
    bazel_names = set(BAZEL_NAME.findall(blank_comments(bazel_text))) if bazel_text else set()
    bazel_lines = bazel_target_lines(bazel_text)
    cc_libs = bazel_cc_libraries(bazel_text) if bazel_text else {}
    has_bazel_host_tool = any(
        bool(t & {"bazel_host_tool", "bazel_ffx_tool"}) for t, _ in new.values()
    )
    had_bazel_input = any(t & GN_BAZEL_INPUT_TEMPLATES for t, _ in old.values())
    folded_from_group, folded_into_group = find_folded_forwarding_groups(
        before, old, new, bazel_names, pkg
    )
    for name, (old_types, _) in sorted(old.items()):
        if name in folded_into_group:
            continue
        orig_name = folded_from_group.get(name, name)
        if name in folded_from_group:
            old_types = old[orig_name][0]
        label = f"//{pkg}:{name}"
        if name in new:
            new_types, line = new[name]
            if new_types & GN_BAZEL_INPUT_TEMPLATES and (
                has_bazel_host_tool or bazel_input_generator_migrated(after, name, pkg, bazel_names, new)
            ):
                findings.append(
                    {
                        "source": "gn_target_type_parity",
                        "category": "stale_bazel_input_target",
                        "severity": "ERROR",
                        "file": gn_rel,
                        "line": line,
                        "message": (
                            f"`{label}` in `{gn_rel}` still uses `bazel_input_file`/`bazel_input_directory` for a "
                            f"target now built in `{bazel_rel}` (or wrapped by `bazel_host_tool()`). A "
                            "`bazel_host_tool()` or Bazel-migrated target must never be bridged back to Bazel via "
                            "`bazel_input_file()`."
                        ),
                        "remediation": (
                            f"Replace `bazel_input_file(\"{name}\")` (and any `else` branch) in `{gn_rel}` with an "
                            f"empty `group(\"{name}\") {{}}` for soft-transitioning references in other repositories "
                            "(or delete it if unreferenced), wrap migrated host binaries in `bazel_host_tool()` "
                            "instead of dual-building them with `bazel2gn`, and update any "
                            f"`@gn_targets//.../{pkg}:...` references in `.bzl` or `BUILD.bazel` files to point "
                            f"directly at the migrated target in `//{pkg}`."
                        ),
                    }
                )
                continue
            has_bazel_test_suite = any("bazel_test_suite" in t for t, _ in new.values())
            is_test_suite_conversion = (
                FX_TEST_AVAILABLE
                and (
                    new_types == {"bazel_test_suite"}
                    or (new_types == {"group"} and has_bazel_test_suite)
                )
                and old_types <= GN_TEST_TEMPLATES
            )
            is_host_tool_conversion = (
                has_bazel_host_tool
                and new_types <= {"bazel_host_tool", "bazel_ffx_tool", "group"}
                and old_types <= GN_HOST_TOOL_TEMPLATES | GN_BAZEL_INPUT_TEMPLATES | {"group"}
            )
            is_bazel_input_transition = (
                bool(bazel_text)
                and new_types == {"group"}
                and old_types <= GN_BAZEL_INPUT_TEMPLATES | {"group"}
            )
            # Check whether a GN fuchsia_package or fuchsia_package_with_single_component
            # was migrated to fx_package in BUILD.bazel while retaining a thin
            # bazel_fuchsia_package wrapper (or forwarding group) in BUILD.gn.
            is_fuchsia_package_conversion = (
                bool(bazel_text)
                and new_types <= {"bazel_fuchsia_package", "group"}
                and old_types <= GN_PACKAGE_TEMPLATES | {"group"}
                and any("bazel_fuchsia_package" in t for t, _ in new.values())
            )
            # Check whether a rustc_library_with_features template expanded into explicit
            # rustc_library targets for the base target and all feature_sets variants.
            is_feature_set_expansion = False
            if old_types == {"rustc_library_with_features"} and new_types == {"rustc_library"}:
                code_before = blank_comments(before)
                pat = re.compile(
                    r'\brustc_library_with_features\(\s*"'
                    + re.escape(orig_name)
                    + r'"\s*\)\s*\{'
                )
                variant_names = set()
                for fm in pat.finditer(code_before):
                    fbody = code_before[fm.end() - 1 : call_end(code_before, fm.end() - 1)]
                    for vm in re.finditer(r'\btarget_name\s*=\s*"([^"$]+)"', fbody):
                        if vm.group(1):
                            variant_names.add(vm.group(1))
                is_feature_set_expansion = all(
                    "rustc_library" in new.get(v, (set(), 0))[0] for v in variant_names
                )
            if is_feature_set_expansion and name in bazel_names:
                bz_line = bazel_target_line(bazel_text, name)
                if bz_line:
                    findings.append(
                        {
                            "source": "gn_target_type_parity",
                            "category": "template_parity",
                            "severity": "INFO",
                            "file": bazel_rel,
                            "line": bz_line,
                            "message": (
                                f'Target "{name}" (Rule & Directives): bazel2gn generated '
                                f'rustc_library("{name}") below the sentinel ({gn_rel}:{line}), '
                                "but the original GN target was "
                                f'rustc_library_with_features("{name}"); the migration must keep '
                                "the GN template"
                            ),
                        }
                    )
            # Accept a GN cc_test_executable (or test when fx_cc_test.bzl is unavailable) converted
            # from a Bazel fx_cc_binary(testonly = True): bazel2gn maps fx_cc_binary/cc_binary to
            # executable() and has no form that emits cc_test_executable(). The executable must
            # keep cc_test_executable's LTO config (see append_lost_lto_config_error).
            allowed_exec_tmpls = (
                ({"cc_test_executable"},)
                if FX_CC_TEST_AVAILABLE
                else ({"cc_test_executable"}, {"test"})
            )
            is_cc_test_executable_conversion = (
                old_types in allowed_exec_tmpls
                and new_types == {"executable"}
                and name in bazel_names
                and is_testonly_target(after, name)
            )
            if is_cc_test_executable_conversion:
                was_tmpl = next(iter(old_types))
                findings.append(
                    {
                        "source": "gn_target_type_parity",
                        "category": "cc_test_executable_converted",
                        "severity": "INFO",
                        "file": gn_rel,
                        "line": line,
                        "message": (
                            f"`{label}` was GN `{was_tmpl}` and is now a bazel2gn-generated\n"
                            "testonly `executable` from a Bazel fx_cc_binary(testonly = True); bazel2gn\n"
                            f"has no form for {was_tmpl}, so this type change is expected."
                        ),
                        "remediation": (
                            "None needed for the type change; do not dispute or work around this\n"
                            "INFO. Keep the LTO config (`cc_test_executable_lost_lto_config`)."
                        ),
                    }
                )
                append_lost_lto_config_error(
                    findings, before, after, gn_rel, label, name, line, was_tmpl, old_name=orig_name
                )
                append_template_parity_info(
                    findings,
                    bazel_text,
                    bazel_rel,
                    gn_rel,
                    name,
                    line,
                    was_tmpl,
                    "executable",
                )
            # Accept a GN `fuchsia_unittest_package` or `fuchsia_unittest_component` that
            # becomes `fuchsia_test_package` or `fuchsia_test_component` below the sentinel
            # when migrated to `fx_package(test_components = [...])`, `fx_test_component`, or
            # multi-binary `fx_unittest_package` in BUILD.bazel (e.g. for a custom .cml
            # manifest or test_type; fuchsia_unittest_package.gni and
            # fuchsia_unittest_component.gni expand to those templates internally).
            is_unittest_package_unwrap = (
                old_types == {"fuchsia_unittest_package"}
                and new_types == {"fuchsia_test_package"}
                and name in bazel_names
            )
            if is_unittest_package_unwrap:
                findings.append(
                    {
                        "source": "gn_target_type_parity",
                        "category": "unittest_package_unwrapped",
                        "severity": "INFO",
                        "file": gn_rel,
                        "line": line,
                        "message": (
                            f"`{label}` was GN `fuchsia_unittest_package` and is now a "
                            "bazel2gn-generated `fuchsia_test_package` (which "
                            "`fuchsia_unittest_package.gni` expands to internally when "
                            "unrolled in Bazel for a custom manifest, test_type, or "
                            "multiple test components)."
                        ),
                        "remediation": "None needed; do not dispute or work around this INFO.",
                    }
                )
                append_template_parity_info(
                    findings,
                    bazel_text,
                    bazel_rel,
                    gn_rel,
                    name,
                    line,
                    "fuchsia_unittest_package",
                    "fuchsia_test_package",
                )
            is_unittest_component_unwrap = (
                is_test_root_component_in_base(before, orig_name, old_types)
                and new_types == {"fuchsia_test_component"}
                and name in bazel_names
            )
            if is_unittest_component_unwrap:
                was_comp_tmpl = next(iter(old_types))
                findings.append(
                    {
                        "source": "gn_target_type_parity",
                        "category": "unittest_component_unwrapped",
                        "severity": "INFO",
                        "file": gn_rel,
                        "line": line,
                        "message": (
                            f"`{label}` was GN `{was_comp_tmpl}` and is now a "
                            "bazel2gn-generated `fuchsia_test_component` (required when "
                            "packaged via `fx_package(test_components = [...])` in Bazel)."
                        ),
                        "remediation": "None needed; do not dispute or work around this INFO.",
                    }
                )
                append_template_parity_info(
                    findings,
                    bazel_text,
                    bazel_rel,
                    gn_rel,
                    name,
                    line,
                    was_comp_tmpl,
                    "fuchsia_test_component",
                    old_name=orig_name,
                )
            # Without `test_type`, `fuchsia_test_component` and `testonly` `fuchsia_component`
            # expand identically (//build/components/fuchsia_test_component.gni): helper
            # components migrate to `fx_component(testonly = True)` (`fuchsia_component`),
            # while test roots in `fx_package(test_components = [...])` require
            # `fx_test_component` (`fuchsia_test_component`).
            equiv_test_comp = equivalent_test_component_templates(
                before, after, orig_name, name, old_types, new_types
            )
            if equiv_test_comp and not is_unittest_component_unwrap and name in bazel_names:
                old_tmpl, new_tmpl = equiv_test_comp
                append_template_parity_info(
                    findings,
                    bazel_text,
                    bazel_rel,
                    gn_rel,
                    name,
                    line,
                    old_tmpl,
                    new_tmpl,
                    old_name=orig_name,
                )
            if new_types != old_types and not (
                is_cc_test_executable_conversion
                or is_test_suite_conversion
                or is_host_tool_conversion
                or is_bazel_input_transition
                or is_fuchsia_package_conversion
                or is_feature_set_expansion
                or is_unittest_package_unwrap
                or is_unittest_component_unwrap
                or equiv_test_comp
            ):
                findings.append(
                    type_change_finding(label, name, pkg, gn_rel, line, old_types, new_types)
                )
        elif name in cc_libs:
            rule, alwayslink, line = cc_libs[name]
            f = alwayslink_finding(label, name, pkg, bazel_rel, rule, alwayslink, line, old_types)
            if f:
                findings.append(f)
        elif (
            bazel_text
            and name not in bazel_names
            and (old_types <= GN_CONVERTIBLE_SYMMETRY_TEMPLATES)
            and not is_testonly_target(before, name)
        ):
            was = "/".join(sorted(old_types))
            findings.append(
                {
                    "source": "gn_target_type_parity",
                    "category": "missing_migrated_target",
                    "severity": "ERROR",
                    "file": bazel_rel,
                    "line": 1,
                    "message": (
                        f"`{label}` (GN `{was}`) existed in `{gn_rel}` at `{change_base}` but is missing from both "
                        f"`{bazel_rel}` and `{gn_rel}`. Every convertible non-test GN target must have a "
                        f"corresponding target in `{bazel_rel}` (or remain in `{gn_rel}` above the BAZEL2GN SENTINEL "
                        "if blocked by a GN-only attribute)."
                    ),
                    "remediation": (
                        f"Migrate `{name}` (`{was}`) to `{bazel_rel}` and run `fx bazel2gn -d {pkg}` (or restore it "
                        f"in `{gn_rel}` if it cannot be migrated). If dropping `{label}` was intentional because it "
                        "had zero reverse dependencies and is dead code, dispute this finding in the CoderReport."
                    ),
                }
            )

    record_renamed_unwrap_template_parity(
        findings, before, after, old, new, bazel_text, bazel_names, bazel_rel, gn_rel
    )

    if bazel_text and (had_bazel_input or has_bazel_host_tool):
        gn_targets_pat = re.compile(r'@gn_targets//(?:[^\s"\':]+/)?' + re.escape(pkg) + r'(?::([^\s"\']+))?')
        bazel_dir = os.path.join(workdir, "build/bazel")
        if os.path.isdir(bazel_dir):
            for root, _, files in os.walk(bazel_dir):
                for fn in sorted(files):
                    if not (fn in ("BUILD.bazel", "BUILD") or fn.endswith(".bzl")):
                        continue
                    rel_f = os.path.relpath(os.path.join(root, fn), workdir)
                    txt = new_text(rel_f)
                    if not txt:
                        continue
                    code_f = blank_comments(txt)
                    for gm in gn_targets_pat.finditer(code_f):
                        findings.append(
                            {
                                "source": "gn_target_type_parity",
                                "category": "stale_gn_targets_reference",
                                "severity": "ERROR",
                                "file": rel_f,
                                "line": line_of(code_f, gm.start()),
                                "message": (
                                    f"`{rel_f}` still references `{gm.group(0)}`, which bridged `//{pkg}` from GN "
                                    f"via `bazel_input_file`, even though `//{pkg}` is now migrated to `{bazel_rel}`."
                                ),
                                "remediation": (
                                    f"Replace `{gm.group(0)}` in `{rel_f}` with the migrated Bazel target label in "
                                    f"`//{pkg}` (e.g. `//{pkg}:<target>`) so Bazel rules use the Bazel-compiled "
                                    f"target directly, and ensure `{bazel_rel}` grants visibility to `{rel_f}`'s "
                                    "package or callers (such as `\"//build/bazel:__subpackages__\"`)."
                                ),
                            }
                        )

print(json.dumps(findings, indent=2))
PYEOF
