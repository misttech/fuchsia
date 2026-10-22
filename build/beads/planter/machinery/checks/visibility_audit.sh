#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check auditing BUILD.bazel files for visibility scoping and rdeps anti-patterns:
# 1. Package-level default_visibility in package(...)
# 2. Missing target-level visibility when a target has external callers
# 3. Overly narrow sprawling visibility lists (> 15 entries) on widely-used libraries/utilities (should use 2-3 level subsystem/top-level area rollups or ["//:__subpackages__"] for foundational tree-wide libraries)
# 4. Disallowed target-level '//visibility:public' when migrating GN targets to Bazel (per determining_bazel_visibility/SKILL.md),
#    including on test entry points (fx_unittest_package, fx_test, host_*_test), which should be private
# 5. Redundant ':__pkg__' (or '//<current_pkg>:__pkg__') in target visibility lists
# 6. Overbroad repository-wide visibility ('//:__subpackages__') and top-level roots ('//src:__subpackages__', '//sdk:__subpackages__', etc.) on non-globally-used targets
# 7. Multi-domain catch-all umbrella wildcards ('//src/lib:__subpackages__', '//sdk/lib:__subpackages__', '//src/testing:__subpackages__', '//src/tests:__subpackages__')
# 8. Cross-area depth-2 wildcards ('//src/<area>:__subpackages__') on non-global targets outside //src/<area>
# 9. Overbroad depth >= 3 ':__subpackages__' wildcards wider than the Lowest Common Ancestor (LCA) of actual callers on non-global targets
# 10. False-positive rdeps from visibility allowlists (visibility.gni / visibility = [...]) or GN-only targets (:tests, :benchmarks)
# 11. Uncovered callers: a package whose BUILD.gn/BUILD.bazel depends on the target, or on a generated
#     '<target>_<suffix>' sub-target (e.g. fidl_library bindings '<fidl>_rust', '<fidl>_cpp'), that no
#     visibility entry covers. References to generated sub-targets count as rdeps of the owning target.

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")

EXCLUDED_GLOBAL_VIS_PREFIXES = (
    "bundles/assembly/",
    "build/bazel/",
    "build/bazel2gn/",
    "build/images/",
    "build/config/rust/lints/",
    "third_party/rust_crates/",
)

candidate_files = set()
if target_dir:
    rel_bazel = os.path.join(target_dir, "BUILD.bazel")
    if os.path.isfile(os.path.join(workdir, rel_bazel)):
        candidate_files.add(rel_bazel)

change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
git_cmds = [
    ["git", "-C", workdir, "diff", "--name-only", change_base, "--", "*BUILD.bazel"],
    ["git", "-C", workdir, "diff", "--cached", "--name-only", "--", "*BUILD.bazel"],
]
if target_dir:
    git_cmds.append(["git", "-C", workdir, "ls-files", "--others", "--exclude-standard", "--", f"{target_dir}/*BUILD.bazel"])
else:
    git_cmds.append(["git", "-C", workdir, "ls-files", "--others", "--exclude-standard", "--", "*BUILD.bazel"])
for cmd in git_cmds:
    try:
        out = subprocess.check_output(cmd, stderr=subprocess.DEVNULL, text=True)
        for line in out.splitlines():
            line = line.strip()
            if (
                line.endswith("BUILD.bazel")
                and not line.startswith(EXCLUDED_GLOBAL_VIS_PREFIXES)
                and os.path.isfile(os.path.join(workdir, line))
            ):
                candidate_files.add(line)
    except Exception:
        pass

if not candidate_files:
    print("[]")
    sys.exit(0)

BANNED_TOP_LEVEL_SUBPKGS = {
    "src",
    "sdk",
    "build",
    "zircon",
    "third_party",
}

BANNED_UMBRELLA_SUBPKGS = {
    "src/lib",
    "sdk/lib",
    "src/testing",
    "src/tests",
    "third_party/rust_crates",
}

def get_pkg_area(p):
    parts = p.strip("/").split("/")
    if not parts or not parts[0]:
        return ""
    if parts[0] in ("src", "sdk") and len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0]

def get_distinct_areas(rdep_pkgs, pkg_path):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    return sorted({get_pkg_area(p) for p in pkgs if get_pkg_area(p)})

VG_ROOT = "vendor/" + "google"
VG_SUBPKGS = "//" + VG_ROOT + ":__subpackages__"

def recommend_narrow_visibility(rdep_pkgs, pkg_path):
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    if not raw_pkgs:
        return []
    has_vendor_google = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in raw_pkgs)
    pkgs = [p for p in raw_pkgs if p != VG_ROOT and not p.startswith(VG_ROOT + "/")]
    vendor_entries = [VG_SUBPKGS] if has_vendor_google else []
    if not pkgs:
        return vendor_entries
    if len(pkgs) <= 5:
        result = list(vendor_entries)
        for p in pkgs:
            if p == "bundles/assembly":
                result.append("//bundles/assembly:__subpackages__")
                continue
            if any(p != other and p.startswith(other + "/") for other in pkgs if len(other.split("/")) >= 3):
                continue
            has_child = any(other != p and other.startswith(p + "/") for other in pkgs)
            if has_child and len(p.split("/")) >= 3:
                result.append(f"//{p}:__subpackages__")
            else:
                result.append(f"//{p}:__pkg__")
        return sorted(set(result))

    by_d3 = {}
    for p in pkgs:
        parts = p.split("/")
        if len(parts) < 3:
            by_d3.setdefault(p, []).append(p)
        else:
            d3 = "/".join(parts[:3])
            by_d3.setdefault(d3, []).append(p)

    result = set(vendor_entries)
    for d3, group in sorted(by_d3.items()):
        parts_d3 = d3.split("/")
        if len(parts_d3) < 3:
            for p in group:
                if p == "bundles/assembly":
                    result.add("//bundles/assembly:__subpackages__")
                else:
                    result.add(f"//{p}:__pkg__")
            continue

        split_group = [g.split("/") for g in group]
        min_len = min(len(s) for s in split_group)
        lca_len = 0
        for i in range(min_len):
            if len({s[i] for s in split_group}) == 1:
                lca_len = i + 1
            else:
                break
        lca = "/".join(split_group[0][:max(3, lca_len)])
        lca_depth = len(lca.split("/"))

        if lca in group and len(group) >= 2:
            result.add(f"//{lca}:__subpackages__")
        elif (lca_depth >= 4 and len(group) >= 3) or (lca_depth == 3 and len(group) >= 4):
            result.add(f"//{lca}:__subpackages__")
        else:
            for p in group:
                if any(p != other and p.startswith(other + "/") for other in group if len(other.split("/")) >= 3):
                    continue
                has_child = any(other != p and other.startswith(p + "/") for other in group)
                if has_child and len(p.split("/")) >= 3:
                    result.add(f"//{p}:__subpackages__")
                else:
                    result.add(f"//{p}:__pkg__")

    return sorted(result)

def recommend_area_rollup_visibility(rdep_pkgs, pkg_path):
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    if not raw_pkgs:
        return []
    has_vendor_google = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in raw_pkgs)
    pkgs = [p for p in raw_pkgs if p != VG_ROOT and not p.startswith(VG_ROOT + "/")]
    by_area = {}
    for p in pkgs:
        area = get_pkg_area(p)
        by_area.setdefault(area, []).append(p)
    result = {VG_SUBPKGS} if has_vendor_google else set()
    for area, group in sorted(by_area.items()):
        if area in BANNED_UMBRELLA_SUBPKGS:
            for item in recommend_narrow_visibility(group, pkg_path):
                result.add(item)
        elif "/" in area:
            if len(group) >= 2 or any(g != area and len(g.split("/")) > 2 for g in group):
                result.add(f"//{area}:__subpackages__")
            else:
                result.add(f"//{group[0]}:__pkg__")
        else:
            by_d2 = {}
            for g in group:
                parts = g.split("/")
                d2 = "/".join(parts[:2]) if len(parts) >= 2 else parts[0]
                by_d2.setdefault(d2, []).append(g)
            for d2, d2_group in sorted(by_d2.items()):
                if len(d2_group) >= 2 or any(x != d2 for x in d2_group):
                    result.add(f"//{d2}:__subpackages__")
                else:
                    result.add(f"//{d2_group[0]}:__pkg__")
    return sorted(result)

def is_globally_used_target(rdep_pkgs, pkg_path):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    narrow = recommend_narrow_visibility(pkgs, pkg_path)
    areas = get_distinct_areas(pkgs, pkg_path)
    return len(areas) >= 6 and (len(narrow) > 15 or len(pkgs) >= 20)

def recommend_visibility(rdep_pkgs, pkg_path, is_test_support=False):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    narrow = recommend_narrow_visibility(pkgs, pkg_path)
    globally_used = is_globally_used_target(pkgs, pkg_path)
    if globally_used or len(narrow) > 8:
        rollup = recommend_area_rollup_visibility(pkgs, pkg_path)
        if globally_used and len(rollup) > 15:
            return ["//:__subpackages__"]
        if len(rollup) <= len(narrow):
            return rollup
    if is_test_support and len(get_distinct_areas(pkgs, pkg_path)) >= 2:
        return ["//:__subpackages__"]
    return narrow

def get_orig_gn_visibility_scopes(pkg_path):
    """Returns {target_name: [(scope_pkg, 'pkg'|'subpackages'), ...]} for targets in change_base BUILD.gn
    that had an explicit non-wildcard visibility = [...] restriction."""
    gn_rel = f"{pkg_path}/BUILD.gn" if pkg_path else "BUILD.gn"
    try:
        gn_src = subprocess.check_output(
            ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return {}
    no_comments = re.sub(r"#[^\n]*", "", gn_src)
    def parse_gn_vis_list(vis_body):
        if "$" in vis_body:
            return None
        pats = re.findall(r'"([^"]+)"', vis_body)
        if not pats:
            return None
        scopes = []
        for pat in pats:
            if pat == "//*":
                return None
            if pat.startswith(":"):
                scopes.append((pkg_path, "pkg"))
                continue
            m_gn = re.match(r"^//([^:*]*?)/?(\*|:.*)?$", pat)
            if not m_gn:
                return None
            sp = m_gn.group(1).strip("/")
            suffix = m_gn.group(2) or ""
            scopes.append((sp, "subpackages" if suffix == "*" else "pkg"))
        return scopes
    out = {}
    target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    for m in target_re.finditer(no_comments):
        tname = m.group(2)
        depth, in_str, i, n = 0, False, m.end() - 1, len(no_comments)
        while i < n:
            c = no_comments[i]
            if in_str:
                if c == "\\":
                    i += 2
                    continue
                if c == '"':
                    in_str = False
            elif c == '"':
                in_str = True
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        tbody = no_comments[m.end() : i]
        if re.search(r"\bvisibility\s*\+=", tbody):
            continue
        vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
        if vm:
            scopes = parse_gn_vis_list(vm.group(1))
            if scopes:
                out[tname] = scopes
    return out

def get_preexisting_vis_info(rel_path):
    """Returns (vis_strings, no_vis_targets) already present in change_base/HEAD~1 of rel_path (unless rel_path is target_dir)."""
    if target_dir and rel_path == os.path.join(target_dir, "BUILD.bazel"):
        return set(), set()
    for ref in dict.fromkeys([change_base, "HEAD~1"]):
        try:
            prev_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            prev_tree = ast.parse(prev_src, filename=rel_path)
            vis_vals = set()
            no_vis_targets = set()
            for node in ast.walk(prev_tree):
                if isinstance(node, ast.Call):
                    tname = None
                    has_vis = False
                    for kw in node.keywords:
                        if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                            tname = kw.value.value
                        elif kw.arg == "visibility":
                            has_vis = True
                            if isinstance(kw.value, ast.List):
                                for elt in kw.value.elts:
                                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                                        vis_vals.add(elt.value)
                    if tname and not has_vis:
                        no_vis_targets.add(tname)
            return vis_vals, no_vis_targets
        except Exception:
            continue
    return set(), set()

# Batch-discover all files mentioning any candidate package in a single rg invocation
pkg_paths = sorted({os.path.dirname(p).strip("/") for p in candidate_files})
rg_args = ["rg", "-l", "--fixed-strings"]
for p in pkg_paths:
    rg_args.extend(["-e", f"//{p}"])
rg_args.extend(["-g", "*BUILD.gn", "-g", "*BUILD.bazel", "-g", "*.gni", "-g", "*.bzl", workdir])

matched_files = set()
try:
    out = subprocess.check_output(rg_args, stderr=subprocess.DEVNULL, text=True)
    for line in out.splitlines():
        if line.strip():
            matched_files.add(line.strip())
except Exception:
    pass

for p in pkg_paths:
    parts = p.split("/")
    for i in range(1, len(parts)):
        anc_gn = os.path.join(workdir, "/".join(parts[:i]), "BUILD.gn")
        if os.path.isfile(anc_gn):
            matched_files.add(anc_gn)

file_cache = {}
for fpath in sorted(matched_files):
    try:
        with open(fpath, "r", encoding="utf-8", errors="ignore") as f:
            raw = f.read()
        no_comments = re.sub(r"#[^\n]*", "", raw)
        stripped = re.sub(r"(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]", "", no_comments, flags=re.DOTALL)
        file_cache[fpath] = (no_comments, stripped)
    except Exception:
        pass

CONVERTIBLE_GN_TEMPLATES = {
    "static_library",
    "source_set",
    "shared_library",
    "executable",
    "library_headers",
    "rustc_library",
    "rustc_binary",
    "rustc_macro",
    "rustc_test",
    "go_library",
    "go_binary",
    "go_test",
    "python_library",
    "python_binary",
    "fidl",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
    "fuchsia_package",
    "fuchsia_package_with_single_component",
    "fuchsia_component",
    "fuchsia_component_manifest",
    "resource",
}

BAZEL_NON_GN_SYNCED_RULES = {
    "fx_packaged_binary",
    "fx_test_component",
    "fx_unittest_component",
    "fx_test",
    "fx_unittest_package",
    "test_suite",
    "rustc_test",
    "host_rustc_test",
    "host_go_test",
    "host_py_test",
    "host_test",
    "host_test_data_files",
    "host_test_data_map",
    "cc_binary_host_tool",
    "go_binary_host_tool",
    "py_binary_host_tool",
    "rustc_binary_host_tool",
    "idk_cc_binary_host_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "wrap_host_rust_test",
    "go_test",
}

def owning_bazel_target(sub, bazel_targets):
    """Maps a referenced label name to the Bazel target that defines or generates it.

    Rules such as fidl_library emit GN sub-targets named '<name>_<suffix>' (e.g.
    '<fidl>_rust', '<fidl>_cpp', '<fidl>_hlcpp') that callers depend on and that share the
    visibility of '<name>'; attribute them to the longest matching Bazel target name.
    """
    if sub in bazel_targets:
        return sub
    best = None
    for t in bazel_targets:
        if sub.startswith(t + "_") and (best is None or len(t) > len(best)):
            best = t
    return best

def compute_package_rdeps(pkg_path, bazel_targets, package_targets, non_gn_synced_targets, alias_map):
    default_target = os.path.basename(pkg_path)
    gn_file = os.path.join(workdir, pkg_path, "BUILD.gn")
    gn_only_targets = {"tests", "benchmarks"}
    gn_nonconvertible_targets = {"tests", "benchmarks"}
    if os.path.isfile(gn_file):
        try:
            with open(gn_file, "r", encoding="utf-8", errors="ignore") as f:
                gn_src = f.read()
            above_sentinel = gn_src.split("BAZEL2GN SENTINEL")[0] if "BAZEL2GN SENTINEL" in gn_src else gn_src
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', above_sentinel, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if t not in bazel_targets:
                    gn_only_targets.add(t)
                if tmpl not in CONVERTIBLE_GN_TEMPLATES:
                    gn_nonconvertible_targets.add(t)
        except Exception:
            pass

    abs_label_re = re.compile(r'"//' + re.escape(pkg_path) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')
    true_rdep_packages = set()
    visibility_only_packages = set()
    gn_only_ref_packages = set()
    per_target_rdeps = {t: set() for t in bazel_targets}
    per_target_direct_rdeps = {t: {} for t in bazel_targets}
    has_macro_injected_rdeps = False

    for fpath, (no_comments, stripped) in file_cache.items():
        rel = os.path.relpath(fpath, workdir)
        caller_pkg = os.path.dirname(rel).strip("/")
        if caller_pkg == pkg_path:
            continue

        rel_sub = None
        if pkg_path.startswith(caller_pkg + "/"):
            rel_sub = pkg_path[len(caller_pkg) + 1 :]
        rel_label_re = (
            re.compile(r'"' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')
            if rel_sub
            else None
        )

        if f"//{pkg_path}" not in no_comments and (not rel_sub or rel_sub not in no_comments):
            continue

        if os.path.basename(rel) == "visibility.gni":
            if abs_label_re.search(no_comments):
                visibility_only_packages.add(caller_pkg)
            continue

        all_matches = list(abs_label_re.finditer(no_comments)) + (
            list(rel_label_re.finditer(no_comments)) if rel_label_re else []
        )
        if not all_matches:
            continue

        dep_matches = list(abs_label_re.finditer(stripped)) + (
            list(rel_label_re.finditer(stripped)) if rel_label_re else []
        )
        if not dep_matches:
            visibility_only_packages.add(caller_pkg)
            continue

        for m in dep_matches:
            sub = m.group(1) or default_target
            if sub in gn_only_targets and sub not in bazel_targets:
                gn_only_ref_packages.add(caller_pkg)
                continue
            if rel.endswith((".gn", ".gni")) and (sub in non_gn_synced_targets or sub in gn_nonconvertible_targets):
                gn_only_ref_packages.add(caller_pkg)
                continue
            is_pkg_target = sub in package_targets or (
                sub == default_target and len(bazel_targets) == 1 and bazel_targets[0] in package_targets
            )
            if is_pkg_target and rel.endswith((".gn", ".gni")) and not caller_pkg.startswith(("bundles/assembly", "build/images")):
                gn_only_ref_packages.add(caller_pkg)
                continue
            norm_caller = (
                "bundles/assembly"
                if (caller_pkg == "bundles/assembly" or caller_pkg.startswith("bundles/assembly/"))
                else caller_pkg
            )
            if caller_pkg.startswith("build/") and (rel.endswith(".bzl") or rel.endswith(".gni")) and "verification" not in os.path.basename(rel):
                has_macro_injected_rdeps = True
            true_rdep_packages.add(norm_caller)
            owner = owning_bazel_target(sub, bazel_targets)
            if owner is None and sub == default_target and len(bazel_targets) == 1:
                owner = bazel_targets[0]
            if owner is not None:
                per_target_rdeps[owner].add(norm_caller)
                # GN and Bazel enforce visibility in the package whose BUILD file names the
                # label (not in the .gni/.bzl that a template reference comes from).
                if os.path.basename(rel) in ("BUILD.gn", "BUILD.bazel"):
                    kind_bs = "gn" if rel.endswith(".gn") else "bazel"
                    per_target_direct_rdeps[owner].setdefault(caller_pkg, set()).add(kind_bs)

    for alias_name, actual_name in alias_map.items():
        if alias_name in per_target_rdeps and actual_name in per_target_rdeps:
            combined = per_target_rdeps[alias_name] | per_target_rdeps[actual_name]
            per_target_rdeps[alias_name] = set(combined)
            per_target_rdeps[actual_name] = set(combined)

    visibility_only_packages -= true_rdep_packages
    gn_only_ref_packages -= true_rdep_packages
    return true_rdep_packages, per_target_rdeps, per_target_direct_rdeps, visibility_only_packages, gn_only_ref_packages, has_macro_injected_rdeps

def extract_list_entries(expr, var_table, default_lineno):
    if isinstance(expr, ast.List):
        res = []
        for elt in expr.elts:
            if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                res.append((elt.value, getattr(elt, "lineno", default_lineno)))
        return res
    if isinstance(expr, ast.Name) and expr.id in var_table:
        return extract_list_entries(var_table[expr.id], var_table, default_lineno)
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
        return (
            extract_list_entries(expr.left, var_table, default_lineno)
            + extract_list_entries(expr.right, var_table, default_lineno)
        )
    return None

findings = []

for rel_path in sorted(candidate_files):
    full_path = os.path.join(workdir, rel_path)
    pkg_path = os.path.dirname(rel_path).strip("/")
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            src = f.read()
        tree = ast.parse(src, filename=rel_path)
    except Exception:
        continue
    # Generated files (e.g. crate_universe's third_party/rust_crates/vendor/*/BUILD.bazel)
    # get their visibility from the generator; fix the generator input instead.
    if re.search(r"^#\s*[@]generated\b", "\n".join(src.splitlines()[:10]), flags=re.MULTILINE):
        continue

    preexisting_vis, preexisting_no_vis_targets = get_preexisting_vis_info(rel_path)
    orig_gn_vis_map = get_orig_gn_visibility_scopes(pkg_path)
    var_table = {}
    bazel_targets = []
    package_targets = set()
    non_gn_synced_targets = set()
    alias_map = {}
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    var_table[t.id] = node.value

    for node in ast.walk(tree):
        if isinstance(node, ast.Call):
            func = (
                node.func.id
                if isinstance(node.func, ast.Name)
                else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
            )
            tname = None
            actual = None
            for kw in node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    tname = kw.value.value
                elif kw.arg == "actual" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    actual = kw.value.value
            if tname:
                bazel_targets.append(tname)
                if func in ("fx_package", "fuchsia_package", "fx_unittest_package"):
                    package_targets.add(tname)
                if func in BAZEL_NON_GN_SYNCED_RULES:
                    non_gn_synced_targets.add(tname)
                if func == "alias" and actual and actual.startswith(":"):
                    alias_map[tname] = actual[1:]

    true_rdeps, per_target_rdeps, per_target_direct_rdeps, vis_only_pkgs, gn_only_pkgs, has_macro_injected_rdeps = compute_package_rdeps(
        pkg_path, bazel_targets, package_targets, non_gn_synced_targets, alias_map
    )

    has_pkg_default_vis = False
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func_name = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )

        if func_name == "package":
            for kw in node.keywords:
                if kw.arg == "default_visibility":
                    has_pkg_default_vis = True
                    rec_pkg = recommend_visibility(true_rdeps, pkg_path)
                    findings.append({
                        "source": "visibility_audit",
                        "category": "visibility_scoping",
                        "severity": "error",
                        "file": rel_path,
                        "line": kw.lineno,
                        "message": "Do not set package-level 'default_visibility' in package(...). Declare 'visibility' explicitly on individual targets.",
                        "remediation": f"Remove default_visibility from package() and set target-level visibility = {json.dumps(rec_pkg)} on targets with external callers."
                    })
            continue

        if func_name == "exports_files":
            if not any(kw.arg == "visibility" for kw in node.keywords) and len(node.args) < 2:
                findings.append({
                    "source": "visibility_audit",
                    "category": "exports_files_missing_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": node.lineno,
                    "message": "exports_files() omits 'visibility' (Bazel then exports the files publicly).",
                    "remediation": "Add visibility = [...] to exports_files(), scoped as narrowly as practical to the packages that consume the files (e.g. [\"//<consumer_pkg>:__pkg__\"], or an area rollup like \"//<area>:__subpackages__\" for many consumers), per the determining_bazel_visibility skill."
                })
            continue

        target_name = None
        vis_kw = None
        is_testonly = False
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                target_name = kw.value.value
            elif kw.arg == "visibility":
                vis_kw = kw
            elif kw.arg == "testonly":
                if isinstance(kw.value, ast.Constant) and kw.value.value is True:
                    is_testonly = True
                elif isinstance(kw.value, ast.Name) and kw.value.id in ("True", "true"):
                    is_testonly = True

        if not target_name:
            continue

        is_test_rule = (
            func_name in (
                "host_rustc_test", "host_test", "wrap_host_rust_test",
                "fx_test", "fx_unittest_package", "fx_test_package",
                "rustc_test", "rust_test", "cc_test", "fx_cc_test", "go_test",
            )
            or func_name.startswith("wrap_host_")
            or func_name.endswith("_test")
        )
        is_test_support_pkg = (
            rel_path.startswith("src/testing/")
            or rel_path.startswith("src/lib/testing/")
            or rel_path.startswith("testing/")
            or "/testing/" in rel_path
        )
        target_rdeps = per_target_rdeps.get(target_name, true_rdeps)
        distinct_areas = get_distinct_areas(target_rdeps, pkg_path)
        # Shared test-support targets may use broad visibility only when they are reused across
        # >= 2 areas, the same rule find_rdeps.sh uses to recommend ["//:__subpackages__"].
        allow_broad_test_vis = (is_testonly or is_test_support_pkg or is_test_rule) and len(distinct_areas) >= 2

        narrow_vis = recommend_narrow_visibility(target_rdeps, pkg_path)
        area_rollup_vis = recommend_area_rollup_visibility(target_rdeps, pkg_path)
        rec_target_vis = recommend_visibility(target_rdeps, pkg_path, is_test_support=allow_broad_test_vis)
        is_globally_used = is_globally_used_target(target_rdeps, pkg_path)

        if vis_kw is None:
            if not has_pkg_default_vis and len(target_rdeps) > 0 and target_name not in preexisting_no_vis_targets:
                findings.append({
                    "source": "visibility_audit",
                    "category": "missing_target_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": node.lineno,
                    "message": f"Target '{target_name}' has {len(target_rdeps)} external reverse-dependency package(s) but omits 'visibility' (defaulting to private in Bazel and unrestricted in bazel2gn).",
                    "remediation": f"Add visibility = {json.dumps(rec_target_vis)} to '{target_name}'."
                })
            continue

        entries = extract_list_entries(vis_kw.value, var_table, vis_kw.lineno)
        if entries is None:
            continue

        vis_vals = [val for (val, _) in entries]
        vis_scopes = []
        only_scope_entries = True
        for val in vis_vals:
            if val == "//visibility:private":
                continue
            if val == "//visibility:public":
                only_scope_entries = False
                break
            m_scope = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", val)
            if not m_scope:
                # package_group or other label: coverage cannot be decided statically.
                only_scope_entries = False
                break
            scope_pkg = pkg_path if m_scope.group(1) is None else m_scope.group(1).strip("/")
            vis_scopes.append((scope_pkg, m_scope.group(2)))
        # bazel2gn emits the Bazel list as GN visibility unless the closing line carries
        # '# @bazel2gn:raw_overwrite:[ ... ]' (GN patterns '//*', '//a/*', '//a:*').
        gn_scopes = vis_scopes
        end_line = getattr(vis_kw.value, "end_lineno", None)
        src_lines = src.splitlines()
        if end_line and end_line <= len(src_lines):
            m_raw = re.search(r"#\s*@bazel2gn:raw_overwrite:\s*(\[.*\])", src_lines[end_line - 1])
            if m_raw:
                try:
                    gn_scopes = []
                    for pat in json.loads(m_raw.group(1)):
                        m_gn = re.match(r"^//([^:*]*?)/?(\*|:.*)$", pat)
                        if not m_gn:
                            gn_scopes = None
                            break
                        gn_pkg = m_gn.group(1).strip("/")
                        gn_scopes.append((gn_pkg, "subpackages" if m_gn.group(2) == "*" else "pkg"))
                except Exception:
                    gn_scopes = None
        if only_scope_entries:
            uncovered = set()
            for caller, kinds in per_target_direct_rdeps.get(target_name, {}).items():
                for kind_bs in kinds:
                    scopes = gn_scopes if kind_bs == "gn" else vis_scopes
                    if scopes is None:
                        continue
                    covered = any(
                        caller == sp or (kind == "subpackages" and (sp == "" or caller.startswith(sp + "/")))
                        for sp, kind in scopes
                    )
                    if not covered:
                        uncovered.add(caller)
            if uncovered:
                shown = sorted(
                    {
                        f"//{VG_ROOT}" if (c == VG_ROOT or c.startswith(VG_ROOT + "/")) else f"//{c}"
                        for c in uncovered
                    }
                )
                add_entries = recommend_narrow_visibility(uncovered, pkg_path)
                findings.append({
                    "source": "visibility_audit",
                    "category": "uncovered_rdep_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": vis_kw.lineno,
                    "message": (
                        f"Target '{target_name}' has visibility that does not cover {len(shown)} package(s) whose "
                        f"BUILD.gn/BUILD.bazel depend on it or on a sub-target it generates "
                        f"(e.g. '{target_name}_rust', '{target_name}_cpp'): {', '.join(shown)}. GN enforces the "
                        f"converted visibility on every generated sub-target, and callers outside the locally "
                        f"built product graph (other products, the internal tree) only fail in CQ with "
                        f"'Dependency not allowed'."
                    ),
                    "remediation": (
                        f"Add {json.dumps(add_entries)} to the visibility of '{target_name}' (or use the "
                        f"find_rdeps per_target_recommended_visibility {json.dumps(rec_target_vis)}) and re-run "
                        f"fx bazel2gn."
                    ),
                })

        new_entries = [(val, lineno) for (val, lineno) in entries if val not in preexisting_vis]
        if len(new_entries) > 15 and (is_globally_used or len(area_rollup_vis) < len(entries)):
            if is_globally_used:
                findings.append({
                    "source": "visibility_audit",
                    "category": "overly_narrow_global_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": vis_kw.lineno,
                    "message": (
                        f"Target '{target_name}' is widely used across {len(distinct_areas)} distinct areas "
                        f"({len(target_rdeps)} reverse-dependency packages) but enumerates {len(entries)} narrow "
                        f"visibility entries ('too narrow' for a widely-used platform library/test utility)."
                    ),
                    "remediation": (
                        f"Replace the sprawling {len(entries)}-entry visibility list on '{target_name}' with "
                        f"subsystem/top-level rollup visibility {json.dumps(rec_target_vis)} (or "
                        f"visibility = [\"//:__subpackages__\"] for foundational tree-wide libraries), per "
                        f"determining_bazel_visibility/SKILL.md, and re-run fx bazel2gn."
                    ),
                })
            else:
                findings.append({
                    "source": "visibility_audit",
                    "category": "overly_narrow_global_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": vis_kw.lineno,
                    "message": (
                        f"Target '{target_name}' enumerates {len(entries)} narrow visibility entries across multiple "
                        f"subtrees. Drop visibility to 2-3 levels deep instead of listing dozens of leaf packages."
                    ),
                    "remediation": (
                        f"Replace visibility on '{target_name}' with 2-3 level area rollup entries: "
                        f"{json.dumps(area_rollup_vis)} and re-run fx bazel2gn."
                    ),
                })
            continue

        findings_before_target = len(findings)
        max_rollup_entries = 15 if is_globally_used else max(15, len(area_rollup_vis))
        allow_broad_rollup = (is_globally_used or len(narrow_vis) > 5 or len(distinct_areas) >= 3 or allow_broad_test_vis) and len(entries) <= max_rollup_entries
        top_level_subpkgs = []
        for val, lineno in entries:
            if val in preexisting_vis:
                continue
            if val == "//visibility:private":
                # Check redundant //visibility:private when package has no default_visibility.
                if not has_pkg_default_vis:
                    findings.append({
                        "source": "visibility_audit",
                        "category": "redundant_private_visibility",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Target '{target_name}' specifies redundant "
                            "`visibility = [\"//visibility:private\"]`, which is already the "
                            "Bazel default when `default_visibility` is not set."
                        ),
                        "remediation": (
                            f"Remove `visibility = [\"//visibility:private\"]` from '{target_name}' "
                            f"in '{rel_path}'."
                        ),
                    })
                continue
            if val == "//visibility:public":
                if rel_path.startswith("build/bazel_sdk/"):
                    continue
                if is_test_rule:
                    # Test entry points (fx_unittest_package, fx_test, host_*_test, ...) are built as
                    # top-level Bazel targets by the GN bazel_test_suite() bridge and by fx test, and
                    # Bazel never visibility-checks top-level targets. They only need visibility when
                    # a Bazel test_suite() in another package aggregates them.
                    if target_rdeps and narrow_vis:
                        rem = (
                            f"Replace '//visibility:public' on test target '{target_name}' with only the "
                            f"packages whose Bazel test_suite() aggregates it: {json.dumps(narrow_vis)}, and "
                            f"re-run fx bazel2gn."
                        )
                    elif has_macro_injected_rdeps:
                        rem = (
                            f"Replace '//visibility:public' on test target '{target_name}' with the specific "
                            f"packages that aggregate it (e.g. [\"//<parent>:__pkg__\"]), and re-run fx bazel2gn."
                        )
                    else:
                        rem = (
                            f"Omit 'visibility' on test target '{target_name}' (default private) and re-run fx "
                            f"bazel2gn. The GN bazel_test_suite() bridge and fx test build it as a top-level "
                            f"Bazel target, which Bazel does not visibility-check, and no other Bazel package "
                            f"depends on it."
                        )
                elif allow_broad_test_vis:
                    rem = (
                        f"Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        f"determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        f"[\"//:__subpackages__\"] for test support libraries and re-run fx bazel2gn."
                    )
                elif len(target_rdeps) == 0 and not has_macro_injected_rdeps:
                    rem = f"Omit 'visibility' on '{target_name}' (default private) as no external packages depend on it, and re-run fx bazel2gn."
                elif is_globally_used or has_macro_injected_rdeps:
                    rem = (
                        f"Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        f"determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        f"subsystem-scoped visibility {json.dumps(rec_target_vis)} (or "
                        f"[\"//:__subpackages__\"] for foundational tree-wide libraries) and re-run fx bazel2gn."
                    )
                else:
                    rem = (
                        f"Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        f"determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        f"tightly-scoped caller visibility: {json.dumps(rec_target_vis)} and re-run fx bazel2gn."
                    )
                findings.append({
                    "source": "visibility_audit",
                    "category": "overbroad_public_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' uses '//visibility:public', which is disallowed outside "
                        f"//build/bazel_sdk/... ({len(target_rdeps)} package(s) across {len(distinct_areas)} "
                        f"area(s) depend on '{target_name}')."
                    ),
                    "remediation": rem,
                })
                continue
            if val in (":__pkg__", f"//{pkg_path}:__pkg__"):
                findings.append({
                    "source": "visibility_audit",
                    "category": "redundant_same_package_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": f"Target '{target_name}' includes redundant '{val}' in visibility. Same-package visibility is implicit in Bazel and added automatically by bazel2gn.",
                    "remediation": f"Remove '{val}' from visibility."
                })
            elif val == "//:__subpackages__" and is_test_rule and not target_rdeps and not has_macro_injected_rdeps:
                findings.append({
                    "source": "visibility_audit",
                    "category": "overbroad_test_target_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Test target '{target_name}' uses '//:__subpackages__' in visibility, but no Bazel "
                        f"package depends on it. Test entry points are built as top-level targets by the GN "
                        f"bazel_test_suite() bridge and fx test, which Bazel does not visibility-check."
                    ),
                    "remediation": f"Omit 'visibility' on '{target_name}' (default private) and re-run fx bazel2gn.",
                })
            elif val == "//:__subpackages__":
                if not is_globally_used and not has_macro_injected_rdeps and not allow_broad_test_vis:
                    findings.append({
                        "source": "visibility_audit",
                        "category": "disguised_public_visibility",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Target '{target_name}' uses '//:__subpackages__' in visibility, but only "
                            f"{len(target_rdeps)} package(s) across {len(distinct_areas)} area(s) depend on '{target_name}'. "
                            f"Reserve '//:__subpackages__' for foundational libraries with widespread tree-wide usage."
                        ),
                        "remediation": f"Replace '//:__subpackages__' with {json.dumps(rec_target_vis)} and re-run fx bazel2gn."
                    })
            else:
                if val.startswith("//" + VG_ROOT + "/"):
                    findings.append({
                        "source": "visibility_audit",
                        "category": "internal_vendor_subpath_visibility",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Target '{target_name}' exposes internal vendor subpath '{val}' in visibility. "
                            f"Never use internal vendor subpaths in visibility; expose to '{VG_SUBPKGS}' instead."
                        ),
                        "remediation": (
                            f"Replace '{val}' with '\"{VG_SUBPKGS}\"' and re-run fx bazel2gn."
                        ),
                    })
                    continue

                m_sub = re.match(r"^//([^:]+):__subpackages__$", val)
                m_pkg = re.match(r"^//([^:]+):__pkg__$", val)

                if m_sub:
                    prefix = m_sub.group(1).strip("/")
                    if prefix == VG_ROOT:
                        has_vg_rdep = any(
                            r == VG_ROOT or r.startswith(VG_ROOT + "/") for r in target_rdeps
                        )
                        if not has_vg_rdep and not has_macro_injected_rdeps:
                            findings.append({
                                "source": "visibility_audit",
                                "category": "false_positive_rdep_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' grants visibility to '{val}', but no package in that vendor tree actually depends on '{target_name}'.",
                                "remediation": f"Remove '{val}' from visibility.",
                            })
                        continue
                    parts = prefix.split("/")
                    depth = len(parts)
                    covered_rdeps = sorted(
                        r for r in target_rdeps if r == prefix or r.startswith(prefix + "/")
                    )
                    replacement = recommend_narrow_visibility(covered_rdeps, pkg_path)

                    if depth == 1:
                        top_level_subpkgs.append((val, lineno))
                        if prefix in BANNED_TOP_LEVEL_SUBPKGS and not is_globally_used and not allow_broad_rollup and not allow_broad_test_vis:
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_top_level_subpackages_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' grants top-level wildcard '{val}' in visibility ('All of //{prefix} seems too wide').",
                                "remediation": (
                                    f"Replace '{val}' with tightly-scoped package/subsystem entries: {json.dumps(replacement)}."
                                    if replacement
                                    else f"Remove '{val}' from visibility (no true reverse dependencies exist under '//{prefix}')."
                                ),
                            })
                            continue

                    if prefix in BANNED_UMBRELLA_SUBPKGS:
                        if not allow_broad_test_vis and not is_globally_used:
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_subpackages_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' grants multi-domain umbrella wildcard '{val}' in visibility ('//{prefix} seems too wide').",
                                "remediation": (
                                    f"Replace '{val}' with specific library/package visibility entries: {json.dumps(replacement)}."
                                    if replacement
                                    else f"Remove '{val}' from visibility (no true reverse dependencies exist under '//{prefix}')."
                                ),
                            })
                            continue
                        if not allow_broad_test_vis and covered_rdeps:
                            # docs/development/languages/bazel/style/build_files.md#target-visibility
                            # suggests top-level directories such as //sdk/lib:__subpackages__ for
                            # targets used across the code base, so on globally-used targets this is
                            # only a hint for a human to confirm. A grant with no caller under the
                            # prefix still falls through to false_positive_rdep_visibility below.
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_subpackages_visibility",
                                "severity": "info",
                                "file": rel_path,
                                "line": lineno,
                                "message": (
                                    f"Globally used target '{target_name}' grants multi-domain umbrella wildcard "
                                    f"'{val}' in visibility."
                                ),
                                "remediation": (
                                    f"No change needed if '{target_name}' is meant for use across '//{prefix}' "
                                    "(a human reviewer should confirm)."
                                ),
                            })
                            continue

                    if len(covered_rdeps) == 0:
                        if not has_macro_injected_rdeps:
                            findings.append({
                                "source": "visibility_audit",
                                "category": "false_positive_rdep_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' grants visibility to '{val}', but no package under '//{prefix}' actually depends on '{target_name}'.",
                                "remediation": f"Remove '{val}' from visibility."
                            })
                        continue

                    if allow_broad_rollup:
                        continue

                    if depth == 2 and parts[0] in ("src", "sdk"):
                        same_area = pkg_path == prefix or pkg_path.startswith(prefix + "/")
                        if not same_area:
                            d3_subtrees = {
                                "/".join(r.split("/")[:3]) for r in covered_rdeps if len(r.split("/")) >= 3
                            }
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_subpackages_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' (in '//{pkg_path}') grants cross-area wildcard '{val}' ('//{prefix} seems too wide'; only {len(covered_rdeps)} caller package(s) across {len(d3_subtrees)} subcomponent(s) depend on '{target_name}').",
                                "remediation": f"Replace '{val}' with narrower package/subcomponent visibility entries: {json.dumps(replacement)}."
                            })
                            continue

                    if depth >= 3 and not prefix.startswith(("build/bazel/", "build/images/", "bundles/")):
                        split_cov = [r.split("/") for r in covered_rdeps]
                        min_len = min(len(s) for s in split_cov)
                        lca_len = 0
                        for i in range(min_len):
                            if len({s[i] for s in split_cov}) == 1:
                                lca_len = i + 1
                            else:
                                break
                        lca = "/".join(split_cov[0][:lca_len])

                        if len(covered_rdeps) == 1:
                            only_pkg = covered_rdeps[0]
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_subpackages_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' uses wildcard '{val}', but only a single package ('//{only_pkg}') under '//{prefix}' depends on '{target_name}'.",
                                "remediation": f"Replace '{val}' with '\"//{only_pkg}:__pkg__\"'."
                            })
                        elif lca != prefix and lca.startswith(prefix + "/"):
                            findings.append({
                                "source": "visibility_audit",
                                "category": "overbroad_subpackages_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": f"Target '{target_name}' uses '{val}', which is wider than the Lowest Common Ancestor ('//{lca}') of its actual callers under '//{prefix}'.",
                                "remediation": f"Replace '{val}' with {json.dumps(replacement)}."
                            })

                elif m_pkg:
                    caller_pkg = m_pkg.group(1).strip("/")
                    if caller_pkg == "visibility":
                        continue
                    if caller_pkg in vis_only_pkgs:
                        findings.append({
                            "source": "visibility_audit",
                            "category": "visibility_allowlist_false_rdep",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": f"Target '{target_name}' grants visibility to '{val}', but '//{caller_pkg}' only references '//{pkg_path}' inside its own visibility allowlist.",
                            "remediation": f"Remove '{val}' from visibility."
                        })
                    elif caller_pkg in gn_only_pkgs or caller_pkg not in target_rdeps:
                        findings.append({
                            "source": "visibility_audit",
                            "category": "false_positive_rdep_visibility",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": f"Target '{target_name}' grants visibility to '{val}', but '//{caller_pkg}' does not depend on Bazel target '{target_name}' (e.g. only references :tests or :benchmarks).",
                            "remediation": f"Remove '{val}' from visibility."
                        })

        if len(top_level_subpkgs) >= 5 and not allow_broad_rollup:
            findings.append({
                "source": "visibility_audit",
                "category": "disguised_public_visibility",
                "severity": "error",
                "file": rel_path,
                "line": vis_kw.lineno,
                "message": f"Target '{target_name}' enumerates {len(top_level_subpkgs)} top-level '//<dir>:__subpackages__' roots in visibility.",
                "remediation": f"Scope visibility to {json.dumps(rec_target_vis)}."
            })

        if target_name in orig_gn_vis_map and len(findings) == findings_before_target:
            gn_allowed = orig_gn_vis_map[target_name]
            all_rdeps_in_gn = all(
                any(
                    r == gsp or (gk == "subpackages" and (gsp == "" or r.startswith(gsp + "/")))
                    for gsp, gk in gn_allowed
                )
                for r in target_rdeps
            )
            if all_rdeps_in_gn:
                for val, lineno in entries:
                    if val in preexisting_vis or val == "//visibility:private" or val in rec_target_vis:
                        continue
                    m_scope = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", val)
                    if not m_scope:
                        continue
                    sp = pkg_path if m_scope.group(1) is None else m_scope.group(1).strip("/")
                    kind = m_scope.group(2)
                    if kind == "pkg":
                        covered_by_gn = any(
                            sp == gsp or (gk == "subpackages" and (gsp == "" or sp.startswith(gsp + "/")))
                            for gsp, gk in gn_allowed
                        )
                    else:
                        covered_by_gn = any(
                            gk == "subpackages" and (gsp == "" or sp == gsp or sp.startswith(gsp + "/"))
                            for gsp, gk in gn_allowed
                        )
                    if not covered_by_gn:
                        gn_bounds_desc = sorted(
                            {f"//{gsp}/*" if gk == "subpackages" else f"//{gsp}:*" for gsp, gk in gn_allowed}
                        )
                        findings.append({
                            "source": "visibility_audit",
                            "category": "visibility_widened_beyond_gn",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": (
                                f"Target '{target_name}' widens visibility with '{val}' beyond the explicit "
                                f"visibility bounds in `{pkg_path}/BUILD.gn` at `{change_base}` ({', '.join(gn_bounds_desc)}), "
                                f"even though all actual reverse dependencies are within the original GN scope."
                            ),
                            "remediation": (
                                f"Scope visibility on '{target_name}' to {json.dumps(rec_target_vis)} (or omit "
                                "visibility if private to the package) and re-run fx bazel2gn."
                            ),
                        })

print(json.dumps(findings, indent=2))
PYEOF
