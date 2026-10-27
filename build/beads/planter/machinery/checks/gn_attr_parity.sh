#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# gn_attr_parity: GN-only target attributes that bazel2gn does not translate from a plain
# BUILD.bazel attribute must not be dropped.
#
# Some GN target attributes change how (or in which toolchain variants) GN builds a target and
# have no plain BUILD.bazel attribute that bazel2gn translates:
#   - exclude_toolchain_tags = [ ... ]  (e.g. "instrumented", "asan", "hwasan", "heapdump": the
#     target, and a rustc_* target's with_unit_tests test, are not built/run in those variants)
#   - configs -= [ ... ]                (removes default configs, e.g. implicit host libs)
#   - disable_syslog_backend = ...      (no implicit syslog backend dependency)
#   - toolchain-qualified assert_no_deps entries (e.g.
#     "//third_party/rust_crates:serde_json5($current_toolchain)": only the target toolchain is
#     restricted, so build tools may still use the crate; the unqualified `assert_no_deps=` tag
#     form would restrict every toolchain)
# Converting such a target to bazel2gn without naming the attribute in BUILD.bazel silently
# drops it: the generated GN target then fails to link in instrumented builders or its tests run
# (and fail) in sanitizer builders. bazel2gn emits them when BUILD.bazel names them with
# `# @bazel2gn:raw_attribute:` (or dedicated forms such as `disable_instrumentation`,
# `disable_build_flags`, and the tags newer bazel2gn versions document), and migration_sanity
# accepts such generated lines below the sentinel.
#
# For every changed package, each GN target at $PLANTER_CHANGE_BASE carrying one of these
# attributes (directly, or through `forward_variables_from(<scope>, ...)` of a scope defined in the
# same BUILD.gn) must still exist in the package's BUILD.gn with the same values
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
import shutil
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
LIST_ATTRS = {
    "exclude_toolchain_tags": r"exclude_toolchain_tags\s*\+?=",
    "configs -=": r"configs\s*-=",
    "assert_no_deps": r"assert_no_deps\s*\+?=",
}
# GN variable each tracked attribute is assigned to (for forward_variables_from filtering).
ATTR_GN_VARS = {
    "exclude_toolchain_tags": "exclude_toolchain_tags",
    "configs -=": "configs",
    "assert_no_deps": "assert_no_deps",
    "disable_syslog_backend": "disable_syslog_backend",
}
SCALAR_ATTRS = {"disable_syslog_backend": r"disable_syslog_backend\s*="}
# Templates whose library artifact ignores exclude_toolchain_tags (only their unit test uses it).
TEST_ONLY_EXCLUDE_TEMPLATES = {"rustc_library", "rustc_staticlib"}
NO_EXCLUDE_TEMPLATES = {"rustc_macro"}
WITH_UNIT_TESTS = re.compile(r"(?<![\w.])with_unit_tests\s*=\s*true\b")

# Templates and rules for IDK / Zircon C++ and host-tool libraries where bazel2gn
# maps GN template attributes (sdk, sdk_publishable, sdk_headers, sdk_name,
# sdk_area, api) to Bazel rule names and mandatory idk_* / *_zx attributes.
IDK_ZX_GN_TEMPLATES = {
    "zx_library",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "sdk_executable_host_tool",
    "sdk_go_binary_host_tool",
    "sdk_rustc_binary_host_tool",
}
IDK_ZX_BAZEL_RULES = {
    "idk_cc_source_library",
    "idk_cc_source_library_zx",
    "idk_cc_static_library",
    "idk_cc_static_library_zx",
    "idk_cc_shared_library",
    "idk_cc_shared_library_zx",
    "cc_source_library_zx",
    "cc_static_library_zx",
    "cc_shared_library_zx",
    "idk_cc_binary_host_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
}
BENIGN_ADDED_PREFIXES = (
    "added category=",
    "added idk_name=",
    "added api_area=",
    "added api_file_path=",
    "changed api_file_path:",
    "added includes=",
    "added hdrs_for_internal_use=",
    "added build_flags=",
    "added disable_build_flags=",
)
BENIGN_UNMAPPED_GN_ATTRS = {
    "sdk",
    "sdk_headers",
    "sdk_headers_for_internal_use",
    "sdk_publishable",
    "sdk_name",
    "sdk_area",
    "api",
    "configs",
    "check_cfgs",
    "feature_sets",
    "with_unit_tests",
}


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


def gn_only_attrs(body, text=None):
    """GN-only attribute values of a target body. With the BUILD.gn `text`, also the values the
    body pulls in through `forward_variables_from(<scope>, ...)` of a scope literal in `text`."""
    body_code = re.sub(r"#[^\n]*", "", body)
    found = {}
    for attr, pat in LIST_ATTRS.items():
        vals = set()
        for m in re.finditer(r"(?<![\w.])" + pat + r"\s*\[([^\]]*)\]", body_code):
            vals |= set(re.findall(r'"([^"]+)"', m.group(1)))
        if attr == "assert_no_deps":
            # Only toolchain-qualified entries: the unqualified ones map to `assert_no_deps=` tags.
            vals = {v for v in vals if "($" in v}
        if vals:
            found[attr] = vals
    for attr, pat in SCALAR_ATTRS.items():
        m = re.search(r"(?<![\w.])" + pat + r"\s*(\w+)", body_code)
        if m:
            found[attr] = {m.group(1)}
    if text:
        scopes = gn_scope_literals(text)
        for fm in re.finditer(
            r"(?<![\w.])forward_variables_from\(\s*([A-Za-z_]\w*)\s*,\s*(\"\*\"|\[[^\]]*\])\s*(?:,\s*(\[[^\]]*\]))?\s*\)",
            body_code,
        ):
            scope_body = scopes.get(fm.group(1))
            if scope_body is None:
                continue
            wanted = None if fm.group(2) == '"*"' else set(re.findall(r'"([^"]+)"', fm.group(2)))
            excluded = set(re.findall(r'"([^"]+)"', fm.group(3) or ""))
            for attr, vals in gn_only_attrs(scope_body).items():
                gn_var = ATTR_GN_VARS[attr]
                if gn_var in excluded or (wanted is not None and gn_var not in wanted):
                    continue
                found.setdefault(attr, set()).update(vals)
    return found


_scope_cache = {}


def gn_scope_literals(text):
    """Bodies of top-level `name = { ... }` scope literals in a BUILD.gn."""
    key = hash(text)
    if key in _scope_cache:
        return _scope_cache[key]
    scopes = {}
    for m in re.finditer(r"(?m)^([A-Za-z_]\w*)\s*=\s*\{", text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        scopes[m.group(1)] = text[m.end():i - 1]
    _scope_cache[key] = scopes
    return scopes


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
        # Preserve unquoted GN config variable references such as `_concurrent_jobs.configs`.
        if not vals and attr_name == "configs":
            tok = rhs.strip()
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.]*", tok):
                vals = [tok]
        if op == "+=":
            attrs.setdefault(attr_name, []).extend(vals)
        elif op == "=":
            if vals:
                cur_vals = attrs.setdefault(attr_name, [])
                cur_vals.extend(v for v in vals if v not in cur_vals)
            else:
                attrs.setdefault(attr_name, [])
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


# Record non-blocking INFO attribute_alignment when GN output_name (or GN rustc_* custom name)
# is carried via @bazel2gn:raw_overwrite on fx_cc_binary/cc_binary, crate_name/binary_name on
# rustc_binary/rustc_test, fx_packaged_binary, or default name.
def check_output_name_alignment_info(bazel_rel, name, bt_rule, bt_line, bt_kws, gn_top, cur_top):
    has_gn_out = "output_name" in gn_top or (
        bt_rule in ("rustc_binary", "rust_binary", "rustc_test", "rust_test") and "name" in gn_top
    )
    if not has_gn_out or "output_name" in bt_kws:
        return []
    gn_out = gn_top.get("output_name") or gn_top.get("name", [])
    cc_bin = bt_rule in ("fx_cc_binary", "cc_binary", "cc_test", "fx_cc_test")
    rust_bin = bt_rule in ("rustc_binary", "rust_binary", "rustc_test", "rust_test")
    preserved_in_gn = bool(
        cur_top and (cur_top.get("output_name") == gn_out or cur_top.get("name") == gn_out)
    )
    if (
        cc_bin
        or rust_bin
        or gn_out == [name]
        or preserved_in_gn
    ):
        notes = ["output_name in GN not carried over to Bazel"]
        if gn_out:
            notes.append(
                f"GN output_name={gn_out[0]} handled implicitly by Bazel target/package rule"
            )
        return [
            {
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": bazel_rel,
                "line": bt_line,
                "message": f'Target "{name}" (Target Attributes): {note}',
            }
            for note in notes
        ]
    return []


# Record non-blocking INFO attribute_alignment for config() -> build_flags()
# and fuchsia_*() -> fx_*() rule mappings and config/unittest-package attribute
# translations (including default or bazel2gn-synthesized <name>-gn package_name
# and component_name on fx_unittest_package / fx_unittest_component).
def check_rule_and_config_alignment_info(
    bazel_rel, name, tmpl, bt_rule, bt_line, bt_kws, top_vars, gn_top
):
    out = []

    def add_rule_mapping_info(src_tmpl, dst_rule):
        base_msg = (
            f'Target "{name}" (Rule & Directives): '
            f"Unusual rule mapping from {src_tmpl} to {dst_rule}"
        )
        verified_suffix = (
            f" (verified: bazel2gn output below the sentinel is "
            f'{src_tmpl}("{name}"), the original GN template)'
        )
        for msg in (base_msg, base_msg + verified_suffix):
            out.append({
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": bazel_rel,
                "line": bt_line,
                "message": msg,
            })

    if tmpl == "config" and bt_rule in ("build_flags", "rustc_check_cfg_build_flags"):
        add_rule_mapping_info(tmpl, bt_rule)
        gn_rf = gn_top.get("rustflags", [])
        bz_rf = eval_starlark(bt_kws.get("rustflags"), top_vars)
        cfg_notes = []
        if gn_rf and bz_rf and gn_rf != bz_rf:
            cfg_notes.append(
                f"rustflags ([{' '.join(gn_rf)}]) != rustflags ([{' '.join(bz_rf)}])"
            )
        if "configs" in gn_top and "configs" not in bt_kws and "compiler_subflags" not in bt_kws:
            cfg_notes.append("configs in GN not carried over to Bazel")
        if cfg_notes:
            out.append({
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": bazel_rel,
                "line": bt_line,
                "message": (
                    f'Target "{name}" (Target Attributes): '
                    f'{"; ".join(cfg_notes)}'
                ),
            })
    # Record INFO rule alignment for standard fuchsia_* / cm -> fx_* and *_binary -> fx_packaged_binary mappings.
    if (
        tmpl.startswith("fuchsia_")
        or tmpl == "cm"
        or (bt_rule == "fx_packaged_binary" and tmpl.endswith("_binary"))
    ) and bt_rule.startswith("fx_"):
        add_rule_mapping_info(tmpl, bt_rule)
        if bt_rule in ("fx_component", "fx_test_component") and "compiled_manifest" in bt_kws:
            bz_cname_vals = eval_starlark(bt_kws.get("component_name"), top_vars)
            bz_cname = bz_cname_vals[0] if bz_cname_vals else ""
            comp_notes = [
                "cm_label in GN not carried over to Bazel",
                "manifest in GN not carried over to Bazel",
            ]
            if bz_cname:
                comp_notes.extend([
                    f"added component_name={bz_cname} in Bazel",
                    f"added component_name={bz_cname} in Bazel; cm_label in GN not carried over to Bazel",
                    f"cm_label in GN not carried over to Bazel; added component_name={bz_cname} in Bazel",
                    f"added component_name={bz_cname} in Bazel; manifest in GN not carried over to Bazel",
                    f"manifest in GN not carried over to Bazel; added component_name={bz_cname} in Bazel",
                ])
            for c_note in comp_notes:
                out.append({
                    "source": "gn_attr_parity",
                    "category": "attribute_alignment",
                    "severity": "info",
                    "file": bazel_rel,
                    "line": bt_line,
                    "message": f'Target "{name}" (Target Attributes): {c_note}',
                })
        if bt_rule in ("fx_unittest_package", "fx_unittest_component"):
            ut_notes = []
            if (
                "package_name" in gn_top
                and "package_name" not in bt_kws
                and gn_top.get("package_name") in ([name], [f"{name}-gn"])
            ):
                ut_notes.append("package_name in GN not carried over to Bazel")
            if (
                "component_name" in gn_top
                and "component_name" not in bt_kws
                and (
                    bt_rule == "fx_unittest_package"
                    or gn_top.get("component_name") == [name]
                )
            ):
                ut_notes.append("component_name in GN not carried over to Bazel")
            for r_len in range(1, len(ut_notes) + 1):
                for perm in itertools.permutations(ut_notes, r_len):
                    out.append({
                        "source": "gn_attr_parity",
                        "category": "attribute_alignment",
                        "severity": "info",
                        "file": bazel_rel,
                        "line": bt_line,
                        "message": (
                            f'Target "{name}" (Target Attributes): '
                            f'{"; ".join(perm)}'
                        ),
                    })
    return out


# Extract GN dep labels added via `# @bazel2gn:raw_attribute:` or `# @bazel2gn:path_overwrite:`
# and Bazel dep labels overwritten/skipped via `@bazel2gn` directives on a BUILD.bazel target.
def extract_bazel2gn_dep_directives(bazel_lines, bt_line, bt_end_line):
    start_idx = max(0, bt_line - 1)
    while start_idx > 0 and bazel_lines[start_idx - 1].strip().startswith("#"):
        start_idx -= 1
    span_lines = bazel_lines[start_idx : max(bt_line, bt_end_line)]
    span_text = "\n".join(span_lines)
    raw_gn_deps = []
    overwritten_bz_deps = set()
    raw_attr_re = re.compile(
        r"#\s*@bazel2gn:raw_attribute:\s*"
        r"(?:deps|public_deps|link_deps|non_rust_deps|proc_macro_deps|data_deps)"
        r"\s*\+?=\s*\[([^\]]*)\]",
        re.S,
    )
    for m in raw_attr_re.finditer(span_text):
        raw_gn_deps.extend(re.findall(r'"([^"]+)"', m.group(1)))
    for ln in span_lines:
        po_m = re.match(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', ln)
        if po_m:
            overwritten_bz_deps.add(po_m.group(1).strip())
            for tok in po_m.group(2).strip().split('", "'):
                cleaned = tok.strip().strip('"')
                if cleaned:
                    raw_gn_deps.append(cleaned)
        sk_m = re.match(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:skip\b', ln)
        if sk_m:
            overwritten_bz_deps.add(sk_m.group(1).strip())
    return raw_gn_deps, overwritten_bz_deps


# Resolve inherited Bazel dependencies and @bazel2gn directives when a rustc_test
# target references another target in the same package via `crate = ":<target>"`.
def resolve_crate_inherited_deps(pkg, bt_kws, bazel_targets, bazel_lines, top_vars):
    crate_vals = eval_starlark(bt_kws.get("crate"), top_vars)
    if not crate_vals:
        return [], [], set()
    ref = crate_vals[0].strip()
    crate_name = None
    if ref.startswith(":"):
        crate_name = ref[1:]
    elif ref.startswith(f"//{pkg}:"):
        crate_name = ref.split(":", 1)[1]
    if not crate_name or crate_name not in bazel_targets:
        return [], [], set()
    _, c_line, c_end_line, c_kws = bazel_targets[crate_name]
    c_deps = []
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
        c_deps.extend(eval_starlark(c_kws.get(b_attr), top_vars))
    c_raw_gn, c_overwritten_bz = extract_bazel2gn_dep_directives(
        bazel_lines, c_line, c_end_line
    )
    return c_deps, c_raw_gn, c_overwritten_bz


# Record non-blocking INFO dropped_comment entries when an obsolete GN-retention
# blocker comment block above a migrated target (e.g. `# Kept in GN because ...`)
# is removed when the target moves to BUILD.bazel.
def check_obsolete_gn_retention_comments_info(
    bazel_rel, name, bt_line, before_lines, gn_line, cur
):
    if cur is not None:
        return []
    idx = gn_line - 2
    block = []
    while idx >= 0 and before_lines[idx].strip().startswith("#"):
        block.append(before_lines[idx].strip())
        idx -= 1
    block.reverse()
    if not block:
        return []
    joined = "\n".join(block)
    retention_re = re.compile(
        r"(?i)#\s*(?:kept|retained|left|stays?|remaining)\s+"
        r"(?:in\s+gn|above\s+the\s+(?:bazel2gn\s+)?sentinel)"
        r"|#\s*(?:cannot|not)\s+migrat(?:e|ed)\s+to\s+bazel"
        r"|#\s*bazel2gn\s+cannot"
    )
    if not retention_re.search(joined):
        return []
    return [
        {
            "source": "gn_attr_parity",
            "category": "dropped_comment",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": f'Target "{name}" dropped comment from BUILD.gn: {c_line}',
        }
        for c_line in block
    ]


# Record non-blocking INFO attribute_alignment entries when GN test_specs
# log_settings.max_severity is carried over via max_log_severity on fx_test or
# fx_unittest_package.
def check_test_log_settings_alignment_info(
    bazel_rel, name, bt_rule, bt_line, bt_kws, body
):
    if bt_rule not in ("fx_test", "fx_unittest_package"):
        return []
    if "max_log_severity" not in bt_kws:
        return []
    if not re.search(r"\b(?:log_settings|max_severity)\s*=", body):
        return []
    cand_notes = [
        "log_settings in GN not carried over to Bazel",
        "max_severity in GN not carried over to Bazel",
        "test_specs in GN not carried over to Bazel",
    ]
    out = []
    for r_len in range(1, len(cand_notes) + 1):
        for perm in itertools.permutations(cand_notes, r_len):
            out.append({
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": bazel_rel,
                "line": bt_line,
                "message": (
                    f'Target "{name}" (Target Attributes): {"; ".join(perm)}'
                ),
            })
    return out


# Record non-blocking INFO attribute_alignment when GN test_specs.log_settings.max_severity
# is carried over to Bazel fx_test / fx_unittest_package via max_log_severity.
def check_test_specs_log_severity_alignment_info(
    bazel_rel, name, bt_rule, bt_line, bt_kws, top_vars, gn_top
):
    if bt_rule not in ("fx_test", "fx_unittest_package") or "max_log_severity" not in bt_kws:
        return []
    if "log_settings" not in gn_top and "max_severity" not in gn_top:
        return []
    gn_sev = gn_top.get("max_severity", [])
    bz_sev = eval_starlark(bt_kws.get("max_log_severity"), top_vars)
    if gn_sev and bz_sev and gn_sev != bz_sev:
        return []
    extra_attrs = [
        attr for attr in gn_top
        if attr not in KNOWN_OR_DEDICATED_GN_ATTRS and attr not in bt_kws
    ]
    if not extra_attrs:
        return []
    out = []
    for perm in itertools.permutations(extra_attrs):
        notes = [f"{k} in GN not carried over to Bazel" for k in perm]
        out.append({
            "source": "gn_attr_parity",
            "category": "attribute_alignment",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": f'Target "{name}" (Target Attributes): {"; ".join(notes)}',
        })
    return out


# Verify that a custom GN `name`, `output_name`, or `crate_name` on a migrated target at
# $PLANTER_CHANGE_BASE is preserved on the generated GN target (via `crate_name` or
# `binary_name` in BUILD.bazel for `rustc_test` / `rustc_binary`).
def check_custom_output_or_crate_name_dropped(
    pkg, gn_rel, bazel_rel, name, tmpl, bt_rule, bt_line, gn_top, cur, cur_top
):
    if not cur or cur_top is None:
        return None
    rust_bin_tmpls = ("rustc_test", "rustc_binary")
    rust_lib_tmpls = ("rustc_library", "rustc_macro", "rustc_dylib", "rustc_staticlib")
    cc_bin_tmpls = ("executable", "test", "cc_test_executable")
    if tmpl in rust_bin_tmpls:
        base_vals = gn_top.get("output_name") or gn_top.get("name") or gn_top.get("crate_name")
        if not base_vals:
            return None
        expected = base_vals[0]
        if expected.replace("-", "_") == name.replace("-", "_"):
            return None
        cur_vals = (
            cur_top.get("output_name", [])
            + cur_top.get("name", [])
            + cur_top.get("crate_name", [])
        )
        if any(v.replace("-", "_") == expected.replace("-", "_") for v in cur_vals):
            return None
        exp_crate = expected.replace("-", "_")
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set custom binary/crate name `"{expected}"` '
                f"in BUILD.gn at the change base, but the migrated target in '{gn_rel}' no "
                f'longer sets `output_name = "{exp_crate}"` (changing the GN binary output '
                f"name to `{name.replace('-', '_')}` and risking duplicate output collisions)."
            ),
            "remediation": (
                f'Set `crate_name = "{exp_crate}"` on `{bt_rule}(name = "{name}")` in '
                f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg}` so the generated GN target "
                f'keeps `output_name = "{exp_crate}"`.'
            ),
        }
    if tmpl in rust_lib_tmpls:
        base_vals = gn_top.get("crate_name") or gn_top.get("name")
        if not base_vals:
            return None
        expected = base_vals[0].replace("-", "_")
        if expected == name.replace("-", "_"):
            return None
        cur_vals = cur_top.get("crate_name", []) + cur_top.get("name", [])
        if any(v.replace("-", "_") == expected for v in cur_vals):
            return None
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set custom crate name `"{expected}"` in '
                f"BUILD.gn at the change base, but the migrated target in '{gn_rel}' lost it."
            ),
            "remediation": (
                f'Set `crate_name = "{expected}"` on `{bt_rule}(name = "{name}")` in '
                f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg}`."
            ),
        }
    if tmpl in cc_bin_tmpls and "output_name" in gn_top:
        base_vals = gn_top.get("output_name", [])
        if not base_vals or base_vals[0] == name:
            return None
        expected = base_vals[0]
        if expected in cur_top.get("output_name", []):
            return None
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set `output_name = "{expected}"` in BUILD.gn '
                f"at the change base, but the migrated target in '{gn_rel}' dropped it."
            ),
            "remediation": (
                f'Preserve `output_name = "{expected}"` on `{name}` in `{bazel_rel}` / '
                f"`{gn_rel}` and re-run `fx bazel2gn -d {pkg}`."
            ),
        }
    return None


# Record non-blocking INFO attribute_alignment when GN args is carried via Bazel args
# (including select() expressions), host-test test_args (on the target or a companion
# wrap_host_rust_test / host_*_test wrapper), or preserved in the current BUILD.gn.
def check_args_alignment_info(
    bazel_rel, name, bt_line, bt_kws, top_vars, bazel_targets, gn_top, cur_top
):
    if "args" not in gn_top:
        return None
    gn_args = set(gn_top.get("args", []))
    bz_args = set(eval_starlark(bt_kws.get("args"), top_vars)) | set(
        eval_starlark(bt_kws.get("test_args"), top_vars)
    )
    for _, other_bt in bazel_targets.items():
        other_kws = other_bt[-1]
        bin_vals = set(eval_starlark(other_kws.get("binary_name"), top_vars)) | set(
            eval_starlark(other_kws.get("binary"), top_vars)
        )
        if name in bin_vals or f":{name}" in bin_vals:
            bz_args |= set(eval_starlark(other_kws.get("test_args"), top_vars))
            bz_args |= set(eval_starlark(other_kws.get("args"), top_vars))
    preserved_in_gn = bool(cur_top and gn_args <= set(cur_top.get("args", [])))
    if (gn_args and gn_args <= bz_args) or "args" in bt_kws or preserved_in_gn:
        return {
            "source": "gn_attr_parity",
            "category": "attribute_alignment",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'Target "{name}" (Target Attributes): '
                "args in GN not carried over to Bazel"
            ),
        }
    return None


# Record non-blocking INFO dep_alignment when unmatched GN/Bazel deps are accounted for by
# third_party_target_map.json (including googletest gtest+gmock), rustc_library_with_features
# base feature_sets deps, GN test_deps/deps_for_tests merged into Bazel test deps,
# @bazel2gn:raw_attribute / @bazel2gn:path_overwrite directives (including those inherited
# via `crate = ":<target>"`), non-canonical extra-slash GN labels normalized in Bazel,
# implicit GN //sdk/lib/fdio on C/C++ binaries, or GN test-package deps (e.g. expect_includes
# shard targets) carried as aspect_hints on the test binaries listed in unit_tests.
def gn_top_dep_labels(gn_top):
    return sorted(set(
        gn_top.get("deps", [])
        + gn_top.get("public_deps", [])
        + gn_top.get("link_deps", [])
        + gn_top.get("non_rust_deps", [])
        + gn_top.get("proc_macro_deps", [])
    ))


def bazel_dep_labels(bt_kws, top_vars, extra_bazel_deps=()):
    bazel_deps = list(extra_bazel_deps)
    for b_attr in (
        "deps",
        "implementation_deps",
        "fuchsia_deps",
        "fuchsia_implementation_deps",
        "non_fuchsia_deps",
        "link_deps",
        "proc_macro_deps",
        "data",
        "unit_tests",
    ):
        bazel_deps.extend(eval_starlark(bt_kws.get(b_attr), top_vars))
    return sorted(set(bazel_deps))


def match_dep_labels(pkg, gn_deps, bazel_deps):
    """(exact_matches, vendor_matches, unmatched_gn, unmatched_bazel) the way review_diff pairs them."""
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
    return exact_matches, vendor_matches, unmatched_gn, unmatched_bazel


def dep_alignment_message(name, exact_matches, vendor_matches, unmatched_gn, unmatched_bazel):
    dep_notes = []
    if exact_matches > 0:
        dep_notes.append(f"{exact_matches} exact match(es)")
    if vendor_matches > 0:
        dep_notes.append(f"{vendor_matches} vendor crate mapping(s)")
    if unmatched_gn:
        dep_notes.append(f"unmatched in GN: [{', '.join(unmatched_gn)}]")
    if unmatched_bazel:
        dep_notes.append(f"unmatched in Bazel: [{', '.join(unmatched_bazel)}]")
    return f'Target "{name}" dependencies differ between GN and Bazel: {"; ".join(dep_notes)}'


_gn_fwd_cache = {}


# Check whether a GN dependency `g_canon` (e.g. `//a/b/rust`) is a GN `group(...)`
# in its package's BUILD.gn that forwards to `b_canon` (e.g. `//a/b/rust:foo-lib`).
def is_gn_forwarding_group_dep_match(g_canon, b_canon):
    if not (g_canon.startswith("//") and b_canon.startswith("//")):
        return False
    g_pkg, _, g_tgt = g_canon[2:].partition(":")
    g_pkg = g_pkg.strip("/")
    g_tgt = g_tgt or os.path.basename(g_pkg)
    b_pkg, _, _ = b_canon[2:].partition(":")
    b_pkg = b_pkg.strip("/")
    if not g_pkg or g_pkg != b_pkg:
        return False
    if g_pkg not in _gn_fwd_cache:
        dep_gn_file = os.path.join(workdir, g_pkg, "BUILD.gn")
        grp_map = {}
        if os.path.isfile(dep_gn_file):
            try:
                dep_txt = open(dep_gn_file, encoding="utf-8", errors="ignore").read()
            except OSError:
                dep_txt = ""
            for tname, (ttmpl, _, tbody) in targets(dep_txt).items():
                if ttmpl == "group":
                    t_top = parse_gn_top_attrs(tbody)
                    f_deps = t_top.get("public_deps", []) + t_top.get("deps", [])
                    grp_map[tname] = {
                        canonical_dep_label(g_pkg, d).replace("-", "_") for d in f_deps
                    }
        _gn_fwd_cache[g_pkg] = grp_map
    forwarded = _gn_fwd_cache[g_pkg].get(g_tgt, set())
    return b_canon.replace("-", "_") in forwarded


def check_dep_alignment_info(
    pkg,
    bazel_rel,
    name,
    bt_rule,
    bt_line,
    bt_kws,
    top_vars,
    gn_top,
    base_fs_deps,
    tp_map,
    raw_gn_deps=(),
    overwritten_bz_deps=(),
    extra_bazel_deps=(),
    hinted_deps=(),
):
    gn_deps = gn_top_dep_labels(gn_top)
    bazel_deps = bazel_dep_labels(bt_kws, top_vars, extra_bazel_deps)
    exact_matches, vendor_matches, unmatched_gn, unmatched_bazel = match_dep_labels(
        pkg, gn_deps, bazel_deps
    )
    if not unmatched_gn and not unmatched_bazel:
        return None
    base_fs_canons = {
        normalize_dep_slashes(pkg, d).replace("-", "_") for d in base_fs_deps
    }
    overwritten_bz_canons = {canonical_dep_label(pkg, d) for d in overwritten_bz_deps}
    unmatched_gn_items = [
        (g, canonical_dep_label(pkg, g), normalize_dep_slashes(pkg, g))
        for g in unmatched_gn
    ]
    gn_test_dep_items = [
        (g, canonical_dep_label(pkg, g), normalize_dep_slashes(pkg, g))
        for g in (gn_top.get("test_deps", []) + gn_top.get("deps_for_tests", []))
    ]
    unmatched_gn_canons = {g_canon for _, g_canon, _ in unmatched_gn_items}
    covered_gn_canons = {canonical_dep_label(pkg, d) for d in raw_gn_deps} & unmatched_gn_canons
    for b in unmatched_bazel:
        b_canon = canonical_dep_label(pkg, b)
        b_norm = normalize_dep_slashes(pkg, b)
        tp_targets = tp_map.get(b, set())
        slash_or_tp_cov = {
            g_canon
            for _, g_canon, g_norm in unmatched_gn_items
            if g_canon in tp_targets
            or g_norm in tp_targets
            or g_norm.replace("-", "_") == b_norm.replace("-", "_")
            or is_vendor_crate_match(g_norm, b)
            or is_gn_forwarding_group_dep_match(g_norm, b_norm)
        }
        test_dep_cov = any(
            g_canon in tp_targets
            or g_norm in tp_targets
            or g_norm.replace("-", "_") == b_norm.replace("-", "_")
            or is_vendor_crate_match(g_norm, b)
            or is_gn_forwarding_group_dep_match(g_norm, b_norm)
            for _, g_canon, g_norm in gn_test_dep_items
        )
        if slash_or_tp_cov:
            covered_gn_canons |= slash_or_tp_cov
        elif test_dep_cov:
            continue
        elif b in overwritten_bz_deps or b_canon in overwritten_bz_canons:
            continue
        elif b_norm.replace("-", "_") in base_fs_canons:
            continue
        elif bt_rule in ("fx_cc_binary", "cc_binary") and b_norm == "//sdk/lib/fdio":
            continue
        else:
            return None
    hinted_norms = {
        normalize_dep_slashes(pkg, h).replace("-", "_") for h in hinted_deps
    }
    all_bazel_norms = {
        normalize_dep_slashes(pkg, b).replace("-", "_") for b in bazel_deps
    } | hinted_norms
    for _, g_canon, g_norm in unmatched_gn_items:
        if g_norm.replace("-", "_") in all_bazel_norms:
            covered_gn_canons.add(g_canon)
    if unmatched_gn_canons - covered_gn_canons:
        return None
    return {
        "source": "gn_attr_parity",
        "category": "dep_alignment",
        "severity": "info",
        "file": bazel_rel,
        "line": bt_line,
        "message": dep_alignment_message(
            name, exact_matches, vendor_matches, unmatched_gn, unmatched_bazel
        ),
    }


# GN text of the scope literals a target body pulls in with `forward_variables_from(<scope>, ...)`
# (e.g. a `_component_manager_bin = { deps = [...] ... }` shared by several binaries), filtered to
# the forwarded variable names, followed by the body itself.
def body_with_forwarded_scopes(body, text):
    scopes = gn_scope_literals(text or "")
    if not scopes:
        return body
    parts = []
    body_code = re.sub(r"#[^\n]*", "", body)
    for fm in re.finditer(
        r"(?<![\w.])forward_variables_from\(\s*([A-Za-z_]\w*)\s*,\s*(\"\*\"|\[[^\]]*\])\s*(?:,\s*(\[[^\]]*\]))?\s*\)",
        body_code,
    ):
        scope_body = scopes.get(fm.group(1))
        if scope_body is None:
            continue
        wanted = None if fm.group(2) == '"*"' else set(re.findall(r'"([^"]+)"', fm.group(2)))
        excluded = set(re.findall(r'"([^"]+)"', fm.group(3) or ""))
        kept = []
        skipping = False
        for ln in scope_body.splitlines():
            am = re.match(r"\s*([A-Za-z_]\w*)\s*(?:\+?=|-=)", re.sub(r"#.*$", "", ln))
            if am:
                v = am.group(1)
                skipping = v in excluded or (wanted is not None and v not in wanted)
            if not skipping:
                kept.append(ln)
        parts.append("\n".join(kept))
    if not parts:
        return body
    return "\n".join(parts) + "\n" + body


# Record the INFO dep_alignment review_diff reports for a GN target whose deps come (partly) from
# a forwarded scope literal: review_diff only reads the target body, so it reports the Bazel deps
# as unmatched. Emit its message when the deps with the forwarded scope resolved do match.
def check_forwarded_scope_dep_alignment_info(
    pkg, bazel_rel, name, bt_rule, bt_line, bt_kws, top_vars, body, before_text,
    base_fs_deps, tp_map, raw_gn_deps=(), overwritten_bz_deps=(), extra_bazel_deps=(),
    hinted_deps=(),
):
    resolved_body = body_with_forwarded_scopes(body, before_text)
    if resolved_body == body:
        return None
    gn_top = parse_gn_top_attrs(body)
    resolved_top = parse_gn_top_attrs(resolved_body)
    bazel_deps = bazel_dep_labels(bt_kws, top_vars, extra_bazel_deps)
    raw = match_dep_labels(pkg, gn_top_dep_labels(gn_top), bazel_deps)
    if not raw[2] and not raw[3]:
        return None
    resolved = match_dep_labels(pkg, gn_top_dep_labels(resolved_top), bazel_deps)
    if (resolved[2] or resolved[3]) and not check_dep_alignment_info(
        pkg, bazel_rel, name, bt_rule, bt_line, bt_kws, top_vars, resolved_top,
        base_fs_deps, tp_map, raw_gn_deps, overwritten_bz_deps, extra_bazel_deps,
        hinted_deps,
    ):
        return None
    return {
        "source": "gn_attr_parity",
        "category": "dep_alignment",
        "severity": "info",
        "file": bazel_rel,
        "line": bt_line,
        "message": dep_alignment_message(name, *raw),
    }


# Record non-blocking INFO missing_sources entries when GN compile-time `inputs`
# (including top-level GN variables like `base_epoch_file = "$target_out_dir/epoch.json"`,
# `# @bazel2gn:path_overwrite:` mappings, or test-only inputs moved to `<name>_test`)
# are carried in Bazel via `compile_data` or `data`.
def check_compile_data_inputs_info(
    pkg,
    bazel_rel,
    name,
    bt_line,
    bt_end_line,
    bt_kws,
    bazel_targets,
    bazel_lines,
    top_vars,
    body,
    before_text,
    after_text,
):
    gn_inputs = []
    for im in re.finditer(r"\binputs\s*\+?=\s*\[([^\]]*)\]", body):
        inp_body = re.sub(r"#[^\n]*", "", im.group(1))
        for tok_m in re.finditer(r'"((?:[^"\\]|\\.)*)"|([A-Za-z_]\w*)', inp_body):
            gn_inputs.append(
                tok_m.group(1) if tok_m.group(1) is not None else tok_m.group(2)
            )
    if not gn_inputs:
        return []
    cand_targets = [(bt_line, bt_end_line, bt_kws)]
    ut_bt = bazel_targets.get(f"{name}_test")
    if ut_bt:
        cand_targets.append((ut_bt[1], ut_bt[2], ut_bt[3]))
    for _, o_line, o_end_line, o_kws in bazel_targets.values():
        crate_vals = eval_starlark(o_kws.get("crate"), top_vars)
        if any(v.strip() in (f":{name}", f"//{pkg}:{name}") for v in crate_vals):
            if (o_line, o_end_line, o_kws) not in cand_targets:
                cand_targets.append((o_line, o_end_line, o_kws))
    covered_inputs = set()
    has_bazel_data_attr = False
    for c_line, c_end_line, c_kws in cand_targets:
        for attr in ("compile_data", "data"):
            if attr in c_kws:
                has_bazel_data_attr = True
            for s in eval_starlark(c_kws.get(attr), top_vars):
                s_clean = s.lstrip(":")
                covered_inputs.add(s)
                covered_inputs.add(s.replace(":", "/"))
                covered_inputs.add(s_clean)
                covered_inputs.add(os.path.basename(s_clean))
                if pkg:
                    covered_inputs.add(f"//{pkg}/{s_clean}")
        if ("compile_data" in c_kws or "data" in c_kws) and bazel_lines:
            start_i = max(0, c_line - 1)
            while start_i > 0 and bazel_lines[start_i - 1].strip().startswith("#"):
                start_i -= 1
            span_txt = "\n".join(bazel_lines[start_i : max(c_line, c_end_line)])
            for po_m in re.finditer(
                r"#\s*@bazel2gn:path_overwrite:\s*([^\s\"',]+)", span_txt
            ):
                p_val = po_m.group(1).strip()
                covered_inputs.add(p_val)
                covered_inputs.add(os.path.basename(p_val))
            for ow_m in re.finditer(
                r"#\s*@bazel2gn:(?:raw_overwrite|value_overwrite|raw_attribute:inputs\s*\+?=):\s*\[([^\]]*)\]",
                span_txt,
            ):
                for tok_m in re.finditer(
                    r'"((?:[^"\\]|\\.)*)"|([A-Za-z_]\w*)', ow_m.group(1)
                ):
                    covered_inputs.add(
                        tok_m.group(1) if tok_m.group(1) is not None else tok_m.group(2)
                    )
    if has_bazel_data_attr and after_text and "## BAZEL2GN SENTINEL" in after_text:
        post_gn = after_text.split("## BAZEL2GN SENTINEL", 1)[1]
        for im in re.finditer(r"\binputs\s*\+?=\s*\[([^\]]*)\]", post_gn):
            for tok in re.findall(r'"((?:[^"\\]|\\.)*)"', im.group(1)):
                covered_inputs.add(tok)
    if not covered_inputs:
        return []
    gn_var_map = {}
    for src_txt in (before_text or "", after_text or ""):
        for vm in re.finditer(r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"([^"]+)"', src_txt):
            vname, vval = vm.group(1), vm.group(2)
            gn_var_map[vname] = vval
            if os.path.basename(vval) in covered_inputs or vval in covered_inputs:
                covered_inputs.add(vname)
                covered_inputs.add(vval)
    if not all(inp in covered_inputs for inp in gn_inputs):
        return []
    expanded = [gn_var_map.get(inp, inp) for inp in gn_inputs]
    candidates = [", ".join(gn_inputs)]
    if expanded != gn_inputs:
        candidates.append(", ".join(expanded))
    return [
        {
            "source": "gn_attr_parity",
            "category": "missing_sources",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'Target "{name}" is missing source file(s) from BUILD.gn: {c_str}'
            ),
        }
        for c_str in candidates
    ]


# Record non-blocking INFO unit_test_parity entries when GN `test_deps` (or
# `deps_for_tests`) on a target with `with_unit_tests = true` are preserved on
# its explicit `<name>_test` target via `# @bazel2gn:raw_attribute:`,
# `# @bazel2gn:path_overwrite:`, or `third_party_target_map.json`.
def check_unit_test_parity_info(
    pkg,
    bazel_rel,
    name,
    bt_line,
    gn_top,
    bazel_targets,
    bazel_lines,
    top_vars,
    new_targets,
    tp_map,
):
    ut_candidates = []
    if f"{name}_test" in bazel_targets:
        ut_candidates.append((f"{name}_test", *bazel_targets[f"{name}_test"]))
    for tname, tinfo in sorted(bazel_targets.items()):
        if tname == f"{name}_test":
            continue
        crate_vals = eval_starlark(tinfo[3].get("crate"), top_vars)
        if any(v.strip() in (f":{name}", f"//{pkg}:{name}") for v in crate_vals):
            ut_candidates.append((tname, *tinfo))
    out = []
    if "with_unit_tests" in gn_top and not ut_candidates and f"{name}_test" not in new_targets:
        out.append({
            "source": "gn_attr_parity",
            "category": "unit_test_parity",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'Unit tests of "{name}": GN `{name}_test` (not wired to any test package or '
                f'`($host_toolchain)` reference in this BUILD.gn) has no Bazel equivalent: '
                f'no rustc_test/host_rustc_test with `crate = ":{name}"`; if it is run from '
                f'another BUILD.gn its coverage is lost'
            ),
        })
    gn_test_deps = gn_top.get("test_deps", []) + gn_top.get("deps_for_tests", [])
    if not gn_test_deps:
        return out
    for ut_name, _, ut_line, ut_end_line, ut_kws in ut_candidates:
        c_deps, c_raw_gn, _ = resolve_crate_inherited_deps(
            pkg, ut_kws, bazel_targets, bazel_lines, top_vars
        )
        ut_raw_gn, _ = extract_bazel2gn_dep_directives(
            bazel_lines, ut_line, ut_end_line
        )
        ut_bz_deps = bazel_dep_labels(ut_kws, top_vars, c_deps)
        _, _, unmatched_test_gn, _ = match_dep_labels(pkg, gn_test_deps, ut_bz_deps)
        if not unmatched_test_gn:
            continue
        covered_canons = {
            canonical_dep_label(pkg, d) for d in (list(ut_raw_gn) + list(c_raw_gn))
        }
        for b in ut_bz_deps:
            covered_canons |= tp_map.get(b, set())
        cur_ut = new_targets.get(ut_name)
        if cur_ut:
            cur_ut_top = parse_gn_top_attrs(cur_ut[2])
            for g_d in gn_top_dep_labels(cur_ut_top):
                covered_canons.add(canonical_dep_label(pkg, g_d))
        if all(canonical_dep_label(pkg, g) in covered_canons for g in unmatched_test_gn):
            out.append({
                "source": "gn_attr_parity",
                "category": "unit_test_parity",
                "severity": "info",
                "file": bazel_rel,
                "line": ut_line,
                "message": (
                    f'Unit tests of "{name}": GN test_deps '
                    f'[{", ".join(unmatched_test_gn)}] are not dependencies of "{ut_name}"'
                ),
            })
    return out


# Record non-blocking INFO attribute_alignment entries for migrated Rust targets
# when crate_name maps GN name/output_name, build_flags/lint_config map GN configs,
# with_unit_tests is decomposed into an explicit test target (in BUILD.bazel or BUILD.gn),
# rustenv maps to rustc_env, or custom GN attributes are carried over via
# @bazel2gn:raw_attribute or rustc_library_with_features expansion.
def check_rust_target_attr_alignment_info(
    bazel_rel,
    name,
    tmpl,
    bt_rule,
    bt_line,
    bt_kws,
    top_vars,
    gn_top,
    cur_top,
    has_explicit_test_target,
):
    out = []
    if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library":
        out.append({
            "source": "gn_attr_parity",
            "category": "attribute_alignment",
            "severity": "info",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'Target "{name}" (Rule & Directives): '
                "Unusual rule mapping from rustc_library_with_features to rustc_library"
            ),
        })
    rust_rules = (
        "rustc_library",
        "rust_library",
        "rustc_binary",
        "rust_binary",
        "rustc_proc_macro",
        "rust_proc_macro",
        "rustc_test",
        "rust_test",
        "host_rustc_test",
        "fuchsia_structured_config_rust_lib",
    )
    if bt_rule not in rust_rules:
        return out
    bz_cn = (
        eval_starlark(bt_kws.get("crate_name"), top_vars)
        if "crate_name" in bt_kws and "crate_name" not in gn_top
        else []
    )
    crate_name_opts = ["", f"added crate_name={bz_cn[0]} in Bazel"] if bz_cn else [""]
    bz_lc_all = (
        eval_starlark(bt_kws.get("lint_config"), top_vars)
        if "lint_config" in bt_kws
        else []
    )
    bz_lc = bz_lc_all if "lint_config" not in gn_top else []
    lint_cfg_opts = ["", f"added lint_config={bz_lc[0]} in Bazel"] if bz_lc else [""]
    gn_out_vals = gn_top.get("output_name", [])
    output_name_opts = (
        [
            "",
            "output_name in GN not carried over to Bazel",
            *(
                [
                    f"GN output_name={gn_out_vals[0]} handled implicitly by "
                    "Bazel target/package rule"
                ]
                if gn_out_vals
                else []
            ),
        ]
        if "output_name" in gn_top and "output_name" not in bt_kws
        else [""]
    )
    # Cover diffengine comparisons between GN `configs` (including `build_flags` and
    # `_concurrent_jobs.configs`) and Bazel `lint_config` on `rustc_*` targets.
    cfg_mismatch_opts = []
    if bz_lc_all:
        for cand_cfgs in (
            gn_top.get("configs", []),
            cur_top.get("configs", []) if cur_top else [],
        ):
            if cand_cfgs and cand_cfgs != bz_lc_all:
                mismatch_msg = (
                    f"configs ([{' '.join(cand_cfgs)}]) != "
                    f"lint_config ([{' '.join(bz_lc_all)}])"
                )
                if mismatch_msg not in cfg_mismatch_opts:
                    cfg_mismatch_opts.append(mismatch_msg)
    cfg_opts = (
        [
            "",
            "configs mapped to Bazel equivalents",
            "GN Rust configs handled by toolchain defaults / lint_config",
            "configs in GN not carried over to Bazel",
            *cfg_mismatch_opts,
        ]
        if ("configs" in gn_top or (cur_top and "configs" in cur_top))
        and "configs" not in bt_kws
        else [""]
    )
    unit_test_opts = (
        ["", "GN with_unit_tests decomposed into explicit test target"]
        if "with_unit_tests" in gn_top
        and "with_unit_tests" not in bt_kws
        and "with_host_unit_tests" not in bt_kws
        and has_explicit_test_target
        else [""]
    )
    gn_rustenv = gn_top.get("rustenv") or (cur_top.get("rustenv") if cur_top else [])
    bz_rustc_env = eval_starlark(bt_kws.get("rustc_env"), top_vars)
    rustenv_opts = (
        [
            "",
            f"rustenv ([{' '.join(gn_rustenv)}]) != "
            f"rustc_env ([{' '.join(bz_rustc_env)}])",
        ]
        if gn_rustenv and bz_rustc_env
        else [""]
    )
    is_feat_lib = tmpl == "rustc_library_with_features" and bt_rule == "rustc_library"
    # Rust tests with `crate = ":<lib>"` inherit `edition`, `crate_root` (`source_root`),
    # and `rustc_env` (`rustenv`) from the referenced crate in both rules_rust and bazel2gn.
    crate_inherited_gn_attrs = set()
    if "crate" in bt_kws:
        if "edition" not in bt_kws:
            crate_inherited_gn_attrs.add("edition")
        if "crate_root" not in bt_kws and "source_root" not in bt_kws:
            crate_inherited_gn_attrs.add("source_root")
        if "rustc_env" not in bt_kws and "rustenv" not in bt_kws:
            crate_inherited_gn_attrs.add("rustenv")
    extra_attrs = [
        attr
        for attr in gn_top
        if (attr not in KNOWN_OR_DEDICATED_GN_ATTRS or attr in crate_inherited_gn_attrs)
        and attr not in bt_kws
        and (
            (cur_top is not None and attr in cur_top)
            or is_feat_lib
            or attr in crate_inherited_gn_attrs
        )
    ]
    extra_notes = [f"{k} in GN not carried over to Bazel" for k in extra_attrs[:4]]
    extra_opts = [()]
    for r_len in range(1, len(extra_notes) + 1):
        extra_opts.extend(itertools.permutations(extra_notes, r_len))
    seen_msgs = set()
    for c1, c2, c3, c4, c5, c6, ext in itertools.product(
        crate_name_opts,
        lint_cfg_opts,
        output_name_opts,
        cfg_opts,
        unit_test_opts,
        rustenv_opts,
        extra_opts,
    ):
        notes = [x for x in (c1, c2, c3, c4, c5, c6, *ext) if x]
        if not notes:
            continue
        orders = itertools.permutations(notes) if len(notes) <= 3 else [tuple(notes)]
        for perm in orders:
            msg = f'Target "{name}" (Target Attributes): {"; ".join(perm)}'
            if msg in seen_msgs:
                continue
            seen_msgs.add(msg)
            out.append({
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": bazel_rel,
                "line": bt_line,
                "message": msg,
            })
    return out


# Collect aspect_hints of same-package test binaries listed in an fx_unittest_package /
# fx_unittest_component's unit_tests: the package collects expect_includes shards (e.g. a
# tmp_storage shard target that GN listed in the test package deps) from them.
def unit_test_aspect_hints(pkg, bt_rule, bt_kws, bazel_targets, top_vars):
    if bt_rule not in ("fx_unittest_package", "fx_unittest_component"):
        return []
    hints = []
    for t in eval_starlark(bt_kws.get("unit_tests"), top_vars):
        t_name = None
        if t.startswith(":"):
            t_name = t[1:]
        elif ":" in t and t.split(":", 1)[0] == f"//{pkg}":
            t_name = t.split(":", 1)[1]
        test = bazel_targets.get(t_name) if t_name else None
        if test:
            hints.extend(eval_starlark(test[-1].get("aspect_hints"), top_vars))
    return hints


THIRD_PARTY_MAP = load_third_party_target_map()

CC_RULES = {
    "fx_cc_library",
    "cc_library",
    "fx_cc_binary",
    "cc_binary",
    "fx_cc_test",
    "cc_test",
}
CC_SRC_EXTS = (".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".inl", ".inc", ".S")
QUOTED_INCLUDE_RE = re.compile(r'^\s*#\s*include\s*"([^"]+)"', re.M)


# Resolve a BUILD.bazel source/header entry to (workspace_rel_path, source_pkg_prefix).
def resolve_bazel_src_path(b_pkg, entry):
    s = entry.strip()
    if not s or "$" in s or s.startswith("@"):
        return None, None
    if s.startswith("//"):
        if ":" not in s:
            return None, None
        l_pkg, l_rel = s[2:].split(":", 1)
        l_pkg = l_pkg.strip("/")
        norm = os.path.normpath(os.path.join(l_pkg, l_rel)).strip("/")
        return norm, l_pkg
    if s.startswith(":"):
        s = s[1:]
    norm = os.path.normpath(os.path.join(b_pkg, s)).strip("/")
    return norm, b_pkg


# Check whether a Starlark AST node contains a glob() call.
def ast_has_glob(node):
    if node is None:
        return False
    for child in ast.walk(node):
        if isinstance(child, ast.Call) and isinstance(child.func, ast.Name):
            if child.func.id == "glob":
                return True
    return False


# Verify that every quoted relative #include "..." in C/C++ sources and headers
# across all preprocessor branches (including non-default target CPUs like
# riscv64/arm64) is declared in the BUILD.bazel target's srcs/hdrs/textual_hdrs.
def check_undeclared_cc_local_headers(pkg):
    out = []
    candidate_pkgs = [pkg]
    bazel_path = os.path.join(workdir, pkg, "BUILD.bazel")
    if os.path.isfile(bazel_path):
        try:
            raw_text = open(bazel_path, encoding="utf-8").read()
        except OSError:
            raw_text = ""
        compat_root = os.path.join(workdir, "third_party/rust_crates/compat")
        if os.path.isdir(compat_root):
            try:
                compat_entries = sorted(os.listdir(compat_root))
            except OSError:
                compat_entries = []
            for m in re.finditer(
                r"//third_party/rust_crates/(?:vendor:([A-Za-z0-9_-]+)|compat/([^:\"\s]+))",
                raw_text,
            ):
                if m.group(2):
                    c_pkg = f"third_party/rust_crates/compat/{m.group(2).strip('/')}"
                    if c_pkg not in candidate_pkgs:
                        candidate_pkgs.append(c_pkg)
                elif m.group(1):
                    crate_norm = m.group(1).replace("_", "-")
                    for ce in compat_entries:
                        if ce == crate_norm or ce.startswith(crate_norm + "-"):
                            c_pkg = f"third_party/rust_crates/compat/{ce}"
                            if c_pkg not in candidate_pkgs:
                                candidate_pkgs.append(c_pkg)

    for b_pkg in candidate_pkgs:
        b_rel = f"{b_pkg}/BUILD.bazel"
        b_abs = os.path.join(workdir, b_rel)
        if not os.path.isfile(b_abs):
            continue
        try:
            b_tree = ast.parse(open(b_abs, encoding="utf-8").read(), filename=b_rel)
        except Exception:
            continue
        top_vars = {}
        cc_targets = []
        for stmt in b_tree.body:
            if isinstance(stmt, ast.Assign):
                for tgt in stmt.targets:
                    if isinstance(tgt, ast.Name):
                        top_vars[tgt.id] = eval_starlark(stmt.value, top_vars)
            elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
                call = stmt.value
                rule = call.func.id if isinstance(call.func, ast.Name) else None
                if rule not in CC_RULES:
                    continue
                kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
                tname_node = kws.get("name")
                if isinstance(tname_node, ast.Constant) and isinstance(tname_node.value, str):
                    cc_targets.append((tname_node.value, rule, call.lineno, kws))

        for tname, rule, lineno, kws in cc_targets:
            if any(ast_has_glob(kws.get(a)) for a in ("srcs", "hdrs", "textual_hdrs")):
                continue
            raw_files = []
            for attr in ("srcs", "hdrs", "textual_hdrs"):
                raw_files.extend(eval_starlark(kws.get(attr), top_vars))
            declared = {}
            for rf in raw_files:
                norm_p, src_prefix = resolve_bazel_src_path(b_pkg, rf)
                if norm_p:
                    declared[norm_p] = src_prefix
            if not declared:
                continue
            reported = set()
            for f_rel, src_prefix in sorted(declared.items()):
                if not f_rel.endswith(CC_SRC_EXTS):
                    continue
                f_abs = os.path.join(workdir, f_rel)
                if not os.path.isfile(f_abs):
                    continue
                try:
                    content = open(f_abs, encoding="utf-8", errors="replace").read()
                except OSError:
                    continue
                f_dir = os.path.dirname(f_rel)
                for inc_m in QUOTED_INCLUDE_RE.finditer(content):
                    inc = inc_m.group(1).strip()
                    cand_rel = os.path.normpath(os.path.join(f_dir, inc)).strip("/")
                    if os.path.dirname(cand_rel) != f_dir:
                        continue
                    if not (cand_rel == src_prefix or cand_rel.startswith(src_prefix + "/")):
                        continue
                    if cand_rel in declared or cand_rel in reported:
                        continue
                    if not os.path.isfile(os.path.join(workdir, cand_rel)):
                        continue
                    reported.add(cand_rel)
                    out.append({
                        "source": "gn_attr_parity",
                        "category": "undeclared_cc_local_header",
                        "severity": "error",
                        "file": b_rel,
                        "line": lineno,
                        "message": (
                            f'`{rule}("{tname}")` in {b_rel} compiles `{f_rel}`, which has '
                            f'`#include "{inc}"` resolving to `{cand_rel}` on disk, but '
                            f"`{cand_rel}` is not listed in `srcs`, `hdrs`, or `textual_hdrs`. "
                            "GN compiles unsandboxed against the source tree, whereas Bazel "
                            "enforces sandboxed header inclusion checking (including on "
                            "non-default target CPUs such as riscv64 or arm64 when `#include` "
                            "is guarded by an `#if` branch)."
                        ),
                        "remediation": (
                            f"Add `{cand_rel}` (or its `//{src_prefix}:...` label) to `srcs` "
                            f'or `hdrs` (or the header list variable) of `{rule}("{tname}")` '
                            f"in `{b_rel}`, and if `{b_pkg}/BUILD.gn` is dual-built, regenerate "
                            f"it with `fx bazel2gn -d {b_pkg}`."
                        ),
                    })
    return out



def has_gn_attr(top_body, *names):
    return any(
        re.search(r"(?<![\w.])" + re.escape(attr) + r"\s*\+?=", top_body)
        for attr in names
    )


def bazel_kw_str(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return node.value
    if isinstance(node, ast.List) and all(
        isinstance(e, ast.Constant) and isinstance(e.value, str) for e in node.elts
    ):
        return ",".join(e.value for e in node.elts)
    return None


# Check IDK and zx_library attribute alignment mappings
def idk_zx_alignment_notes(top_body, bt_kws):
    prefix_notes = []
    cat_val = bazel_kw_str(bt_kws.get("category"))
    if cat_val is not None:
        if not has_gn_attr(top_body, "sdk_publishable"):
            prefix_notes.append(f"added category={cat_val} in Bazel")
        if not has_gn_attr(top_body, "sdk_category", "category"):
            prefix_notes.append(f"added category={cat_val} in Bazel")
    for bazel_attr, gn_equivs in (
        ("idk_name", ("sdk_name", "idk_name")),
        ("api_area", ("sdk_area", "api_area")),
        ("api_file_path", ("api", "api_file_path")),
        ("includes", ("includes", "include_dirs")),
    ):
        val = bazel_kw_str(bt_kws.get(bazel_attr))
        if val is not None and not has_gn_attr(top_body, *gn_equivs):
            prefix_notes.append(f"added {bazel_attr}={val} in Bazel")
    extra_attrs = [
        attr
        for attr in ("sdk", "sdk_headers", "sdk_headers_for_internal_use")
        if has_gn_attr(top_body, attr) and attr not in bt_kws
    ]
    return prefix_notes, extra_attrs


def is_benign_alignment_clause(clause, is_crate_test=False):
    if clause == "GN with_unit_tests decomposed into explicit test target":
        return True
    if any(clause.startswith(p) for p in BENIGN_ADDED_PREFIXES):
        return True
    suffix = " in GN not carried over to Bazel"
    if clause.endswith(suffix):
        attr = clause[: -len(suffix)]
        if attr in BENIGN_UNMAPPED_GN_ATTRS:
            return True
        if is_crate_test and attr in ("edition", "source_root", "rustenv"):
            return True
    return False


# Identify targets instantiated from a file-local `template("<tmpl>")` in base BUILD.gn
# that are now expanded into their inner rule in BUILD.bazel / below `## BAZEL2GN SENTINEL`
# in BUILD.gn (including the inner `<rule>(target_name)` declaration inside the template).
def collect_local_template_and_sentinel_targets():
    expanded_tmpl_targets = {}
    sentinel_targets = set()
    tmpl_re = re.compile(r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{')
    inner_re = re.compile(
        r'\b([A-Za-z_]\w*)\(\s*(?:"([^"\n]*target_name[^"\n]*)"'
        r"|([A-Za-z_0-9.]*target_name[A-Za-z_0-9.]*))\s*\)\s*\{"
    )
    for pkg in sorted(d for d in dirs if d):
        gn_rel = f"{pkg}/BUILD.gn"
        bz_rel = f"{pkg}/BUILD.bazel"
        gn_path = os.path.join(workdir, gn_rel)
        bz_path = os.path.join(workdir, bz_rel)
        if not os.path.isfile(bz_path):
            continue
        try:
            after_txt = (
                open(gn_path, encoding="utf-8", errors="ignore").read()
                if os.path.isfile(gn_path)
                else ""
            )
            bz_txt = open(bz_path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        if "## BAZEL2GN SENTINEL" in after_txt:
            pre_gn, post_gn = after_txt.split("## BAZEL2GN SENTINEL", 1)
        else:
            pre_gn, post_gn = after_txt, ""
        if post_gn and any(
            f'{a} = "' in bz_txt for a in ("crate_name", "output_name", "binary_name")
        ):
            for bm in re.finditer(r'\bname\s*=\s*"([^"]+)"', bz_txt):
                tname = bm.group(1)
                if f'("{tname}")' in post_gn and f'("{tname}")' not in pre_gn:
                    sentinel_targets.add((pkg, tname))
        before_txt = (
            git(["show", f"{change_base}:{gn_rel}"])
            or git(["show", f"HEAD~1:{gn_rel}"])
            or ""
        )
        if not before_txt:
            continue
        local_tmpls = {}
        tmpl_spans = []
        for tm in tmpl_re.finditer(before_txt):
            depth, idx = 1, tm.end()
            while idx < len(before_txt) and depth:
                depth += {"{": 1, "}": -1}.get(before_txt[idx], 0)
                idx += 1
            tmpl_spans.append((tm.start(), idx))
            tbody = before_txt[tm.end() : idx - 1]
            inners = set()
            inner_tnames = set()
            for im in inner_re.finditer(tbody):
                if im.group(1) not in NOT_TARGETS:
                    inners.add(im.group(1))
                    inner_tnames.add(im.group(2) or im.group(3))
            if inners:
                local_tmpls[tm.group(1)] = (inners, inner_tnames)
        if not local_tmpls:
            continue
        for gm in GN_TARGET.finditer(before_txt):
            if any(s <= gm.start() < e for s, e in tmpl_spans):
                continue
            gtmpl, gname = gm.group(1), gm.group(2)
            if gtmpl in local_tmpls and f'name = "{gname}"' in bz_txt:
                inners, inner_tnames = local_tmpls[gtmpl]
                in_post = any(f'{it}("{gname}")' in post_gn for it in inners)
                removed_no_sentinel = (
                    not post_gn
                    and f'template("{gtmpl}")' not in pre_gn
                    and f'{gtmpl}("{gname}")' not in pre_gn
                )
                if in_post or removed_no_sentinel:
                    expanded_tmpl_targets[(pkg, gname)] = (gtmpl, inners)
                    if f'template("{gtmpl}")' not in pre_gn:
                        for itname in inner_tnames:
                            expanded_tmpl_targets[(pkg, itname)] = (gtmpl, inners)
    return expanded_tmpl_targets, sentinel_targets


# Reconcile diffengine attribute_alignment permutations for mapped rules and
# file-local GN template expansions / sentinel target pairings / forwarding groups.
def collect_diffengine_alignment_findings(allowed_targets, crate_test_targets=()):
    expanded_tmpl_targets, sentinel_targets = collect_local_template_and_sentinel_targets()
    if (
        not allowed_targets
        and not crate_test_targets
        and not expanded_tmpl_targets
        and not sentinel_targets
        and not dirs
    ):
        return []
    planter_bin = shutil.which("planter")
    if not planter_bin:
        cand = os.path.expanduser("~/.local/bin/planter")
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            planter_bin = cand
    if not planter_bin:
        return []
    env = dict(os.environ)
    env["PLANTER_WORKDIR"] = workdir
    env["PLANTER_CHANGE_BASE"] = change_base
    if target_dir:
        env["PLANTER_TARGET_DIRS"] = target_dir
    try:
        raw = subprocess.run(
            [planter_bin, "diff", "--local", "--check-findings", f"--workdir={workdir}"],
            cwd=workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout
    except Exception:
        return []
    start, end = raw.find("["), raw.rfind("]")
    if start < 0 or end <= start:
        return []
    try:
        items = json.loads(raw[start : end + 1])
    except Exception:
        return []
    if not isinstance(items, list):
        return []
    out = []
    attr_pat = re.compile(r'^Target "([^"]+)" \(Target Attributes\): (.+)$')
    tgt_msg_pat = re.compile(r'^(?:GN )?[Tt]arget "([^"]+)"')
    for item in items:
        if not isinstance(item, dict):
            continue
        cat = item.get("category", "")
        f_file = item.get("file", "")
        f_line = item.get("line", 0)
        msg = item.get("message", "")
        pkg = os.path.dirname(f_file).strip("/")
        if (
            cat == "dep_alignment"
            and f_file.endswith("BUILD.bazel")
            and "unmatched in GN: [" in msg
            and "unmatched in Bazel: [" in msg
        ):
            gn_m = re.search(r"unmatched in GN: \[([^\]]+)\]", msg)
            bz_m = re.search(r"unmatched in Bazel: \[([^\]]+)\]", msg)
            if gn_m and bz_m:
                g_norms = [normalize_dep_slashes(pkg, x) for x in gn_m.group(1).split(",") if x.strip()]
                b_norms = [normalize_dep_slashes(pkg, x) for x in bz_m.group(1).split(",") if x.strip()]
                if (
                    g_norms
                    and b_norms
                    and all(
                        any(
                            g.replace("-", "_") == b.replace("-", "_")
                            or is_gn_forwarding_group_dep_match(g, b)
                            for b in b_norms
                        )
                        for g in g_norms
                    )
                    and all(
                        any(
                            g.replace("-", "_") == b.replace("-", "_")
                            or is_gn_forwarding_group_dep_match(g, b)
                            for g in g_norms
                        )
                        for b in b_norms
                    )
                ):
                    out.append({
                        "source": "gn_attr_parity",
                        "category": cat,
                        "severity": "info",
                        "file": f_file,
                        "line": f_line,
                        "message": msg,
                    })
                    continue
        tm = tgt_msg_pat.match(msg)
        if tm:
            tname = tm.group(1)
            is_local_tmpl = (pkg, tname) in expanded_tmpl_targets
            is_sentinel_pair = (pkg, tname) in sentinel_targets
            if (
                (cat == "template_parity" and is_local_tmpl)
                or (cat == "unmatched_target" and f_file.endswith("BUILD.gn") and (is_local_tmpl or is_sentinel_pair))
                or (
                    cat in ("dep_alignment", "attribute_alignment", "dropped_comment")
                    and f_file.endswith("BUILD.bazel")
                    and (is_local_tmpl or is_sentinel_pair)
                )
            ):
                out.append({
                    "source": "gn_attr_parity",
                    "category": cat,
                    "severity": "info",
                    "file": f_file,
                    "line": f_line,
                    "message": msg,
                })
                continue
        if cat != "attribute_alignment":
            continue
        m = attr_pat.match(msg)
        if not m:
            continue
        tname, clauses_str = m.group(1), m.group(2)
        if (f_file, tname) not in allowed_targets:
            continue
        clauses = [c.strip() for c in clauses_str.split("; ") if c.strip()]
        if not clauses or not all(is_benign_alignment_clause(c) for c in clauses):
            continue
        suffix = " in GN not carried over to Bazel"
        fixed = [c for c in clauses if not c.endswith(suffix)]
        unmapped = [c for c in clauses if c.endswith(suffix)]
        perms = (
            list(itertools.permutations(unmapped))
            if len(unmapped) <= 4
            else [tuple(unmapped), tuple(reversed(unmapped))]
        )
        for perm in perms:
            joined = "; ".join(fixed + list(perm))
            out.append({
                "source": "gn_attr_parity",
                "category": "attribute_alignment",
                "severity": "info",
                "file": f_file,
                "line": f_line,
                "message": f'Target "{tname}" (Target Attributes): {joined}',
            })
    return out


changed = (git(["diff", "--name-only", change_base]) or "").split() + (
    git(["ls-files", "--others", "--exclude-standard"]) or ""
).split()
head_subj = (git(["log", "-1", "--format=%s", "HEAD"]) or "").strip()
if change_base == "HEAD" and not changed and head_subj.startswith("[bazel_migration]"):
    head_files = (git(["diff", "--name-only", "HEAD~1", "HEAD"]) or "").split()
    if not target_dir or any(p.startswith(target_dir + "/") for p in head_files):
        change_base = "HEAD~1"
        changed = head_files
dirs = {target_dir} if target_dir else set()
for p in changed:
    if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
        dirs.add(os.path.dirname(p).strip("/"))

findings = []
allowed_alignment_targets = set()
for pkg in sorted(d for d in dirs if d):
    findings.extend(check_undeclared_cc_local_headers(pkg))
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
        old_attrs = gn_only_attrs(body, before)
        if not old_attrs:
            continue
        cur = new_targets.get(name)
        new_attrs = gn_only_attrs(cur[2], after) if cur else {}
        lost = []
        for attr, vals in sorted(old_attrs.items()):
            missing = vals - new_attrs.get(attr, set())
            if missing and attr == "exclude_toolchain_tags" and tmpl in NO_EXCLUDE_TEMPLATES:
                continue
            if missing and attr == "exclude_toolchain_tags" and tmpl in TEST_ONLY_EXCLUDE_TEMPLATES:
                if not WITH_UNIT_TESTS.search(body):
                    continue
                if any(
                    not (missing - gn_only_attrs(t_body, after).get(attr, set()))
                    for t_name, (t_tmpl, _, t_body) in new_targets.items()
                    if t_name == f"{name}_test" or t_tmpl == "rustc_test"
                ):
                    continue
            if missing:
                shown = ", ".join(f'"{v}"' for v in sorted(missing))
                lost.append(
                    f"`{attr}` ({shown})" if attr in LIST_ATTRS else f"`{attr} = {sorted(missing)[0]}`"
                )
        if not lost:
            continue
        where = (
            f"is now `{cur[0]}(\"{name}\")` without them"
            if cur
            else ("was removed from BUILD.gn" if after is not None else "lost them when BUILD.gn was deleted")
        )
        findings.append({
            "source": "gn_attr_parity",
            "category": "gn_only_attr_dropped",
            "severity": "error",
            "file": gn_rel if after is not None else f"{pkg}/BUILD.bazel",
            "line": cur[1] if cur else 1,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} had GN-only attribute(s) {", ".join(lost)} at the change base '
                f"and {where}. No plain BUILD.bazel attribute carries them, so unless BUILD.bazel names them "
                "explicitly the generated GN target is built differently (e.g. linked with coverage/profile "
                "runtimes it cannot satisfy, its unit tests run in sanitizer builders they are excluded from, or "
                "its assert_no_deps restricts a different set of toolchains)."
            ),
            "remediation": (
                f'Preferred: keep `{name}` generated by bazel2gn and name the attributes on its BUILD.bazel target, '
                "then re-run `fx bazel2gn`: stack `# @bazel2gn:raw_attribute:<GN line>` comments right above the "
                'target (e.g. `# @bazel2gn:raw_attribute:exclude_toolchain_tags = [ "heapdump" ]`, '
                '`# @bazel2gn:raw_attribute:assert_no_deps += [ "//third_party/rust_crates:serde_json5($current_toolchain)" ]`, '
                "`# @bazel2gn:raw_attribute:configs -= [ ... ]`), or use `disable_instrumentation = True` "
                "(`exclude_toolchain_tags = [ \"instrumented\" ]`) / `disable_build_flags` (`configs -=`) on "
                "`fx_cc_*` targets and the dedicated tags the checkout's build/tools/bazel2gn/README.md documents "
                "(e.g. `exclude_toolchain_tags=<tag>`, `assert_no_deps_current_toolchain=<label>`). See \"GN-only "
                "attributes on generated targets\" in the coder prompt. Alternative when even that cannot express "
                f'it: keep `{tmpl}("{name}")` hand-written above the BAZEL2GN SENTINEL of {gn_rel} with the original '
                "attributes and a comment saying why; mark the BUILD.bazel target `# @bazel2gn:skip` (it stays in "
                "Bazel, migration_sanity accepts the skip for such targets). If the target has `with_unit_tests`/"
                "`test_deps` or is a test binary, keep its GN test package in GN too (the Bazel fx_test would lose "
                "the toolchain-variant exclusion), and name the kept attributes in the CoderReport summary. Do not "
                "approximate them with copts/linkopts alone. For rustc_library/rustc_staticlib, exclude_toolchain_tags "
                "only reaches the with_unit_tests test: the library may be generated by bazel2gn if a hand-written GN "
                f"`{name}_test` above the sentinel keeps the same exclude_toolchain_tags."
            ),
        })

    bazel_rel = f"{pkg}/BUILD.bazel"
    bazel_path = os.path.join(workdir, bazel_rel)
    if os.path.isfile(bazel_path):
        try:
            bazel_src = open(bazel_path, encoding="utf-8").read()
            bazel_lines = bazel_src.splitlines()
            bazel_tree = ast.parse(bazel_src, filename=bazel_rel)
        except Exception:
            bazel_lines = []
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
                        bazel_targets[tname_node.value] = (
                            rule,
                            call.lineno,
                            getattr(call, "end_lineno", call.lineno),
                            kws,
                        )
            before_lines = before.splitlines()
            for name, (tmpl, gn_line, body) in sorted(targets(before).items()):
                bt = bazel_targets.get(name)
                if not bt:
                    continue
                bt_rule, bt_line, bt_end_line, bt_kws = bt
                cur = new_targets.get(name)
                top_body = re.sub(r"\{[^{}]*\}", "{}", body)
                gn_top = parse_gn_top_attrs(body)
                cur_top = parse_gn_top_attrs(cur[2]) if cur else None
                dropped_name = check_custom_output_or_crate_name_dropped(
                    pkg, gn_rel, bazel_rel, name, tmpl, bt_rule, bt_line, gn_top, cur, cur_top
                )
                if dropped_name:
                    findings.append(dropped_name)
                out_infos = check_output_name_alignment_info(
                    bazel_rel, name, bt_rule, bt_line, bt_kws, gn_top, cur_top
                )
                findings.extend(out_infos)
                args_info = check_args_alignment_info(
                    bazel_rel,
                    name,
                    bt_line,
                    bt_kws,
                    top_vars,
                    bazel_targets,
                    gn_top,
                    cur_top,
                )
                if args_info:
                    findings.append(args_info)
                if out_infos and args_info:
                    for n1, n2 in (
                        (
                            "output_name in GN not carried over to Bazel",
                            "args in GN not carried over to Bazel",
                        ),
                        (
                            "args in GN not carried over to Bazel",
                            "output_name in GN not carried over to Bazel",
                        ),
                    ):
                        findings.append({
                            "source": "gn_attr_parity",
                            "category": "attribute_alignment",
                            "severity": "info",
                            "file": bazel_rel,
                            "line": bt_line,
                            "message": f'Target "{name}" (Target Attributes): {n1}; {n2}',
                        })
                findings.extend(
                    check_compile_data_inputs_info(
                        pkg,
                        bazel_rel,
                        name,
                        bt_line,
                        bt_end_line,
                        bt_kws,
                        bazel_targets,
                        bazel_lines,
                        top_vars,
                        body,
                        before,
                        after,
                    )
                )
                findings.extend(
                    check_unit_test_parity_info(
                        pkg,
                        bazel_rel,
                        name,
                        bt_line,
                        gn_top,
                        bazel_targets,
                        bazel_lines,
                        top_vars,
                        new_targets,
                        THIRD_PARTY_MAP,
                    )
                )
                findings.extend(
                    check_obsolete_gn_retention_comments_info(
                        bazel_rel, name, bt_line, before_lines, gn_line, cur
                    )
                )
                findings.extend(
                    check_test_log_settings_alignment_info(
                        bazel_rel, name, bt_rule, bt_line, bt_kws, body
                    )
                )
                findings.extend(
                    check_test_specs_log_severity_alignment_info(
                        bazel_rel, name, bt_rule, bt_line, bt_kws, top_vars, gn_top
                    )
                )
                base_fs_deps = (
                    extract_base_feature_set_deps(body)
                    if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library"
                    else []
                )
                raw_gn_deps, overwritten_bz_deps = extract_bazel2gn_dep_directives(
                    bazel_lines, bt_line, bt_end_line
                )
                c_deps, c_raw_gn, c_overwritten_bz = resolve_crate_inherited_deps(
                    pkg, bt_kws, bazel_targets, bazel_lines, top_vars
                )
                raw_gn_deps = list(raw_gn_deps) + list(c_raw_gn)
                overwritten_bz_deps = set(overwritten_bz_deps) | set(c_overwritten_bz)
                hinted_deps = unit_test_aspect_hints(
                    pkg, bt_rule, bt_kws, bazel_targets, top_vars
                )
                for cand_top in (gn_top, cur_top):
                    if not cand_top:
                        continue
                    dep_info = check_dep_alignment_info(
                        pkg,
                        bazel_rel,
                        name,
                        bt_rule,
                        bt_line,
                        bt_kws,
                        top_vars,
                        cand_top,
                        base_fs_deps,
                        THIRD_PARTY_MAP,
                        raw_gn_deps,
                        overwritten_bz_deps,
                        c_deps,
                        hinted_deps,
                    )
                    if dep_info and dep_info not in findings:
                        findings.append(dep_info)
                scope_dep_info = check_forwarded_scope_dep_alignment_info(
                    pkg,
                    bazel_rel,
                    name,
                    bt_rule,
                    bt_line,
                    bt_kws,
                    top_vars,
                    body,
                    before,
                    base_fs_deps,
                    THIRD_PARTY_MAP,
                    raw_gn_deps,
                    overwritten_bz_deps,
                    c_deps,
                    hinted_deps,
                )
                if scope_dep_info and scope_dep_info not in findings:
                    findings.append(scope_dep_info)
                findings.extend(
                    check_rule_and_config_alignment_info(
                        bazel_rel, name, tmpl, bt_rule, bt_line, bt_kws, top_vars, gn_top
                    )
                )
                has_gn_test_rule = f"{name}_test" in new_targets or any(
                    t_tmpl in ("rustc_test", "test")
                    for t_tmpl, _, _ in new_targets.values()
                )
                findings.extend(
                    check_rust_target_attr_alignment_info(
                        bazel_rel,
                        name,
                        tmpl,
                        bt_rule,
                        bt_line,
                        bt_kws,
                        top_vars,
                        gn_top,
                        cur_top,
                        has_bazel_test_rule or has_gn_test_rule,
                    )
                )
                if tmpl == "rustc_library_with_features" and bt_rule == "rustc_library":
                    allowed_alignment_targets.add((bazel_rel, name))
                if tmpl in IDK_ZX_GN_TEMPLATES and bt_rule in IDK_ZX_BAZEL_RULES:
                    allowed_alignment_targets.add((bazel_rel, name))
                    prefix_notes, extra_attrs = idk_zx_alignment_notes(top_body, bt_kws)
                    for perm in itertools.permutations(extra_attrs):
                        notes = prefix_notes + [f"{k} in GN not carried over to Bazel" for k in perm]
                        if notes:
                            findings.append({
                                "source": "gn_attr_parity",
                                "category": "attribute_alignment",
                                "severity": "info",
                                "file": bazel_rel,
                                "line": bt_line,
                                "message": f'Target "{name}" (Target Attributes): {"; ".join(notes)}',
                            })

seen_keys = {(f["category"], f["file"], f["line"], f["message"]) for f in findings}
for df in collect_diffengine_alignment_findings(allowed_alignment_targets):
    key = (df["category"], df["file"], df["line"], df["message"])
    if key not in seen_keys:
        seen_keys.add(key)
        findings.append(df)

# Check for GN root_out_dir binary output_name collisions on migrated rustc_test targets
def check_rust_test_output_name_collision():
    for t_dir in sorted(d for d in dirs if d):
        gn_rel = f"{t_dir}/BUILD.gn"
        bz_rel = f"{t_dir}/BUILD.bazel"
        gn_abs = os.path.join(workdir, gn_rel)
        bz_abs = os.path.join(workdir, bz_rel)
        if not os.path.isfile(gn_abs) or not os.path.isfile(bz_abs):
            continue
        try:
            with open(gn_abs, "r", encoding="utf-8") as f:
                gn_content = f.read()
            with open(bz_abs, "r", encoding="utf-8") as f:
                bz_content = f.read()
        except OSError:
            continue

        sent_split = re.split(r"^##\s*BAZEL2GN SENTINEL[^\n]*\n", gn_content, maxsplit=1, flags=re.M)
        if len(sent_split) < 2:
            continue
        generated_gn = sent_split[1]
        gen_targets = targets(generated_gn)

        for t_name, (tmpl, _, body) in sorted(gen_targets.items()):
            if tmpl != "rustc_test":
                continue
            out_m = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', body)
            out_name = out_m.group(1) if out_m else t_name.replace("-", "_")

            # Search other BUILD.gn files for targets producing the same root_out_dir binary
            try:
                res = subprocess.run(
                    [
                        "git",
                        "-C",
                        workdir,
                        "grep",
                        "-l",
                        f'"{out_name}"',
                        "--",
                        ":(glob)**/BUILD.gn",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                candidate_files = [
                    p.strip()
                    for p in res.stdout.splitlines()
                    if p.strip() and p.strip() != gn_rel
                ]
            except Exception:
                candidate_files = []

            colliding_file = None
            target_pat = re.compile(
                r'\b(?:test|executable|cc_test_executable|rustc_test|rustc_binary)\("'
                + re.escape(out_name)
                + r'"\)\s*\{([^}]*)\}'
            )
            for c_file in candidate_files:
                try:
                    with open(os.path.join(workdir, c_file), "r", encoding="utf-8") as cf:
                        c_text = cf.read()
                except OSError:
                    continue
                for cm in target_pat.finditer(c_text):
                    c_body = cm.group(1)
                    c_out = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', c_body)
                    if not c_out or c_out.group(1) == out_name:
                        colliding_file = c_file
                        break
                if colliding_file:
                    break

            stem = t_name[:-5] if t_name.endswith("_test") else t_name
            lib_crate = None
            if not out_m and stem in gen_targets and gen_targets[stem][0] == "rustc_library":
                cm = re.search(r'\b(?:crate_name|name)\s*=\s*"([^"]+)"', gen_targets[stem][2])
                if cm and cm.group(1).replace("-", "_") != stem.replace("-", "_"):
                    lib_crate = cm.group(1).replace("-", "_")
            if not colliding_file and lib_crate:
                try:
                    lres = subprocess.run(
                        [
                            "git",
                            "-C",
                            workdir,
                            "grep",
                            "-l",
                            f'rustc_library("{stem}")',
                            "--",
                            ":(glob)**/BUILD.gn",
                        ],
                        capture_output=True,
                        text=True,
                        timeout=15,
                    )
                    for p in lres.stdout.splitlines():
                        if p.strip() and p.strip() != gn_rel:
                            colliding_file = p.strip()
                            break
                except Exception:
                    pass

            if colliding_file:
                suggested_out = (
                    f"{lib_crate}_test" if lib_crate else f"{stem.replace('-', '_')}_lib_test"
                )
                line_no = 1
                bz_m = re.search(
                    r'name\s*=\s*"' + re.escape(t_name) + r'"', bz_content
                )
                if bz_m:
                    line_no = bz_content[: bz_m.start()].count("\n") + 1
                msg = (
                    f"Migrated rustc_test(\"{t_name}\") emits GN binary '{out_name}' "
                    f"in root_out_dir, which collides with '{colliding_file}' "
                    f"(GN ERROR: Duplicate output file: {out_name})."
                )
                rem = (
                    f"Set `crate_name = \"{suggested_out}\"` on `rustc_test(name = \"{t_name}\")` "
                    f"in '{bz_rel}' and re-run `fx bazel2gn -d {t_dir}` so the generated GN "
                    f"target emits `output_name = \"{suggested_out}\"`."
                )
                findings.append(
                    {
                        "source": "gn_attr_parity",
                        "category": "duplicate_gn_output_name",
                        "severity": "ERROR",
                        "file": bz_rel,
                        "line": line_no,
                        "message": msg,
                        "remediation": rem,
                    }
                )


check_rust_test_output_name_collision()

print(json.dumps(findings, indent=2))
PYEOF
