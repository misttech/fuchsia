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
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from", "if"}
KNOWN_OR_DEDICATED_GN_ATTRS = {
    "sources", "public", "inputs", "deps", "public_deps", "data_deps", "non_rust_deps",
    "proc_macro_deps", "link_deps", "testonly", "visibility", "output_name", "crate_name",
    "edition", "with_unit_tests", "configs", "public_configs", "all_dependent_configs",
    "cflags", "cflags_c", "cflags_cc", "rustflags", "rustenv", "ldflags", "libs",
    "defines", "include_dirs", "friend", "allow_circular_includes_from", "data",
    "bridge_header", "args", "script", "outputs", "depfile", "hermetic_deps",
    "sdk_name", "category", "sdk_area", "sdk_headers_for_internal_use", "package_name",
    "component_name", "manifest", "deps_for_tests", "test_deps", "version", "test_specs",
    "dest", "source_root", "features", "disable_rustdoc", "disable_clippy",
    "original_target_name", "applicable_licenses", "assert_no_deps",
}
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


def eval_starlark(node, top_vars):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, ast.Name):
        return list(top_vars.get(node.id, [node.id]))
    if isinstance(node, (ast.List, ast.Tuple)):
        out = []
        for elem in node.elts:
            out.extend(eval_starlark(elem, top_vars))
        return out
    if isinstance(node, ast.BinOp):
        return eval_starlark(node.left, top_vars) + eval_starlark(node.right, top_vars)
    if isinstance(node, ast.Call):
        out = []
        for arg in node.args:
            out.extend(eval_starlark(arg, top_vars))
        return out
    if isinstance(node, ast.Dict):
        out = []
        for val in node.values:
            out.extend(eval_starlark(val, top_vars))
        return out
    return []


def parse_gn_top_attrs(body):
    attrs = {}
    lines = body.splitlines()
    i = 0
    assign_re = re.compile(r"^([A-Za-z_]\w*)\s*(\+?=|-=)\s*(.*)$")
    while i < len(lines):
        code = re.sub(r"#.*$", "", lines[i]).strip()
        if not code or code == "}":
            i += 1
            continue
        m = assign_re.match(code)
        if not m or (m.group(2) == "=" and m.group(3).lstrip().startswith("=")):
            i += 1
            continue
        attr_name, op, rhs = m.group(1), m.group(2), m.group(3).strip()
        balance = rhs.count("[") - rhs.count("]")
        while (balance > 0 or rhs.rstrip().endswith("+")) and i + 1 < len(lines):
            i += 1
            nxt = re.sub(r"#.*$", "", lines[i]).strip()
            rhs += " " + nxt
            balance += nxt.count("[") - nxt.count("]")
        vals = re.findall(r'"([^"]+)"', rhs)
        if op == "+=":
            attrs.setdefault(attr_name, []).extend(vals)
        elif op == "=":
            attrs[attr_name] = vals
        i += 1
    return attrs


def extract_base_feature_set_deps(body):
    m = re.search(r"(?<![\w.])feature_sets\s*=\s*\[", body)
    if not m:
        return []
    i = m.end()
    while i < len(body) and body[i] != "{":
        if body[i] == "]":
            return []
        i += 1
    if i >= len(body):
        return []
    depth, start = 1, i + 1
    i += 1
    while i < len(body) and depth:
        depth += {"{": 1, "}": -1}.get(body[i], 0)
        i += 1
    first_fs = body[start:i - 1]
    if re.search(r"(?<![\w.])target_name\s*=", first_fs):
        return []
    deps = []
    for dm in re.finditer(r"(?<![\w.])deps\s*\+?=\s*\[([^\]]*)\]", first_fs):
        deps.extend(re.findall(r'"([^"]+)"', dm.group(1)))
    return deps


def canonical_dep_label(pkg_dir, dep):
    d = dep.strip().strip('"')
    if "(" in d:
        d = d[:d.index("(")]
    if d.startswith(":"):
        d = f"//:{d[1:]}" if not pkg_dir else f"//{pkg_dir}:{d[1:]}"
    elif not d.startswith("//") and not d.startswith("@"):
        d = f"//{d}" if not pkg_dir else f"//{pkg_dir}/{d}"
    if d.startswith("//") and ":" in d:
        pkg_part, target_part = d.split(":", 1)
        if target_part == pkg_part.rsplit("/", 1)[-1]:
            d = pkg_part
    return d


# Collapse redundant slashes in a GN/Bazel label (e.g. `////pkg/path` -> `//pkg/path`)
# before canonicalizing so non-canonical GN labels match canonical Bazel labels.
def normalize_dep_slashes(pkg_dir, dep):
    d = dep.strip().strip('"')
    if d.startswith("/"):
        pkg_part, sep, target_part = d.partition(":")
        pkg_part = "//" + re.sub(r"/+", "/", pkg_part.strip("/"))
        d = f"{pkg_part}:{target_part}" if sep else pkg_part
    return canonical_dep_label(pkg_dir, d)


def is_vendor_crate_match(gn_dep, bazel_dep):
    g = gn_dep.strip().strip('"')
    b = bazel_dep.strip().strip('"')
    if g.startswith("//third_party/rust_crates:") and b.startswith("//third_party/rust_crates/vendor:"):
        gc = g[len("//third_party/rust_crates:"):].replace("-", "_")
        bc = b[len("//third_party/rust_crates/vendor:"):].replace("-", "_")
        return gc == bc
    return False


# Load third_party_target_map.json and expand Bazel googletest :gtest to also cover GN :gmock
# (since Bazel @com_google_googletest//:gtest bundles both googletest and googlemock).
def load_third_party_target_map():
    tp_path = os.path.join(workdir, "build/tools/bazel2gn/third_party_target_map.json")
    raw = {}
    try:
        with open(tp_path, encoding="utf-8") as f:
            raw = json.load(f)
    except Exception:
        return {}
    out = {}
    for bz_lbl, gn_lbl in raw.items():
        g_canon = canonical_dep_label("", gn_lbl)
        covered = {g_canon}
        if g_canon == "//third_party/googletest:gtest":
            covered.add("//third_party/googletest:gmock")
        elif g_canon == "//third_party/googletest:gtest_main":
            covered.add("//third_party/googletest:gmock_main")
        out[bz_lbl.strip()] = covered
    return out


# Record non-blocking INFO attribute_alignment when GN output_name is carried via
# @bazel2gn:raw_overwrite on fx_cc_binary/cc_binary, crate_name on rustc_binary, or default name.
def check_output_name_alignment_info(bazel_rel, name, bt_rule, bt_line, bt_kws, gn_top, cur_top):
    if "output_name" not in gn_top or "output_name" in bt_kws:
        return None
    gn_out = gn_top.get("output_name", [])
    cc_bin = bt_rule in ("fx_cc_binary", "cc_binary", "cc_test", "fx_cc_test")
    rust_bin = bt_rule in ("rustc_binary", "rust_binary", "rustc_test", "rust_test")
    preserved_in_gn = bool(cur_top and cur_top.get("output_name") == gn_out)
    if cc_bin or (rust_bin and "crate_name" in bt_kws) or gn_out == [name] or preserved_in_gn:
        return {
            "source": "gn_attr_parity",
            "category": "attribute_alignment",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'Target "{name}" (Target Attributes): '
                "output_name in GN not carried over to Bazel"
            ),
        }
    return None


# Record non-blocking INFO dep_alignment when unmatched GN/Bazel deps are accounted for by
# third_party_target_map.json (including googletest gtest+gmock), rustc_library_with_features
# base feature_sets deps, non-canonical extra-slash GN labels normalized in Bazel, or implicit
# GN //sdk/lib/fdio on C/C++ binaries.
def check_dep_alignment_info(
    pkg, bazel_rel, name, bt_rule, bt_line, bt_kws, top_vars, gn_top, base_fs_deps, tp_map
):
    gn_deps = sorted(set(
        gn_top.get("deps", [])
        + gn_top.get("public_deps", [])
        + gn_top.get("link_deps", [])
        + gn_top.get("non_rust_deps", [])
        + gn_top.get("proc_macro_deps", [])
    ))
    bazel_deps = []
    for b_attr in (
        "deps",
        "implementation_deps",
        "fuchsia_deps",
        "fuchsia_implementation_deps",
        "non_fuchsia_deps",
        "link_deps",
        "proc_macro_deps",
        "data",
    ):
        bazel_deps.extend(eval_starlark(bt_kws.get(b_attr), top_vars))
    bazel_deps = sorted(set(bazel_deps))
    used_bazel = set()
    exact_matches = 0
    vendor_matches = 0
    unmatched_gn = []
    for g in gn_deps:
        g_canon = canonical_dep_label(pkg, g)
        matched = False
        for b_idx, b in enumerate(bazel_deps):
            if b_idx in used_bazel:
                continue
            b_canon = canonical_dep_label(pkg, b)
            if g_canon == b_canon or g_canon.replace("-", "_") == b_canon.replace("-", "_"):
                used_bazel.add(b_idx)
                exact_matches += 1
                matched = True
                break
        if not matched:
            for b_idx, b in enumerate(bazel_deps):
                if b_idx in used_bazel:
                    continue
                if is_vendor_crate_match(g, b):
                    used_bazel.add(b_idx)
                    vendor_matches += 1
                    matched = True
                    break
        if not matched:
            unmatched_gn.append(g)
    unmatched_bazel = [b for b_idx, b in enumerate(bazel_deps) if b_idx not in used_bazel]
    if not unmatched_gn and not unmatched_bazel:
        return None
    base_fs_canons = {
        normalize_dep_slashes(pkg, d).replace("-", "_") for d in base_fs_deps
    }
    unmatched_gn_items = [
        (g, canonical_dep_label(pkg, g), normalize_dep_slashes(pkg, g))
        for g in unmatched_gn
    ]
    unmatched_gn_canons = {g_canon for _, g_canon, _ in unmatched_gn_items}
    covered_gn_canons = set()
    for b in unmatched_bazel:
        b_norm = normalize_dep_slashes(pkg, b)
        tp_targets = tp_map.get(b, set())
        slash_or_tp_cov = {
            g_canon
            for _, g_canon, g_norm in unmatched_gn_items
            if g_canon in tp_targets
            or g_norm in tp_targets
            or g_norm.replace("-", "_") == b_norm.replace("-", "_")
            or is_vendor_crate_match(g_norm, b)
        }
        if slash_or_tp_cov:
            covered_gn_canons |= slash_or_tp_cov
        elif b_norm.replace("-", "_") in base_fs_canons:
            continue
        elif bt_rule in ("fx_cc_binary", "cc_binary") and b_norm == "//sdk/lib/fdio":
            continue
        else:
            return None
    all_bazel_norms = {
        normalize_dep_slashes(pkg, b).replace("-", "_") for b in bazel_deps
    }
    for _, g_canon, g_norm in unmatched_gn_items:
        if g_norm.replace("-", "_") in all_bazel_norms:
            covered_gn_canons.add(g_canon)
    if unmatched_gn_canons - covered_gn_canons:
        return None
    dep_notes = []
    if exact_matches > 0:
        dep_notes.append(f"{exact_matches} exact match(es)")
    if vendor_matches > 0:
        dep_notes.append(f"{vendor_matches} vendor crate mapping(s)")
    if unmatched_gn:
        dep_notes.append(f"unmatched in GN: [{', '.join(unmatched_gn)}]")
    if unmatched_bazel:
        dep_notes.append(f"unmatched in Bazel: [{', '.join(unmatched_bazel)}]")
    return {
        "source": "gn_attr_parity",
        "category": "dep_alignment",
        "severity": "info",
        "file": bazel_rel,
        "line": bt_line,
        "message": (
            f'Target "{name}" dependencies differ between GN and Bazel: '
            f'{"; ".join(dep_notes)}'
        ),
    }


THIRD_PARTY_MAP = load_third_party_target_map()


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
        if tmpl == "config":
            continue
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
            top_vars = {}
            bazel_targets = {}
            has_bazel_test_rule = False
            for stmt in bazel_tree.body:
                if isinstance(stmt, ast.Assign):
                    for tgt in stmt.targets:
                        if isinstance(tgt, ast.Name):
                            top_vars[tgt.id] = eval_starlark(stmt.value, top_vars)
                elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
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
                gn_top = parse_gn_top_attrs(body)
                cur_top = parse_gn_top_attrs(cur[2]) if cur else None
                out_info = check_output_name_alignment_info(
                    bazel_rel, name, bt_rule, bt_line, bt_kws, gn_top, cur_top
                )
                if out_info:
                    findings.append(out_info)
                base_fs_deps = (
                    extract_base_feature_set_deps(body)
                    if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library"
                    else []
                )
                dep_info = check_dep_alignment_info(
                    pkg,
                    bazel_rel,
                    name,
                    bt_rule,
                    bt_line,
                    bt_kws,
                    top_vars,
                    gn_top,
                    base_fs_deps,
                    THIRD_PARTY_MAP,
                )
                if dep_info:
                    findings.append(dep_info)
                if tmpl == "config" and bt_rule == "build_flags":
                    findings.append({
                        "source": "gn_attr_parity",
                        "category": "attribute_alignment",
                        "severity": "info",
                        "file": bazel_rel,
                        "line": bt_line,
                        "message": f'Target "{name}" (Rule & Directives): Unusual rule mapping from config to build_flags',
                    })
                    gn_rf = gn_top.get("rustflags", [])
                    bz_rf = eval_starlark(bt_kws.get("rustflags"), top_vars)
                    cfg_notes = []
                    if gn_rf and bz_rf and gn_rf != bz_rf:
                        cfg_notes.append(f"rustflags ([{' '.join(gn_rf)}]) != rustflags ([{' '.join(bz_rf)}])")
                    if "configs" in gn_top and "configs" not in bt_kws and "compiler_subflags" not in bt_kws:
                        cfg_notes.append("configs in GN not carried over to Bazel")
                    if cfg_notes:
                        findings.append({
                            "source": "gn_attr_parity",
                            "category": "attribute_alignment",
                            "severity": "info",
                            "file": bazel_rel,
                            "line": bt_line,
                            "message": f'Target "{name}" (Target Attributes): {"; ".join(cfg_notes)}',
                        })
                if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library":
                    findings.append({
                        "source": "gn_attr_parity",
                        "category": "attribute_alignment",
                        "severity": "info",
                        "file": bazel_rel,
                        "line": bt_line,
                        "message": f'Target "{name}" (Rule & Directives): Unusual rule mapping from rustc_library_with_features to rustc_library',
                    })
                    has_gn_configs = "configs" in gn_top
                    if has_gn_configs and "configs" not in bt_kws and "compiler_subflags" not in bt_kws and "lint_config" not in bt_kws:
                        extra_attrs = [
                            attr for attr in gn_top
                            if attr not in KNOWN_OR_DEDICATED_GN_ATTRS and attr not in bt_kws
                        ]
                        for cfg_note in (
                            "GN Rust configs handled by toolchain defaults / lint_config",
                            "configs in GN not carried over to Bazel",
                        ):
                            prefix_notes = [cfg_note]
                            if "with_unit_tests" in gn_top and "with_unit_tests" not in bt_kws and "with_host_unit_tests" not in bt_kws and has_bazel_test_rule:
                                prefix_notes.append("GN with_unit_tests decomposed into explicit test target")
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
