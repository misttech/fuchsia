#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Helper tool to discover true reverse dependencies (deps, public_deps, test_deps, proc_macro_deps, link_deps, actual)
# of a target package directory while filtering out:
# 1. References inside visibility = [...] or visibility.gni allowlists
# 2. Substring prefix collisions with sibling packages (e.g. //a/b-foo or //a/b/subpkg)
# 3. References to GN-only targets above #LOCAL_BAZEL_BUILD_SENTINEL (:tests, :benchmarks)
# Also detects relative GN label references from ancestor BUILD.gn files and computes deterministic,
# tiered Bazel visibility lists per target:
# - Tier 1 (Narrow / Subsystem-Scoped, <= 15 entries): exact :__pkg__ and depth >= 3 LCA :__subpackages__
# - Tier 2 (Multi-Subsystem Broad Usage, > 15 narrow entries): 2-3 level subsystem area rollups (<= 15 entries)
# - Tier 3 (Foundational Tree-Wide Library, > 15 subsystem area rollup entries): ["//:__subpackages__"]

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${1:-${PLANTER_TARGET_DIR:-}}"

if [[ -z "$TARGET_DIR" ]]; then
  echo "Usage: find_rdeps.sh <target_package_dir>" >&2
  exit 1
fi

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_pkg = sys.argv[2].strip().strip("/")
default_target = os.path.basename(target_pkg)

bazel_file = os.path.join(workdir, target_pkg, "BUILD.bazel")
gn_file = os.path.join(workdir, target_pkg, "BUILD.gn")

bazel_targets = []
package_targets = set()
non_gn_synced_targets = set()
alias_map = {}

BAZEL_NON_GN_SYNCED_RULES = {
    "fx_component",
    "fx_component_manifest",
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
    "wrap_host_rust_test",
    "go_test",
}

if os.path.isfile(bazel_file):
    try:
        with open(bazel_file, "r", encoding="utf-8") as f:
            tree = ast.parse(f.read(), filename=bazel_file)
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

GN_PACKAGE_TEMPLATES = {
    "fuchsia_package",
    "fuchsia_package_with_single_component",
}

gn_only_targets = {"tests", "benchmarks"}
gn_nonconvertible_targets = {"tests", "benchmarks"}
if os.path.isfile(gn_file):
    try:
        with open(gn_file, "r", encoding="utf-8", errors="ignore") as f:
            gn_src = f.read()
        for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE):
            if m.group(1) in GN_PACKAGE_TEMPLATES:
                package_targets.add(m.group(2))
        if bazel_targets:
            above_sentinel = gn_src.split("BAZEL2GN SENTINEL")[0] if "BAZEL2GN SENTINEL" in gn_src else gn_src
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', above_sentinel, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if t not in bazel_targets:
                    gn_only_targets.add(t)
                if tmpl not in CONVERTIBLE_GN_TEMPLATES:
                    gn_nonconvertible_targets.add(t)
        else:
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if tmpl in CONVERTIBLE_GN_TEMPLATES and t not in bazel_targets:
                    bazel_targets.append(t)
                elif tmpl not in CONVERTIBLE_GN_TEMPLATES and t not in bazel_targets:
                    gn_only_targets.add(t)
                    gn_nonconvertible_targets.add(t)
    except Exception:
        pass

label_prefix = f"//{target_pkg}"
files = []
try:
    out = subprocess.check_output(
        ["rg", "-l", "--fixed-strings", label_prefix, "-g", "*BUILD.gn", "-g", "*BUILD.bazel", "-g", "*.gni", "-g", "*.bzl", workdir],
        stderr=subprocess.DEVNULL,
        text=True,
    )
    files = [line.strip() for line in out.splitlines() if line.strip()]
except Exception:
    for repo in [workdir, os.path.join(workdir, "vendor/" + "google")]:
        if os.path.isdir(os.path.join(repo, ".git")):
            try:
                out = subprocess.check_output(
                    ["git", "-C", repo, "grep", "-l", "--fixed-strings", label_prefix, "--", "*BUILD.gn", "*BUILD.bazel", "*.gni", "*.bzl"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                for line in out.splitlines():
                    if line.strip():
                        files.append(os.path.join(repo, line.strip()))
            except Exception:
                pass

# Also include ancestor BUILD.gn files that may reference target_pkg via relative GN labels
pkg_parts = target_pkg.split("/")
for i in range(1, len(pkg_parts)):
    anc_gn = os.path.join(workdir, "/".join(pkg_parts[:i]), "BUILD.gn")
    if os.path.isfile(anc_gn):
        files.append(anc_gn)

abs_label_re = re.compile(r'"//' + re.escape(target_pkg) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')

true_rdep_packages = set()
visibility_only_packages = set()
excluded_gn_only_references = set()
per_target_rdeps = {t: set() for t in bazel_targets}

def owning_bazel_target(sub):
    # Rules such as fidl_library emit sub-targets named '<name>_<suffix>' (e.g. '<fidl>_rust',
    # '<fidl>_cpp') that share the visibility of '<name>'; attribute them to the longest match.
    if sub in per_target_rdeps:
        return sub
    best = None
    for t in bazel_targets:
        if sub.startswith(t + "_") and (best is None or len(t) > len(best)):
            best = t
    return best

for fpath in sorted(set(files)):
    rel = os.path.relpath(fpath, workdir)
    pkg = os.path.dirname(rel).strip("/")
    if pkg == target_pkg:
        continue
    if os.path.basename(rel) == "visibility.gni":
        visibility_only_packages.add(pkg)
        continue
    try:
        with open(fpath, "r", encoding="utf-8", errors="ignore") as f:
            content = f.read()
    except Exception:
        continue
    no_comments = re.sub(r"#[^\n]*", "", content)
    stripped = re.sub(r"(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]", "", no_comments, flags=re.DOTALL)

    rel_sub = None
    if target_pkg.startswith(pkg + "/"):
        rel_sub = target_pkg[len(pkg) + 1 :]
    rel_label_re = (
        re.compile(r'"' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')
        if rel_sub
        else None
    )

    all_matches = list(abs_label_re.finditer(no_comments)) + (
        list(rel_label_re.finditer(no_comments)) if rel_label_re else []
    )
    if not all_matches:
        continue

    dep_matches = list(abs_label_re.finditer(stripped)) + (
        list(rel_label_re.finditer(stripped)) if rel_label_re else []
    )
    if not dep_matches:
        visibility_only_packages.add(pkg)
        continue

    for m in dep_matches:
        sub = m.group(1) or default_target
        if sub in gn_only_targets and sub not in bazel_targets:
            excluded_gn_only_references.add(f"//{pkg} (:{sub})")
            continue
        if rel.endswith((".gn", ".gni")) and (sub in non_gn_synced_targets or sub in gn_nonconvertible_targets):
            excluded_gn_only_references.add(f"//{pkg} (:{sub})")
            continue
        is_pkg_target = sub in package_targets or (
            sub == default_target and len(bazel_targets) == 1 and bazel_targets[0] in package_targets
        )
        caller_pkg = (
            "bundles/assembly"
            if (pkg == "bundles/assembly" or pkg.startswith("bundles/assembly/"))
            else pkg
        )
        if is_pkg_target and rel.endswith((".gn", ".gni")) and not pkg.startswith(("bundles/assembly", "build/images")):
            excluded_gn_only_references.add(f"//{pkg} (:{sub})")
            continue
        true_rdep_packages.add(caller_pkg)
        owner = owning_bazel_target(sub)
        if owner is not None:
            per_target_rdeps[owner].add(caller_pkg)
        elif sub == default_target and len(bazel_targets) == 1:
            per_target_rdeps[bazel_targets[0]].add(caller_pkg)

for alias_name, actual_name in alias_map.items():
    if alias_name in per_target_rdeps and actual_name in per_target_rdeps:
        combined = per_target_rdeps[alias_name] | per_target_rdeps[actual_name]
        per_target_rdeps[alias_name] = set(combined)
        per_target_rdeps[actual_name] = set(combined)

visibility_only_packages -= true_rdep_packages

BANNED_UMBRELLA_AREAS = {
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

def get_distinct_areas(rdep_pkgs):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    return sorted({f"//{get_pkg_area(p)}" for p in pkgs if get_pkg_area(p)})

VG_ROOT = "vendor/" + "google"
VG_SUBPKGS = "//" + VG_ROOT + ":__subpackages__"

def sanitize_rdep_pkg(p):
    p = p.strip("/")
    if p == VG_ROOT or p.startswith(VG_ROOT + "/"):
        return VG_ROOT
    return p

def recommend_narrow_visibility(rdep_pkgs):
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
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

def recommend_area_rollup_visibility(rdep_pkgs):
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
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
        if area in BANNED_UMBRELLA_AREAS:
            for item in recommend_narrow_visibility(group):
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

def is_globally_used_target(rdep_pkgs):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    narrow = recommend_narrow_visibility(pkgs)
    areas = {get_pkg_area(p) for p in pkgs if get_pkg_area(p)}
    return len(areas) >= 6 and (len(narrow) > 15 or len(pkgs) >= 20)

def recommend_visibility(rdep_pkgs):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    narrow = recommend_narrow_visibility(pkgs)
    globally_used = is_globally_used_target(pkgs)
    if globally_used or len(narrow) > 8:
        rollup = recommend_area_rollup_visibility(pkgs)
        if globally_used and len(rollup) > 15:
            return ["//:__subpackages__"]
        if len(rollup) <= len(narrow):
            return rollup
    return narrow

print(json.dumps({
    "target_package": f"//{target_pkg}",
    "true_rdep_packages": sorted({f"//{sanitize_rdep_pkg(p)}" for p in true_rdep_packages}),
    "distinct_rdep_areas": get_distinct_areas(true_rdep_packages),
    "is_globally_used": is_globally_used_target(true_rdep_packages),
    "recommended_visibility": recommend_visibility(true_rdep_packages),
    "per_target_rdeps": {
        t: sorted({f"//{sanitize_rdep_pkg(p)}" for p in rset}) for t, rset in per_target_rdeps.items()
    },
    "per_target_is_globally_used": {
        t: is_globally_used_target(rset) for t, rset in per_target_rdeps.items()
    },
    "per_target_recommended_visibility": {
        t: recommend_visibility(rset) for t, rset in per_target_rdeps.items()
    },
    "per_target_area_rollup_visibility": {
        t: recommend_area_rollup_visibility(rset) for t, rset in per_target_rdeps.items()
    },
    "per_target_narrow_visibility": {
        t: recommend_narrow_visibility(rset) for t, rset in per_target_rdeps.items()
    },
    "excluded_visibility_allowlist_packages": sorted({f"//{sanitize_rdep_pkg(p)}" for p in visibility_only_packages}),
    "excluded_gn_only_or_verification_references": sorted(excluded_gn_only_references),
}, indent=2))
PYEOF
