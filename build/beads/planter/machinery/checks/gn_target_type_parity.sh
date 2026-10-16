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
#      forwarding `group` in a `BUILD.gn` that defines a `bazel_host_tool`), and a
#      `bazel_input_file` or `bazel_input_directory` whose generator moved to
#      Bazel may become an empty `group` for soft transitions (while keeping
#      `bazel_input_file` for a Bazel-migrated/`bazel_host_tool` generator or
#      leaving `@gn_targets//.../<pkg>` references under `build/bazel/` is an
#      error).
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
    "executable",
    "install_host_tools",
}
GN_BAZEL_INPUT_TEMPLATES = {
    "bazel_input_file",
    "bazel_input_directory",
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
    bazel_names = set(BAZEL_NAME.findall(blank_comments(bazel_text))) if bazel_text else set()
    cc_libs = bazel_cc_libraries(bazel_text) if bazel_text else {}
    has_bazel_host_tool = any("bazel_host_tool" in t for t, _ in new.values())
    had_bazel_input = any(t & GN_BAZEL_INPUT_TEMPLATES for t, _ in old.values())
    for name, (old_types, _) in sorted(old.items()):
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
            is_test_suite_conversion = (
                FX_TEST_AVAILABLE
                and new_types == {"bazel_test_suite"}
                and old_types <= GN_TEST_TEMPLATES
            )
            is_host_tool_conversion = (
                has_bazel_host_tool
                and new_types <= {"bazel_host_tool", "group"}
                and old_types <= GN_HOST_TOOL_TEMPLATES | GN_BAZEL_INPUT_TEMPLATES | {"group"}
            )
            is_bazel_input_transition = (
                bool(bazel_text)
                and new_types == {"group"}
                and old_types <= GN_BAZEL_INPUT_TEMPLATES | {"group"}
            )
            # Check whether a rustc_library_with_features template expanded into explicit
            # rustc_library targets for the base target and all feature_sets variants.
            is_feature_set_expansion = False
            if old_types == {"rustc_library_with_features"} and new_types == {"rustc_library"}:
                code_before = blank_comments(before)
                pat = re.compile(r'\brustc_library_with_features\(\s*"' + re.escape(name) + r'"\s*\)\s*\{')
                variant_names = set()
                for fm in pat.finditer(code_before):
                    fbody = code_before[fm.end() - 1 : call_end(code_before, fm.end() - 1)]
                    for vm in re.finditer(r'\btarget_name\s*=\s*"([^"$]+)"', fbody):
                        if vm.group(1):
                            variant_names.add(vm.group(1))
                is_feature_set_expansion = all(
                    "rustc_library" in new.get(v, (set(), 0))[0] for v in variant_names
                )
            if new_types != old_types and not (
                is_test_suite_conversion
                or is_host_tool_conversion
                or is_bazel_input_transition
                or is_feature_set_expansion
            ):
                findings.append(type_change_finding(label, name, pkg, gn_rel, line, old_types, new_types))
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
