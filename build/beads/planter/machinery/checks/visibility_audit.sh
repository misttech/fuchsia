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
# 10. False-positive rdeps from visibility allowlists (visibility.gni / visibility = [...]) or GN-only targets (:tests, :benchmarks);
#     GN target_tests/host_tests lists count as allowlists only inside bazel_test_suite() blocks
# 11. Uncovered callers: a package whose BUILD.gn/BUILD.bazel depends on the target, or on a generated
#     '<target>_<suffix>' sub-target (e.g. fidl_library bindings '<fidl>_rust', '<fidl>_cpp'), that no
#     visibility entry covers. References to generated sub-targets count as rdeps of the owning target.

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" <<'PYEOF'
import ast
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

workdir = os.path.abspath(sys.argv[1])
def normalize_target_dir(raw_dir, base_workdir):
    d = raw_dir.strip()
    if d.startswith("//"):
        d = d[2:]
    elif os.path.isabs(d):
        rel = os.path.relpath(os.path.abspath(d), base_workdir)
        if rel != ".." and not rel.startswith("../"):
            d = rel
    while d.startswith("./"):
        d = d[2:]
    d = d.strip("/")
    return "" if d == "." else d


target_dir = normalize_target_dir(sys.argv[2], workdir)
target_dirs = [
    normalize_target_dir(d, workdir)
    for d in re.split(r"[\s,]+", f"{sys.argv[3]} {target_dir}")
    if normalize_target_dir(d, workdir)
]
target_bazel_files = {os.path.join(d, "BUILD.bazel") for d in target_dirs}

EXCLUDED_GLOBAL_VIS_PREFIXES = (
    "bundles/assembly/",
    "build/bazel/",
    "build/bazel2gn/",
    "build/images/",
    "build/config/rust/lints/",
    "third_party/rust_crates/",
)

candidate_files = set()
for rel_bazel in sorted(target_bazel_files):
    if os.path.isfile(os.path.join(workdir, rel_bazel)):
        candidate_files.add(rel_bazel)

change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
git_cmds = [
    ["git", "-C", workdir, "diff", "--name-only", change_base, "--", "*BUILD.bazel"],
    ["git", "-C", workdir, "diff", "--cached", "--name-only", "--", "*BUILD.bazel"],
    ["git", "-C", workdir, "ls-files", "--others", "--exclude-standard", "--", "*BUILD.bazel"],
]
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
                if d2 in BANNED_UMBRELLA_SUBPKGS:
                    for item in recommend_narrow_visibility(d2_group, pkg_path):
                        result.add(item)
                elif len(d2_group) >= 2 or d2 == "bundles/assembly":
                    result.add(f"//{d2}:__subpackages__")
                else:
                    result.add(f"//{d2_group[0]}:__pkg__")
    return sorted(result)

def is_globally_used_target(rdep_pkgs, pkg_path):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    narrow = recommend_narrow_visibility(pkgs, pkg_path)
    areas = get_distinct_areas(pkgs, pkg_path)
    return len(areas) >= 6 and (len(narrow) > 15 or len(pkgs) >= 20)

def collapse_src_rollup_visibility(rdep_pkgs, pkg_path):
    """Collapses //src/* callers into //src:__subpackages__ while keeping narrow entries outside //src."""
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    if not raw_pkgs:
        return []
    has_src = any(p == "src" or p.startswith("src/") for p in raw_pkgs)
    non_src = [p for p in raw_pkgs if p != "src" and not p.startswith("src/")]
    res = set(recommend_narrow_visibility(non_src, pkg_path))
    if has_src:
        res.add("//src:__subpackages__")
    return sorted(res)

# Detect target-level # @bazel2gn:skip annotations in BUILD.bazel
def has_bazel2gn_skip_comment(src_lines, call_lineno):
    """Returns True if the rule call at 1-based call_lineno is preceded by '# @bazel2gn:skip'."""
    idx = call_lineno - 2
    while idx >= 0:
        stripped_line = src_lines[idx].strip()
        if not stripped_line.startswith("#"):
            break
        if re.search(r"^#\s*@bazel2gn:skip\b", stripped_line):
            return True
        idx -= 1
    return False


TEST_OR_BENCH_CALLER_SEGMENTS = {
    "test",
    "tests",
    "testing",
    "bench",
    "benchmarks",
    "test_runners",
    "stress",
    "e2e",
}
TEST_OR_BENCH_CALLER_SUFFIXES = (
    "_test",
    "_tests",
    "_bench",
    "_benchmarks",
    "_stress",
)


# Check whether a caller package path is a test, benchmark, stress-test, or test-runner package.
def is_test_or_bench_caller_pkg(pkg):
    parts = [seg for seg in pkg.strip("/").split("/") if seg]
    return any(
        seg in TEST_OR_BENCH_CALLER_SEGMENTS
        or seg.endswith(TEST_OR_BENCH_CALLER_SUFFIXES)
        for seg in parts
    )


# Check whether all external caller packages across subsystems are test/benchmark packages.
def has_only_test_or_bench_rdeps(rdep_pkgs, pkg_path):
    pkgs = [
        p.strip("/")
        for p in rdep_pkgs
        if p.strip("/")
        and p.strip("/") != pkg_path
        and p.strip("/") != VG_ROOT
        and not p.strip("/").startswith(VG_ROOT + "/")
    ]
    return bool(pkgs) and all(is_test_or_bench_caller_pkg(p) for p in pkgs)


def recommend_visibility(rdep_pkgs, pkg_path, is_test_support=False):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path))
    narrow = recommend_narrow_visibility(pkgs, pkg_path)
    globally_used = is_globally_used_target(pkgs, pkg_path)
    if is_test_support and len(get_distinct_areas(pkgs, pkg_path)) >= 2:
        return ["//:__subpackages__"]
    if globally_used or len(narrow) > 8:
        rollup = recommend_area_rollup_visibility(pkgs, pkg_path)
        distinct_areas = get_distinct_areas(pkgs, pkg_path)
        if globally_used and len(rollup) > 15 and (len(pkgs) >= 50 or len(distinct_areas) > 15):
            if len(pkgs) >= 50:
                return ["//:__subpackages__"]
            src_collapsed = collapse_src_rollup_visibility(pkgs, pkg_path)
            if len(src_collapsed) <= 15:
                return src_collapsed
            return ["//:__subpackages__"]
        if len(rollup) <= len(narrow):
            return rollup
    return narrow

def get_orig_gn_visibility_scopes(pkg_path):
    """Returns {target_name: [(scope_pkg, 'pkg'|'subpackages'), ...]} for targets in the pre-migration
    BUILD.gn (above BAZEL2GN SENTINEL) that had an explicit non-wildcard visibility = [...] restriction."""
    gn_rel = f"{pkg_path}/BUILD.gn" if pkg_path else "BUILD.gn"

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
            if not pat.startswith("//"):
                rel_p = pat
                while rel_p.startswith("./"):
                    rel_p = rel_p[2:]
                pat = f"//{pkg_path}/{rel_p}" if pkg_path else f"//{rel_p}"
            m_gn = re.match(r"^//([^:*]*?)/?(\*|:.*)?$", pat)
            if not m_gn:
                return None
            sp = os.path.normpath(m_gn.group(1).strip("/"))
            if sp == ".":
                sp = ""
            if sp == ".." or sp.startswith("../"):
                return None
            suffix = m_gn.group(2) or ""
            scopes.append((sp, "subpackages" if suffix == "*" else "pkg"))
        return scopes

    out = {}
    target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    for ref in dict.fromkeys(["origin/main", "HEAD~1", change_base, "HEAD"]):
        if not ref:
            continue
        try:
            gn_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            continue
        # Split on BAZEL2GN SENTINEL before stripping '#' comments so targets
        # generated below the sentinel comment are never mistaken for hand-written
        # pre-migration GN targets.
        raw_above = (
            gn_src.split("BAZEL2GN SENTINEL")[0]
            if "BAZEL2GN SENTINEL" in gn_src
            else gn_src
        )
        above_sentinel = re.sub(r"#[^\n]*", "", raw_above)
        tmpl_vis_map = {}
        target_blocks = []
        for m in target_re.finditer(above_sentinel):
            tmpl_kind = m.group(1)
            tname = m.group(2)
            depth, in_str, i, n = 0, False, m.end() - 1, len(above_sentinel)
            while i < n:
                c = above_sentinel[i]
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
            tbody = above_sentinel[m.end() : i]
            if tmpl_kind == "template":
                if not re.search(r"\bvisibility\s*\+=", tbody):
                    vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
                    if vm:
                        scopes = parse_gn_vis_list(vm.group(1))
                        if scopes:
                            tmpl_vis_map[tname] = scopes
                continue
            target_blocks.append((tmpl_kind, tname, tbody))
        for tmpl_kind, tname, tbody in target_blocks:
            if tname in out:
                continue
            if re.search(r"\bvisibility\s*\+=", tbody):
                continue
            vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
            scopes = parse_gn_vis_list(vm.group(1)) if vm else tmpl_vis_map.get(tmpl_kind)
            if scopes:
                # If a non-testonly group() in the same BUILD.gn forwards :<tname> without a
                # visibility restriction, external callers reach :<tname> through that group in GN
                # and must depend on :<tname> directly in Bazel.
                unrestricted_fwd = False
                for gm in re.finditer(r'\bgroup\(\s*"([^"$]+)"\s*\)\s*\{([^{}]*)\}', above_sentinel):
                    gbody = gm.group(2)
                    if (
                        not re.search(r"\btestonly\s*=\s*true\b", gbody)
                        and not re.search(r"\bvisibility\s*=", gbody)
                        and re.search(r'":' + re.escape(tname) + r'(?:\([^)]*\))?"', gbody)
                    ):
                        unrestricted_fwd = True
                        break
                if not unrestricted_fwd:
                    out[tname] = list(scopes)
    return out

# Convert explicit narrow GN visibility from the pre-migration BUILD.gn into Bazel
# visibility labels so developer-chosen subsystem/package bounds are preserved.
def get_preserved_gn_visibility(
    tname, rdep_pkgs, pkg_path, orig_gn_vis_map, vis_only_pkgs=(), gn_only_pkgs=()
):
    gn_scopes = orig_gn_vis_map.get(tname)
    if not gn_scopes:
        return None
    pkgs = sorted(
        set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != pkg_path)
    )
    has_vg = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in pkgs)
    valid_scopes = []
    for sp, kind in gn_scopes:
        if sp == pkg_path and kind == "pkg":
            continue
        if sp == VG_ROOT or sp.startswith(VG_ROOT + "/"):
            if has_vg:
                valid_scopes.append((VG_ROOT, "subpackages"))
            continue
        if kind == "subpackages":
            if not sp or sp in BANNED_TOP_LEVEL_SUBPKGS or sp in BANNED_UMBRELLA_SUBPKGS:
                return None
            parts = sp.split("/")
            if len(parts) == 2 and parts[0] in ("src", "sdk"):
                if not (pkg_path == sp or pkg_path.startswith(sp + "/")):
                    return None
            if not os.path.isdir(os.path.join(workdir, sp)):
                continue
            valid_scopes.append((sp, "subpackages"))
        else:
            if (
                not sp
                or not os.path.isdir(os.path.join(workdir, sp))
                or not (
                    os.path.isfile(os.path.join(workdir, sp, "BUILD.bazel"))
                    or os.path.isfile(os.path.join(workdir, sp, "BUILD"))
                )
                or sp in vis_only_pkgs
                or sp in gn_only_pkgs
            ):
                continue
            valid_scopes.append((sp, "pkg"))
    if not valid_scopes:
        return None
    sub_roots = {sp for sp, k in valid_scopes if k == "subpackages"}
    preserved = set()
    for sp, kind in valid_scopes:
        if kind == "subpackages":
            if any(sp != o and sp.startswith(o + "/") for o in sub_roots):
                continue
            preserved.add(VG_SUBPKGS if sp == VG_ROOT else f"//{sp}:__subpackages__")
        else:
            if any(sp == o or sp.startswith(o + "/") for o in sub_roots):
                continue
            preserved.add(f"//{sp}:__pkg__")
    uncovered = [
        r
        for r in pkgs
        if not any(
            r == gsp or (gk == "subpackages" and (gsp == "" or r.startswith(gsp + "/")))
            for gsp, gk in valid_scopes
        )
    ]
    if uncovered:
        preserved.update(recommend_narrow_visibility(uncovered, pkg_path))
    if not preserved or len(preserved) > 15:
        return None
    return sorted(preserved)


# Flag targets that tighten visibility narrower than an explicit narrow GN visibility list.
def check_visibility_tightened_from_gn(
    target_name, rel_path, lineno, vis_vals, vis_scopes, preserved_gn_vis, pkg_path
):
    missing_preserved = []
    for pval in preserved_gn_vis:
        pm = re.match(r"^//([^:]+):__(pkg|subpackages)__$", pval)
        if not pm:
            continue
        psp, pkind = pm.group(1).strip("/"), pm.group(2)
        if pkind == "pkg":
            cov = any(
                psp == sp or (k == "subpackages" and (sp == "" or psp.startswith(sp + "/")))
                for sp, k in vis_scopes
            )
        else:
            cov = any(
                k == "subpackages" and (sp == "" or psp == sp or psp.startswith(sp + "/"))
                for sp, k in vis_scopes
            )
        if not cov:
            missing_preserved.append(pval)
    if missing_preserved:
        findings.append({
            "source": "visibility_audit",
            "category": "visibility_tightened_from_gn",
            "severity": "error",
            "file": rel_path,
            "line": lineno,
            "message": (
                f"Target '{target_name}' tightens visibility to {json.dumps(vis_vals)} "
                f"from the explicit narrow visibility in `{pkg_path}/BUILD.gn` "
                f"({json.dumps(preserved_gn_vis)}). Stick with the existing narrow "
                "visibility decision even if the target is not currently used outside "
                "a narrower scope."
            ),
            "remediation": (
                f"Set visibility = {json.dumps(preserved_gn_vis)} on '{target_name}' "
                "to preserve the original narrow GN visibility scope, and re-run "
                "fx bazel2gn."
            ),
        })


# Collect pre-existing visibility attributes, unscoped targets, package(default_visibility),
# and exports_files() calls without visibility from change_base/HEAD~1 so collateral edits to
# an existing BUILD.bazel (e.g. registering a test in test_suite) do not flag unrelated targets.
def get_preexisting_vis_info(rel_path):
    """Returns (vis_strings, no_vis_targets, has_pkg_default_vis, no_vis_exports) from change_base."""
    wt_diff = False
    try:
        wt_out = subprocess.check_output(
            ["git", "-C", workdir, "diff", "--name-only", "HEAD", "--", rel_path],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        wt_diff = bool(wt_out)
    except Exception:
        pass
    refs = ["HEAD~1"] if (change_base == "HEAD" and not wt_diff) else [change_base, "HEAD~1"]
    for ref in dict.fromkeys(refs):
        try:
            prev_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            prev_tree = ast.parse(prev_src, filename=rel_path)
            vis_vals = set()
            no_vis_targets = set()
            has_pkg_default_vis = False
            no_vis_exports = set()
            for node in ast.walk(prev_tree):
                if isinstance(node, ast.Call):
                    func_name = (
                        node.func.id
                        if isinstance(node.func, ast.Name)
                        else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
                    )
                    if func_name == "package":
                        if any(kw.arg == "default_visibility" for kw in node.keywords):
                            has_pkg_default_vis = True
                        continue
                    if func_name == "exports_files":
                        if (
                            not any(kw.arg == "visibility" for kw in node.keywords)
                            and len(node.args) < 2
                        ):
                            no_vis_exports.add(ast.dump(node))
                            if node.args:
                                no_vis_exports.add(ast.dump(node.args[0]))
                        continue
                    tname = None
                    has_vis = False
                    for kw in node.keywords:
                        if (
                            kw.arg == "name"
                            and isinstance(kw.value, ast.Constant)
                            and isinstance(kw.value.value, str)
                        ):
                            tname = kw.value.value
                        elif kw.arg == "visibility":
                            has_vis = True
                            if isinstance(kw.value, ast.List):
                                for elt in kw.value.elts:
                                    if (
                                        isinstance(elt, ast.Constant)
                                        and isinstance(elt.value, str)
                                    ):
                                        vis_vals.add(elt.value)
                    if tname and not has_vis:
                        no_vis_targets.add(tname)
            return vis_vals, no_vis_targets, has_pkg_default_vis, no_vis_exports
        except Exception:
            continue
    return set(), set(), False, set()


# Discover all git repositories in the checkout (root and sub-repositories)
def discover_git_repos():
    repos = [workdir]
    sub_root = os.path.join(workdir, VG_ROOT.split("/")[0])
    if os.path.isdir(sub_root):
        try:
            for entry in sorted(os.listdir(sub_root)):
                sub = os.path.join(sub_root, entry)
                if os.path.exists(os.path.join(sub, ".git")):
                    repos.append(sub)
        except OSError:
            pass
    return repos


# Refresh origin/main in each repository (root and sub-repositories) so callers added
# upstream after the local checkout or sub-repository snapshot was created are visible.
FETCH_TTL_SECS = 60


# Build git config arguments that rewrite sso:// remotes to https:// so git fetch works
# when /var/run/ccache is mounted read-only in the sandbox.
def sso_https_git_config_args(repo):
    try:
        url = subprocess.check_output(
            ["git", "-C", repo, "config", "--get", "remote.origin.url"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return []
    if not url.startswith("sso://"):
        return []
    host = url[len("sso://") :].split("/", 1)[0].strip()
    if not host:
        return []
    https_base = f"https://{host}.googlesource.com/"
    return ["-c", f"url.{https_base}.insteadOf=sso://{host}/"]


def maybe_refresh_repo_origin_main(repo):
    if os.environ.get("PLANTER_SKIP_FETCH", "").strip() not in ("", "0"):
        return
    if os.environ.get("PLANTER_AGENT_ROLE", "").strip() == "evolver":
        return
    stamp_key = hashlib.sha256(repo.encode("utf-8")).hexdigest()[:16]
    stamp_path = os.path.join(
        tempfile.gettempdir(), f"planter-origin-main-fetch-{stamp_key}.stamp"
    )
    now = time.time()
    try:
        if now - os.path.getmtime(stamp_path) < FETCH_TTL_SECS:
            return
    except OSError:
        pass
    fetch_ok = False
    try:
        cfg_args = sso_https_git_config_args(repo)
        proc = subprocess.run(
            ["git", "-C", repo]
            + cfg_args
            + [
                "fetch",
                "--quiet",
                "--no-tags",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
        fetch_ok = proc.returncode == 0
    except Exception:
        pass
    if fetch_ok:
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(str(int(now)))
        except OSError:
            pass


# Check whether a sub-repository's local origin/main ref is stale relative to the root
# repository (e.g. when git fetch was skipped or could not update a lagging snapshot).
def is_subrepo_origin_main_stale(sub_rel):
    sub_repo = os.path.join(workdir, sub_rel)
    if not os.path.exists(os.path.join(sub_repo, ".git")):
        return False
    try:
        root_ts = int(
            subprocess.check_output(
                ["git", "-C", workdir, "log", "-1", "--format=%ct", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
        )
        sub_ts = int(
            subprocess.check_output(
                ["git", "-C", sub_repo, "log", "-1", "--format=%ct", "origin/main"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
        )
        return (root_ts - sub_ts) > 86400
    except Exception:
        return False


origin_main_sources = {}

# Batch-discover all build files mentioning any candidate package across all repos.
# Using git grep per repository avoids false-empty results when workdir is a git worktree under
# .jiri_root/worktrees/<slot> (where the parent checkout's .gitignore ignores /.jiri_root/) or
# when sub-repositories are listed in the root .gitignore.
def discover_referencing_build_files(pkg_list):
    found = {
        os.path.join(workdir, rel)
        for rel in candidate_files
        if os.path.isfile(os.path.join(workdir, rel))
    }
    if not pkg_list:
        return found
    search_terms = set()
    for p in pkg_list:
        search_terms.add(f"//{p}")
        p_parts = [seg for seg in p.split("/") if seg]
        for i in range(1, len(p_parts)):
            search_terms.add("../" + "/".join(p_parts[i:]))
    pattern_args = [arg for term in sorted(search_terms) for arg in ("-e", term)]
    repos = discover_git_repos()
    used_git_grep = False
    for repo in repos:
        if not os.path.exists(os.path.join(repo, ".git")):
            continue
        maybe_refresh_repo_origin_main(repo)
        grep_cmd = (
            ["git", "-C", repo, "grep", "--untracked", "-l", "--fixed-strings"]
            + pattern_args
            + ["--", "*BUILD.gn", "*BUILD.bazel", "*.gni", "*.bzl"]
        )
        try:
            proc = subprocess.run(
                grep_cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if proc.returncode in (0, 1):
                used_git_grep = True
            for line in proc.stdout.splitlines():
                if line.strip():
                    found.add(os.path.join(repo, line.strip()))
        except OSError:
            pass
        # Also search origin/main in each repo so callers added upstream in a
        # sub-repository whose working tree is at an older Jiri snapshot are found.
        om_grep_cmd = (
            ["git", "-C", repo, "grep", "-l", "--fixed-strings"]
            + pattern_args
            + ["origin/main", "--", "*BUILD.gn", "*BUILD.bazel", "*.gni", "*.bzl"]
        )
        try:
            om_proc = subprocess.run(
                om_grep_cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if om_proc.returncode == 0:
                for line in om_proc.stdout.splitlines():
                    s = line.strip()
                    if s.startswith("origin/main:"):
                        rel_line = s[len("origin/main:") :]
                        abs_cand = os.path.join(repo, rel_line)
                        origin_main_sources[abs_cand] = (repo, rel_line)
                        found.add(abs_cand)
        except OSError:
            pass
        # Include untracked and modified build files not indexed by git grep
        for diff_cmd in (
            ["git", "-C", repo, "diff", "--name-only", change_base],
            ["git", "-C", repo, "diff", "--cached", "--name-only"],
            [
                "git",
                "-C",
                repo,
                "ls-files",
                "--others",
                "--exclude-standard",
                "--",
                "*BUILD.bazel",
                "*BUILD.gn",
                "*.gni",
                "*.bzl",
            ],
        ):
            try:
                out = subprocess.check_output(
                    diff_cmd, stderr=subprocess.DEVNULL, text=True
                )
                for line in out.splitlines():
                    rel_line = line.strip()
                    if not rel_line:
                        continue
                    direct_cand = os.path.join(repo, rel_line)
                    if rel_line.endswith(("BUILD.bazel", "BUILD.gn", ".gni", ".bzl")) and os.path.isfile(direct_cand):
                        found.add(direct_cand)
                    d = os.path.dirname(rel_line)
                    for bname in ("BUILD.bazel", "BUILD.gn"):
                        cand = os.path.join(repo, d, bname) if d else os.path.join(repo, bname)
                        if os.path.isfile(cand):
                            found.add(cand)
            except Exception:
                pass
    if not used_git_grep:
        rg_bin = shutil.which(
            "rg",
            path=os.environ.get("PATH", "") + os.pathsep + os.path.expanduser("~/.cargo/bin"),
        )
        if rg_bin:
            rg_globs = [
                "-g",
                "*BUILD.gn",
                "-g",
                "*BUILD.bazel",
                "-g",
                "*.gni",
                "-g",
                "*.bzl",
                "-g",
                "!out/**",
            ]
            if ".jiri_root" not in workdir:
                rg_globs.extend(["-g", "!.jiri_root/**"])
            rg_cmd = (
                [
                    rg_bin,
                    "-l",
                    "--no-ignore-parent",
                    "--no-require-git",
                    "--fixed-strings",
                ]
                + rg_globs
                + pattern_args
                + repos
            )
            try:
                proc = subprocess.run(
                    rg_cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
                )
                if proc.returncode in (0, 1):
                    for line in proc.stdout.splitlines():
                        if line.strip():
                            found.add(line.strip())
            except OSError:
                pass
    for p in pkg_list:
        parts = p.split("/")
        for i in range(1, len(parts)):
            anc_gn = os.path.join(workdir, "/".join(parts[:i]), "BUILD.gn")
            if os.path.isfile(anc_gn):
                found.add(anc_gn)
    return found


pkg_paths = sorted({os.path.dirname(p).strip("/") for p in candidate_files})
matched_files = discover_referencing_build_files(pkg_paths)

_NON_DEP_LIST_RE = re.compile(r"\b(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]", re.DOTALL)
_SUITE_LIST_RE = re.compile(r"\b(?:target_tests|host_tests)\s*\+?=\s*\[[^\]]*\]", re.DOTALL)
_BAZEL_TEST_SUITE_OPEN_RE = re.compile(r'\bbazel_test_suite\s*\(\s*"[^"]*"\s*\)\s*\{')


def strip_non_dep_lists(no_comments, is_gn):
    """Removes list attributes that reference targets without creating a dependency edge.

    `visibility = [...]` / `packages = [...]` entries are allowlists in both build systems. In GN,
    `target_tests` / `host_tests` are only label lists without a GN edge inside a
    `bazel_test_suite(...) { ... }` block (Bazel labels that the bridge builds as top-level
    targets, which Bazel never visibility-checks). Other GN templates use attributes of the same
    name as real dependency lists (a test-group template appends `target_tests` to `deps`), so
    those references must stay and count as reverse dependencies, consistent with find_rdeps.
    """
    stripped = _NON_DEP_LIST_RE.sub("", no_comments)
    if not is_gn:
        return stripped
    out = []
    pos = 0
    for m in _BAZEL_TEST_SUITE_OPEN_RE.finditer(stripped):
        start = m.end()
        if start < pos:
            continue
        depth = 1
        i = start
        while i < len(stripped) and depth > 0:
            c = stripped[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
            i += 1
        out.append(stripped[pos:start])
        out.append(_SUITE_LIST_RE.sub("", stripped[start:i]))
        pos = i
    out.append(stripped[pos:])
    return "".join(out)


file_cache = {}
for fpath in sorted(matched_files):
    try:
        parts = []
        if os.path.isfile(fpath):
            with open(fpath, "r", encoding="utf-8", errors="ignore") as f:
                parts.append(f.read())
        om_info = origin_main_sources.get(fpath)
        if om_info:
            repo, rel_line = om_info
            try:
                om_raw = subprocess.check_output(
                    ["git", "-C", repo, "show", f"origin/main:{rel_line}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                if om_raw and (not parts or om_raw != parts[0]):
                    parts.append(om_raw)
            except Exception:
                pass
        raw = "\n".join(parts)
        if not raw:
            continue
        no_comments = re.sub(r"#[^\n]*", "", raw)
        stripped = strip_non_dep_lists(no_comments, fpath.endswith((".gn", ".gni")))
        file_cache[fpath] = (no_comments, stripped)
    except Exception:
        pass


GN_IMPORT_RE = re.compile(r'\bimport\(\s*"([^"]+\.gni)"\s*\)')
_gni_importer_cache = {}


def gni_importer_pkgs(gni_rel, depth=0):
    """Returns the packages whose BUILD.gn imports `gni_rel` (directly or via other .gni files).

    GN resolves labels inside a template, and checks their visibility, in the BUILD.gn that
    invokes it, not in the directory of the .gni that defines it. So a dependency written in
    `//a/b/foo.gni` and used from `//a/BUILD.gn` needs visibility for `//a`, not `//a/b`.
    """
    if gni_rel in _gni_importer_cache:
        return _gni_importer_cache[gni_rel]
    _gni_importer_cache[gni_rel] = set()
    if depth > 3:
        return set()
    importers = set()
    basename = os.path.basename(gni_rel)
    for repo in discover_git_repos():
        if not os.path.exists(os.path.join(repo, ".git")):
            continue
        try:
            proc = subprocess.run(
                ["git", "-C", repo, "grep", "--untracked", "-l", "--fixed-strings", "-e", basename,
                 "--", "*BUILD.gn", "*.gni"],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except OSError:
            continue
        for line in proc.stdout.splitlines():
            imp_abs = os.path.join(repo, line.strip())
            imp_rel = os.path.relpath(imp_abs, workdir)
            try:
                with open(imp_abs, "r", encoding="utf-8", errors="ignore") as f:
                    imp_text = re.sub(r"#[^\n]*", "", f.read())
            except OSError:
                continue
            for path in GN_IMPORT_RE.findall(imp_text):
                if path.startswith("//"):
                    resolved = path[2:]
                else:
                    resolved = os.path.normpath(os.path.join(os.path.dirname(imp_rel), path))
                if resolved != gni_rel:
                    continue
                if imp_rel.endswith(".gni"):
                    importers |= gni_importer_pkgs(imp_rel, depth + 1)
                else:
                    importers.add(os.path.dirname(imp_rel).strip("/"))
    _gni_importer_cache[gni_rel] = importers
    return importers


def is_idk_generated_ref(sub, owner, idk_targets):
    """Whether `sub` names an IDK atom target generated for the idk_* Bazel target `owner`.

    `idk_cc_*` macros add `//sdk:__pkg__` to the visibility of the target and its `_idk` atom
    automatically, and GN `sdk_*` templates only forward `visibility` to the library target,
    not to the `<name>_sdk` atom. So references to `<name>_sdk` / `<name>_idk` (from
    `//sdk/BUILD.gn` or `//sdk/atom_lists.bzl`) need no visibility entry on the Bazel target.
    """
    return bool(owner) and owner in idk_targets and sub != owner and sub.startswith(owner + "_") and (
        sub.endswith(("_sdk", "_idk")) or "_sdk_" in sub or "_idk_" in sub
    )

# GN templates whose targets can be migrated or dual-built in BUILD.bazel and whose
# reverse dependencies in BUILD.gn/BUILD.bazel require Bazel target visibility coverage.
CONVERTIBLE_GN_TEMPLATES = {
    "static_library",
    "source_set",
    "shared_library",
    "executable",
    "library_headers",
    "rustc_library",
    "rustc_library_with_features",
    "rustc_binary",
    "rustc_macro",
    "rustc_dylib",
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
    "fuchsia_structured_config_rust_lib",
    "fuchsia_structured_config_cpp_elf_lib",
    "fuchsia_structured_config_values",
    "resource",
    "resource_tree",
    "resource_group",
    "config",
    "bind_library",
    "cml_shard",
}

GN_PACKAGE_TEMPLATES = {
    "fuchsia_package",
    "fuchsia_package_with_single_component",
}

# Test rules that bazel2gn translates into BUILD.gn below BAZEL2GN SENTINEL (including visibility).
BAZEL_GN_SYNCED_TEST_RULES = {
    "rustc_test",
    "host_rustc_test",
    "go_test",
    "host_go_test",
}

BAZEL_NON_GN_SYNCED_RULES = {
    "cc_test",
    "cml_shard",
    "expect_includes",
    "fx_cc_test",
    "fx_packaged_binary",
    "fx_test_component",
    "fx_unittest_component",
    "fx_test",
    "fx_unittest_package",
    "test_suite",
    "rustc_test",
    "host_cc_test",
    "host_rustc_test",
    "host_go_test",
    "host_py_test",
    "host_shac_test",
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
}


# Map referenced GN/Bazel target labels to their owning BUILD.bazel target
def owning_bazel_target(
    sub, bazel_targets, default_target="", non_gn_synced_targets=(), test_rule_targets=()
):
    """Maps a referenced label name to the Bazel target that defines or generates it.

    Handles exact matches, hyphen/underscore-normalized matches, generated
    '<name>_<suffix>' sub-targets, and shorthand package-default labels in
    packages that also define ':headers' or test targets.
    """
    if sub in bazel_targets:
        return sub
    norm_sub = sub.replace("-", "_")
    for t in bazel_targets:
        if t.replace("-", "_") == norm_sub:
            return t
    best = None
    for t in bazel_targets:
        norm_t = t.replace("-", "_")
        if (sub.startswith(t + "_") or norm_sub.startswith(norm_t + "_")) and (
            best is None or len(t) > len(best)
        ):
            best = t
    if best is not None:
        return best
    if default_target and sub == default_target:
        primary = [
            t
            for t in bazel_targets
            if t not in non_gn_synced_targets
            and t not in test_rule_targets
            and t != "headers"
        ]
        if len(primary) == 1:
            return primary[0]
    return None


# Check whether a .gn/.gni reference targets a GN-only, non-synced, or # @bazel2gn:skip target
def is_gn_only_target_ref(
    rel,
    caller_pkg,
    sub,
    owner,
    bazel_targets,
    is_pkg_target,
    non_gn_synced_targets,
    gn_nonconvertible_targets,
):
    if not rel.endswith((".gn", ".gni")):
        return False
    if sub in non_gn_synced_targets or sub in gn_nonconvertible_targets:
        return True
    if owner in non_gn_synced_targets or owner in gn_nonconvertible_targets:
        return True
    if is_pkg_target and not caller_pkg.startswith(("bundles/assembly", "build/images")):
        return True
    if owner is None and bazel_targets and not is_pkg_target:
        return True
    return False

# Map non-testonly GN forwarding group('<g>') targets above the sentinel to the Bazel
# targets ':bt' they forward in deps/public_deps.
def extract_gn_forwarding_groups(gn_text, bazel_targets, non_gn_synced_targets, package_targets):
    eligible = [
        t for t in bazel_targets if t not in non_gn_synced_targets and t not in package_targets
    ]
    if not eligible:
        return {}
    no_comments = re.sub(r"#[^\n]*", "", gn_text)
    forwarding = {}
    target_re = re.compile(r'\bgroup\(\s*"([^"$]+)"\s*\)\s*\{')
    for m in target_re.finditer(no_comments):
        gname = m.group(1)
        if gname in ("tests", "benchmarks") or gname in bazel_targets:
            continue
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
        gbody = no_comments[m.end() : i]
        if re.search(r"\btestonly\s*=\s*true\b", gbody):
            continue
        forwarded = [
            bt
            for bt in eligible
            if re.search(r'":' + re.escape(bt) + r'(?:\([^)]*\))?"', gbody)
        ]
        if forwarded:
            forwarding[gname] = forwarded
    return forwarding


def compute_package_rdeps(
    pkg_path,
    bazel_targets,
    package_targets,
    non_gn_synced_targets,
    alias_map,
    orig_gn_vis_map=None,
    test_rule_targets=(),
    idk_targets=(),
):
    default_target = os.path.basename(pkg_path)
    gn_file = os.path.join(workdir, pkg_path, "BUILD.gn")
    gn_only_targets = {"tests", "benchmarks"}
    gn_nonconvertible_targets = {"tests", "benchmarks"}
    gn_forwarding_groups = {}

    # Classify pre-sentinel GN-only targets and targets generated below BAZEL2GN SENTINEL.
    # If a pre-sentinel GN target (such as a `group("<name>")` forwarding `expect_includes`)
    # has a same-named target in `BUILD.bazel` (such as `alias(name = "<name>", actual = "...")`),
    # GN callers of `:<name>` are real callers of the Bazel target and must not be filtered out.
    def compute_gn_nonconvertible_targets(
        above_sentinel, below_sentinel, bazel_targets, has_sentinel, fwd_groups
    ):
        only_targets = {"tests", "benchmarks"}
        nonconv_targets = {"tests", "benchmarks"}
        above_clean = re.sub(r"#[^\n]*", "", above_sentinel)
        below_clean = re.sub(r"#[^\n]*", "", below_sentinel)
        below_synced = {
            m.group(2)
            for m in re.finditer(
                r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                below_clean,
                flags=re.MULTILINE,
            )
            if m.group(2) in bazel_targets
        }
        for m in re.finditer(
            r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
            above_clean,
            flags=re.MULTILINE,
        ):
            tmpl, t = m.group(1), m.group(2)
            if t in fwd_groups:
                continue
            if t not in bazel_targets:
                only_targets.add(t)
            is_unmigrated_dep_target = (
                not has_sentinel
                and target_dir
                and pkg_path != target_dir
                and t not in bazel_targets
            )
            if (
                tmpl not in CONVERTIBLE_GN_TEMPLATES
                and (
                    t not in bazel_targets
                    or t in ("tests", "benchmarks")
                    or tmpl == "bazel_test_suite"
                )
            ) or is_unmigrated_dep_target:
                nonconv_targets.add(t)
        return only_targets, nonconv_targets, below_synced

    if os.path.isfile(gn_file):
        try:
            with open(gn_file, "r", encoding="utf-8", errors="ignore") as f:
                gn_src = f.read()
            for m in re.finditer(
                r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE
            ):
                if m.group(1) in GN_PACKAGE_TEMPLATES:
                    package_targets.add(m.group(2))
            has_sentinel = "BAZEL2GN SENTINEL" in gn_src
            above_sentinel, below_sentinel = (
                gn_src.split("BAZEL2GN SENTINEL", 1) if has_sentinel else (gn_src, "")
            )
            gn_forwarding_groups = extract_gn_forwarding_groups(
                above_sentinel, bazel_targets, non_gn_synced_targets, package_targets
            )
            gn_only_targets, gn_nonconvertible_targets, below_synced_targets = (
                compute_gn_nonconvertible_targets(
                    above_sentinel,
                    below_sentinel,
                    bazel_targets,
                    has_sentinel,
                    gn_forwarding_groups,
                )
            )
            non_gn_synced_targets.difference_update(below_synced_targets)
            package_targets.difference_update(below_synced_targets)
        except Exception:
            pass

    abs_label_re = re.compile(r'"//' + re.escape(pkg_path) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')
    true_rdep_packages = set()
    visibility_only_packages = set()
    gn_only_ref_packages = set()
    per_target_rdeps = {t: set() for t in bazel_targets}
    per_target_direct_rdeps = {t: {} for t in bazel_targets}
    has_macro_injected_rdeps = False

    # Collect .gni templates and list variables that expand to dependency references on targets in
    # pkg_path so callers that import the .gni and invoke the template or variable are recognized as GN rdeps.
    def collect_gni_template_matches(p_path, label_re):
        out = {}
        for fp, (_, strp) in list(file_cache.items()):
            g_rel = os.path.relpath(fp, workdir)
            if not g_rel.endswith(".gni") or os.path.basename(g_rel) == "visibility.gni":
                continue
            t_names = set(re.findall(r'\btemplate\(\s*"([^"]+)"\s*\)', strp))
            v_names = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=\s*\[", strp)) - {
                "visibility",
                "packages",
                "target_tests",
                "host_tests",
            }
            m_list = list(label_re.finditer(strp))
            if (t_names or v_names) and m_list:
                out[f"//{g_rel}"] = (t_names, v_names, m_list)
        if out:
            gni_pat_args = [arg for lbl in sorted(out) for arg in ("-e", lbl)]
            for repo in discover_git_repos():
                if not os.path.exists(os.path.join(repo, ".git")):
                    continue
                for g_cmd, is_om in (
                    (
                        ["git", "-C", repo, "grep", "--untracked", "-l", "--fixed-strings"]
                        + gni_pat_args
                        + ["--", "*BUILD.gn", "*.gni"],
                        False,
                    ),
                    (
                        ["git", "-C", repo, "grep", "-l", "--fixed-strings"]
                        + gni_pat_args
                        + ["origin/main", "--", "*BUILD.gn", "*.gni"],
                        True,
                    ),
                ):
                    try:
                        proc = subprocess.run(
                            g_cmd,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL,
                            text=True,
                        )
                        for line in proc.stdout.splitlines():
                            s = line.strip()
                            if not s:
                                continue
                            if is_om and s.startswith("origin/main:"):
                                rel_l = s[len("origin/main:") :]
                                abs_c = os.path.join(repo, rel_l)
                                origin_main_sources[abs_c] = (repo, rel_l)
                            else:
                                abs_c = os.path.join(repo, s)
                            if abs_c not in file_cache:
                                parts_c = []
                                if os.path.isfile(abs_c):
                                    with open(abs_c, "r", encoding="utf-8", errors="ignore") as cf:
                                        parts_c.append(cf.read())
                                om_i = origin_main_sources.get(abs_c)
                                if om_i:
                                    try:
                                        om_r = subprocess.check_output(
                                            ["git", "-C", om_i[0], "show", f"origin/main:{om_i[1]}"],
                                            stderr=subprocess.DEVNULL,
                                            text=True,
                                        )
                                        if om_r and (not parts_c or om_r != parts_c[0]):
                                            parts_c.append(om_r)
                                    except Exception:
                                        pass
                                raw_c = "\n".join(parts_c)
                                if raw_c:
                                    nc = re.sub(r"#[^\n]*", "", raw_c)
                                    st = strip_non_dep_lists(nc, True)
                                    file_cache[abs_c] = (nc, st)
                    except Exception:
                        pass
        return out

    gni_template_map = collect_gni_template_matches(pkg_path, abs_label_re)

    for fpath, (no_comments, stripped) in file_cache.items():
        rel = os.path.relpath(fpath, workdir)
        caller_pkg = os.path.dirname(rel).strip("/")
        if caller_pkg == pkg_path:
            continue

        # Match both ancestor-to-descendant ("sub/pkg") and sibling/cousin ("../pkg") relative GN labels
        rel_sub = None
        if rel.endswith((".gn", ".gni")) and caller_pkg and pkg_path:
            try:
                if os.path.commonpath([pkg_path, caller_pkg]):
                    cand_rel = os.path.relpath(pkg_path, caller_pkg)
                    if not cand_rel.startswith("../../.."):
                        rel_sub = cand_rel
            except ValueError:
                pass
        elif pkg_path.startswith(caller_pkg + "/"):
            rel_sub = pkg_path[len(caller_pkg) + 1 :]
        elif rel.endswith((".gn", ".gni")) and caller_pkg:
            cand_rel = os.path.relpath(pkg_path, caller_pkg)
            if cand_rel and cand_rel != ".":
                rel_sub = cand_rel
        rel_label_re = (
            re.compile(
                r'"(?:\./)?' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
            )
            if rel_sub
            else None
        )

        if (
            f"//{pkg_path}" not in no_comments
            and (not rel_sub or rel_sub not in no_comments)
            and not any(g_lbl in no_comments for g_lbl in gni_template_map)
        ):
            continue

        if os.path.basename(rel) == "visibility.gni":
            if abs_label_re.search(no_comments):
                visibility_only_packages.add(caller_pkg)
            continue

        gni_call_matches = []
        if rel.endswith((".gn", ".gni")) and gni_template_map:
            for gni_lbl, (t_names, v_names, m_list) in gni_template_map.items():
                gni_base = os.path.basename(gni_lbl)
                if (f'"{gni_lbl}"' in stripped or f'"{gni_base}"' in stripped) and (
                    any(re.search(r"\b" + re.escape(tn) + r"\s*\(", stripped) for tn in t_names)
                    or any(re.search(r"\b" + re.escape(vn) + r"\b", stripped) for vn in v_names)
                ):
                    gni_call_matches.extend(m_list)

        all_matches = (
            list(abs_label_re.finditer(no_comments))
            + (list(rel_label_re.finditer(no_comments)) if rel_label_re else [])
            + gni_call_matches
        )
        if not all_matches:
            continue

        dep_matches = (
            list(abs_label_re.finditer(stripped))
            + (list(rel_label_re.finditer(stripped)) if rel_label_re else [])
            + gni_call_matches
        )
        if not dep_matches:
            visibility_only_packages.add(caller_pkg)
            continue

        # Expand GN foreach interpolated labels ("$var:<target>") when a bare path item is in a list.
        interp_gn_subs = (
            re.findall(r'"\$\{?[A-Za-z_]\w*\}?:([A-Za-z0-9_.-]+)(?:\([^)]*\))?"', stripped)
            if rel.endswith((".gn", ".gni"))
            else []
        )
        expanded_subs = []
        for m in dep_matches:
            if m.group(1):
                expanded_subs.append(m.group(1))
            elif interp_gn_subs:
                expanded_subs.extend(interp_gn_subs)
            else:
                expanded_subs.append(default_target)

        for sub in expanded_subs:
            if sub in gn_forwarding_groups:
                norm_caller = (
                    "bundles/assembly"
                    if (caller_pkg == "bundles/assembly" or caller_pkg.startswith("bundles/assembly/"))
                    else caller_pkg
                )
                true_rdep_packages.add(norm_caller)
                for bt in gn_forwarding_groups[sub]:
                    per_target_rdeps[bt].add(norm_caller)
                continue
            if sub in gn_only_targets and sub not in bazel_targets:
                gn_only_ref_packages.add(caller_pkg)
                continue
            is_pkg_target = sub in package_targets or (
                sub == default_target and len(bazel_targets) == 1 and bazel_targets[0] in package_targets
            )
            owner = owning_bazel_target(
                sub,
                bazel_targets,
                default_target,
                non_gn_synced_targets,
                test_rule_targets,
            )
            if owner is None and sub == default_target and len(bazel_targets) == 1:
                owner = bazel_targets[0]
            if is_idk_generated_ref(sub, owner, idk_targets):
                continue
            if is_gn_only_target_ref(
                rel,
                caller_pkg,
                sub,
                owner,
                bazel_targets,
                is_pkg_target,
                non_gn_synced_targets,
                gn_nonconvertible_targets,
            ):
                gn_only_ref_packages.add(caller_pkg)
                continue
            norm_caller = (
                "bundles/assembly"
                if (caller_pkg == "bundles/assembly" or caller_pkg.startswith("bundles/assembly/"))
                else caller_pkg
            )
            if caller_pkg.startswith("build/") and (rel.endswith(".bzl") or rel.endswith(".gni")) and "verification" not in os.path.basename(rel):
                has_macro_injected_rdeps = True
            # GN checks a label written in a .gni in the BUILD.gn that imports it, so attribute
            # references in non-build/ .gni files to the importing packages (while also recording
            # the .gni's own package in true_rdep_packages/per_target_rdeps so granting visibility
            # to the .gni package matching find_rdeps is not flagged as a false-positive rdep).
            gni_importers = (
                gni_importer_pkgs(rel)
                if rel.endswith(".gni") and not caller_pkg.startswith("build/")
                else set()
            )
            if gni_importers and norm_caller and norm_caller != pkg_path:
                true_rdep_packages.add(norm_caller)
                if owner is not None:
                    per_target_rdeps[owner].add(norm_caller)
            callers = sorted(gni_importers - {pkg_path}) if gni_importers else [norm_caller]
            for caller in callers:
                true_rdep_packages.add(caller)
                if owner is None:
                    continue
                per_target_rdeps[owner].add(caller)
                # GN and Bazel enforce visibility in the package whose BUILD file names the
                # label (or imports the .gni whose template names it).
                if gni_importers:
                    per_target_direct_rdeps[owner].setdefault(caller, set()).add("gn")
                elif os.path.basename(rel) in ("BUILD.gn", "BUILD.bazel"):
                    kind_bs = "gn" if rel.endswith(".gn") else "bazel"
                    per_target_direct_rdeps[owner].setdefault(caller_pkg, set()).add(kind_bs)

    for alias_name, actual_name in alias_map.items():
        if alias_name in per_target_rdeps and actual_name in per_target_rdeps:
            combined = per_target_rdeps[alias_name] | per_target_rdeps[actual_name]
            per_target_rdeps[alias_name] = set(combined)
            if not orig_gn_vis_map or actual_name not in orig_gn_vis_map:
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

# Record non-blocking INFO visibility_narrowed coverage when visibility_audit confirms
# that narrowing from the pre-migration BUILD.gn visibility is intentional and valid
# (e.g. dropping packages that only reference :tests/:benchmarks or a split-out unit test).
def record_intentional_visibility_narrowing(
    target_name, rel_path, target_lineno, vis_scopes, orig_gn_vis_map, pkg_path
):
    gn_scopes = orig_gn_vis_map.get(target_name)
    if not gn_scopes:
        return
    uncovered_gn = []
    suggested_bz = []
    for gsp, gk in gn_scopes:
        if gsp == pkg_path and gk == "pkg":
            continue
        if gk == "pkg":
            cov = any(
                gsp == sp or (k == "subpackages" and (sp == "" or gsp.startswith(sp + "/")))
                for sp, k in vis_scopes
            )
            gn_pat = f"//{gsp}:*"
            bz_pat = f"//{gsp}:__pkg__"
        else:
            cov = any(
                k == "subpackages" and (sp == "" or gsp == sp or gsp.startswith(sp + "/"))
                for sp, k in vis_scopes
            )
            gn_pat = "//*" if not gsp else f"//{gsp}/*"
            bz_pat = "//:__subpackages__" if not gsp else f"//{gsp}:__subpackages__"
        if not cov and gn_pat not in uncovered_gn:
            uncovered_gn.append(gn_pat)
            suggested_bz.append(bz_pat)
    if not uncovered_gn:
        return
    msg = (
        f'Target "{target_name}": GN visibility [{", ".join(uncovered_gn)}] is not '
        "covered by Bazel visibility (GN `/*` also grants subpackages, unlike `:*`; "
        "only the target's own package is implicit in Bazel); add "
        f'[{", ".join(suggested_bz)}] or confirm the narrowing is intended'
    )
    findings.append({
        "source": "visibility_audit",
        "category": "visibility_narrowed",
        "severity": "info",
        "file": rel_path,
        "line": target_lineno,
        "message": msg,
        "remediation": (
            "Intentional visibility narrowing verified by visibility_audit "
            "(all actual reverse dependencies and preserved narrow GN scopes are covered)."
        ),
    })


# Query planter diff for any additional visibility_narrowed findings on targets that
# passed visibility_audit without error so review_diff deduplicates them as confirmed INFO.
def record_diff_visibility_narrowed_info(clean_targets):
    if not clean_targets or os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return
    planter_bin = shutil.which("planter")
    if not planter_bin:
        cand = os.path.expanduser("~/.local/bin/planter")
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            planter_bin = cand
    if not planter_bin:
        return
    env = dict(os.environ)
    env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
    try:
        out = subprocess.run(
            [planter_bin, "diff", "--local", "--check-findings", f"--workdir={workdir}"],
            cwd=workdir,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=15,
        ).stdout.strip()
        diff_list = json.loads(out) if out.startswith("[") else []
    except Exception:
        diff_list = []

    seen_keys = {
        (f.get("category"), f.get("file"), f.get("line"), f.get("message"))
        for f in findings
    }
    for df in diff_list:
        if not isinstance(df, dict) or df.get("category") != "visibility_narrowed":
            continue
        f_rel = df.get("file", "")
        msg = df.get("message", "")
        ln = df.get("line", 0)
        tm = re.match(r'^Target "([^"]+)"', msg)
        tname = tm.group(1) if tm else None
        if not tname or (f_rel, tname) not in clean_targets:
            continue
        key = ("visibility_narrowed", f_rel, ln, msg)
        if key in seen_keys:
            continue
        seen_keys.add(key)
        findings.append({
            "source": "visibility_audit",
            "category": "visibility_narrowed",
            "severity": "info",
            "file": f_rel,
            "line": ln,
            "message": msg,
            "remediation": (
                "Intentional visibility narrowing verified by visibility_audit "
                "(all actual reverse dependencies and preserved narrow GN scopes are covered)."
            ),
        })


# Cache ancestor packages that host (or are the canonical area root for) a Bazel test_suite
# so test targets prepared for ancestor suite aggregation (e.g. pending outside_changes in a
# parallel multi-package migration) are not rejected before the ancestor BUILD.bazel is edited.
_ancestor_test_suite_pkg_cache = {}


def is_ancestor_test_suite_package(anc_pkg, child_pkg):
    if not anc_pkg or anc_pkg == child_pkg or not child_pkg.startswith(anc_pkg + "/"):
        return False
    cached = _ancestor_test_suite_pkg_cache.get(anc_pkg)
    if cached is None:
        has_suite = False
        anc_bazel = os.path.join(workdir, anc_pkg, "BUILD.bazel")
        if os.path.isfile(anc_bazel):
            try:
                with open(anc_bazel, "r", encoding="utf-8") as bf:
                    anc_tree = ast.parse(bf.read(), filename=anc_bazel)
                has_suite = any(
                    isinstance(n, ast.Call)
                    and (
                        (isinstance(n.func, ast.Name) and n.func.id == "test_suite")
                        or (isinstance(n.func, ast.Attribute) and n.func.attr == "test_suite")
                    )
                    for n in ast.walk(anc_tree)
                )
            except Exception:
                has_suite = False
        if not has_suite:
            anc_gn = os.path.join(workdir, anc_pkg, "BUILD.gn")
            if os.path.isfile(anc_gn):
                try:
                    with open(anc_gn, "r", encoding="utf-8") as gf:
                        gn_clean = re.sub(r"#[^\n]*", "", gf.read())
                    has_suite = bool(re.search(r"\bbazel_test_suite\s*\(", gn_clean))
                except Exception:
                    has_suite = False
        _ancestor_test_suite_pkg_cache[anc_pkg] = has_suite
        cached = has_suite
    if cached:
        return True
    return anc_pkg == get_pkg_area(child_pkg) and os.path.isfile(
        os.path.join(workdir, anc_pkg, "BUILD.gn")
    )


def is_pending_ancestor_test_suite_vis(
    caller_pkg, child_pkg, target_is_test_rule, direct_rdeps_for_target
):
    if not target_is_test_rule:
        return False
    if any(
        "bazel" in kinds and child_pkg != r_pkg and child_pkg.startswith(r_pkg + "/")
        for r_pkg, kinds in direct_rdeps_for_target.items()
    ):
        return False
    return is_ancestor_test_suite_package(caller_pkg, child_pkg)


findings = []
clean_audited_targets = set()

for rel_path in sorted(candidate_files):
    full_path = os.path.join(workdir, rel_path)
    pkg_path = os.path.dirname(rel_path).strip("/")
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            src = f.read()
        tree = ast.parse(src, filename=rel_path)
    except Exception:
        continue
    src_lines = src.splitlines()
    # Generated files (e.g. crate_universe's third_party/rust_crates/vendor/*/BUILD.bazel)
    # get their visibility from the generator; fix the generator input instead.
    if re.search(r"^#\s*[@]generated\b", "\n".join(src_lines[:10]), flags=re.MULTILINE):
        continue

    (
        preexisting_vis,
        preexisting_no_vis_targets,
        preexisting_pkg_default_vis,
        preexisting_no_vis_exports,
    ) = get_preexisting_vis_info(rel_path)
    orig_gn_vis_map = get_orig_gn_visibility_scopes(pkg_path)
    var_table = {}
    bazel_targets = []
    package_targets = set()
    non_gn_synced_targets = set()
    test_rule_targets = set()
    idk_targets = set()
    bazel2gn_skipped_targets = set()
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
            is_test_package = False
            is_testonly = False
            for kw in node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    tname = kw.value.value
                elif kw.arg == "actual" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    actual = kw.value.value
                elif kw.arg == "test_components":
                    is_test_package = True
                elif kw.arg == "testonly":
                    if isinstance(kw.value, ast.Constant) and kw.value.value is True:
                        is_testonly = True
                    elif isinstance(kw.value, ast.Name) and kw.value.id in ("True", "true"):
                        is_testonly = True
            if tname:
                bazel_targets.append(tname)
                if func in ("fx_package", "fuchsia_package", "fx_unittest_package"):
                    package_targets.add(tname)
                if func in BAZEL_GN_SYNCED_TEST_RULES:
                    test_rule_targets.add(tname)
                if func in BAZEL_NON_GN_SYNCED_RULES:
                    non_gn_synced_targets.add(tname)
                    test_rule_targets.add(tname)
                if func == "fx_package" and is_test_package:
                    test_rule_targets.add(tname)
                if func == "fx_component_manifest" and is_testonly:
                    test_rule_targets.add(tname)
                if func.startswith("idk_"):
                    idk_targets.add(tname)
                if has_bazel2gn_skip_comment(src_lines, node.lineno):
                    bazel2gn_skipped_targets.add(tname)
                    if func not in ("fx_package", "fuchsia_package"):
                        non_gn_synced_targets.add(tname)
                if func == "alias" and actual and actual.startswith(":"):
                    alias_map[tname] = actual[1:]

    true_rdeps, per_target_rdeps, per_target_direct_rdeps, vis_only_pkgs, gn_only_pkgs, has_macro_injected_rdeps = compute_package_rdeps(
        pkg_path,
        bazel_targets,
        package_targets,
        non_gn_synced_targets,
        alias_map,
        orig_gn_vis_map,
        test_rule_targets,
        idk_targets,
    )

    # Check package-level default_visibility and report per-target visibility guidance
    def check_pkg_default_visibility(pkg_node):
        found_default_vis = False
        for kw in pkg_node.keywords:
            if kw.arg != "default_visibility":
                continue
            found_default_vis = True
            if preexisting_pkg_default_vis:
                continue
            ext_targets = [
                t for t in bazel_targets if len(per_target_rdeps.get(t, set())) > 0
            ]
            if ext_targets:
                per_t_desc = ", ".join(
                    f"'{t}': {json.dumps(recommend_visibility(per_target_rdeps[t], pkg_path))}"
                    for t in ext_targets
                )
                target_guidance = (
                    f"Set target-level visibility only on targets used outside this package "
                    f"({per_t_desc}), and omit visibility on package-private targets."
                )
            else:
                rec_pkg = recommend_visibility(true_rdeps, pkg_path)
                target_guidance = (
                    f"Set target-level visibility = {json.dumps(rec_pkg)} only on targets "
                    "used outside this package, and omit visibility on package-private targets."
                )
            msg = (
                "Do not set package-level 'default_visibility' in package(...) "
                "(see //docs/development/languages/bazel/style/build_files.md"
                "#avoid-package-default-visibility). Declare 'visibility' only on individual "
                "targets used outside this package, scoped to the packages and subpackages "
                "that depend on them."
            )
            remediation = f"Remove default_visibility from package(). {target_guidance}"
            findings.append({
                "source": "visibility_audit",
                "category": "visibility_scoping",
                "severity": "error",
                "file": rel_path,
                "line": kw.lineno,
                "message": msg,
                "remediation": remediation,
            })
        return found_default_vis

    has_pkg_default_vis = False
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func_name = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )

        # Check package-level default_visibility (skip when already present at change_base)
        if func_name == "package":
            if check_pkg_default_visibility(node):
                has_pkg_default_vis = True
            continue

        # Check exports_files() missing visibility (skip when unscoped exports_files existed at change_base)
        if func_name == "exports_files":
            exp_key = ast.dump(node.args[0]) if node.args else ""
            if (
                not any(kw.arg == "visibility" for kw in node.keywords)
                and len(node.args) < 2
                and ast.dump(node) not in preexisting_no_vis_exports
                and exp_key not in preexisting_no_vis_exports
            ):
                findings.append({
                    "source": "visibility_audit",
                    "category": "exports_files_missing_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": node.lineno,
                    "message": (
                        "exports_files() omits 'visibility' (Bazel then exports the files "
                        "publicly)."
                    ),
                    "remediation": (
                        "Add visibility = [...] to exports_files(), scoped as narrowly as "
                        "practical to the packages that consume the files (e.g. "
                        "[\"//<consumer_pkg>:__pkg__\"], or an area rollup like "
                        "\"//<area>:__subpackages__\" for many consumers), per the "
                        "determining_bazel_visibility skill."
                    ),
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
            or (func_name == "fx_package" and any(k.arg == "test_components" for k in (node.keywords if hasattr(node, "keywords") else [])))
            or (func_name == "fx_component_manifest" and is_testonly)
        )
        is_test_support_pkg = (
            rel_path.startswith("src/testing/")
            or rel_path.startswith("src/lib/testing/")
            or rel_path.startswith("testing/")
            or "/testing/" in rel_path
        )
        is_shard_rule = func_name in ("cml_shard", "expect_includes")
        target_rdeps = per_target_rdeps.get(target_name, true_rdeps)
        distinct_areas = get_distinct_areas(target_rdeps, pkg_path)
        is_multi_area_test_bench_lib = (
            not is_test_rule
            and len(distinct_areas) >= 2
            and has_only_test_or_bench_rdeps(target_rdeps, pkg_path)
        )
        # Shared test-support targets may use broad visibility only when they are reused across
        # >= 2 areas, the same rule find_rdeps.sh uses to recommend ["//:__subpackages__"].
        # cml_shard/expect_includes targets are consumed through `.cml` includes rather than
        # BUILD deps, so their callers do not show up as rdeps; they keep broad visibility.
        allow_broad_test_vis = (
            (
                is_testonly
                or is_test_support_pkg
                or is_test_rule
                or is_multi_area_test_bench_lib
            )
            and len(distinct_areas) >= 2
        ) or is_shard_rule

        narrow_vis = recommend_narrow_visibility(target_rdeps, pkg_path)
        area_rollup_vis = recommend_area_rollup_visibility(target_rdeps, pkg_path)
        rec_target_vis = recommend_visibility(target_rdeps, pkg_path, is_test_support=allow_broad_test_vis)
        is_globally_used = is_globally_used_target(target_rdeps, pkg_path)
        preserved_gn_vis = (
            None
            if is_test_rule or is_globally_used
            else get_preserved_gn_visibility(
                target_name, target_rdeps, pkg_path, orig_gn_vis_map, vis_only_pkgs, gn_only_pkgs
            )
        )
        if preserved_gn_vis:
            rec_target_vis = preserved_gn_vis

        # Check missing target-level visibility on targets used outside this package.
        # Synced test targets without visibility stay unrestricted in bazel2gn, so GN-only
        # test callers do not require a Bazel visibility attribute unless a Bazel caller
        # (such as an ancestor BUILD.bazel test_suite) also depends on the test target.
        requires_vis_for_callers = (
            not (is_test_rule or target_name in test_rule_targets)
            or has_macro_injected_rdeps
            or any(
                "bazel" in kinds
                for kinds in per_target_direct_rdeps.get(target_name, {}).values()
            )
        )
        target_start_findings = len(findings)
        if vis_kw is None:
            if (
                not preexisting_pkg_default_vis
                and len(target_rdeps) > 0
                and requires_vis_for_callers
                and target_name not in preexisting_no_vis_targets
            ):
                msg = (
                    f"Target '{target_name}' has {len(target_rdeps)} external "
                    "reverse-dependency package(s) but omits 'visibility' "
                    "(defaulting to private in Bazel and unrestricted in bazel2gn)."
                )
                findings.append({
                    "source": "visibility_audit",
                    "category": "missing_target_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": node.lineno,
                    "message": msg,
                    "remediation": (
                        f"Add visibility = {json.dumps(rec_target_vis)} to '{target_name}'."
                    ),
                })
            elif (
                not preexisting_pkg_default_vis
                and preserved_gn_vis
                and target_name not in preexisting_no_vis_targets
            ):
                check_visibility_tightened_from_gn(
                    target_name, rel_path, node.lineno, [], [], preserved_gn_vis, pkg_path
                )
            elif not has_pkg_default_vis and not preserved_gn_vis:
                clean_audited_targets.add((rel_path, target_name))
                record_intentional_visibility_narrowing(
                    target_name, rel_path, node.lineno, [], orig_gn_vis_map, pkg_path
                )
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
        # Determine GN visibility scopes only when bazel2gn syncs the target to BUILD.gn.
        # Targets marked '# @bazel2gn:skip' or using Bazel-only rules are not emitted into
        # BUILD.gn by bazel2gn, so GN callers are governed by the pre-sentinel BUILD.gn target.
        is_gn_synced_target = (
            target_name not in bazel2gn_skipped_targets
            and target_name not in non_gn_synced_targets
        )
        gn_scopes = vis_scopes if is_gn_synced_target else None
        end_line = getattr(vis_kw.value, "end_lineno", None)
        if is_gn_synced_target and end_line and end_line <= len(src_lines):
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

        # Flag shared multi-area test/benchmark support libraries that had unrestricted GN
        # visibility at change_base but were narrowed to leaf caller packages, which breaks
        # cross-tree and out-of-graph benchmark/test callers in CQ.
        def check_narrow_shared_test_support_visibility():
            if (
                is_test_rule
                or is_shard_rule
                or not allow_broad_test_vis
                or preserved_gn_vis
                or target_name in orig_gn_vis_map
                or len(distinct_areas) < 3
                or not new_entries
                or "//:__subpackages__" in vis_vals
                or "//visibility:public" in vis_vals
            ):
                return False
            msg = (
                f"Target '{target_name}' is a shared test/benchmark support library used "
                f"across {len(distinct_areas)} distinct areas ({len(target_rdeps)} caller "
                f"packages) whose pre-migration `{pkg_path}/BUILD.gn` had unrestricted "
                f"visibility, but narrows visibility to {len(entries)} package/subtree "
                "entries, breaking cross-tree and out-of-graph benchmark/test callers in "
                "CQ with 'Dependency not allowed'."
            )
            rem = (
                f"Replace visibility on '{target_name}' with {json.dumps(rec_target_vis)} "
                "(shared test/benchmark support libraries used across multiple areas use "
                "repository-wide subpackage visibility) and re-run fx bazel2gn."
            )
            findings.append({
                "source": "visibility_audit",
                "category": "overly_narrow_global_visibility",
                "severity": "error",
                "file": rel_path,
                "line": vis_kw.lineno,
                "message": msg,
                "remediation": rem,
            })
            return True

        # Allow 2-3 level area rollups whose entry count exceeds 15 due to expanded umbrella directories
        max_rollup_entries = max(15, len(area_rollup_vis))
        new_entries = [(val, lineno) for (val, lineno) in entries if val not in preexisting_vis]
        if check_narrow_shared_test_support_visibility():
            continue
        # Audit sprawling visibility lists whenever a target's visibility is authored or
        # extended by this change (new_entries is non-empty), checking the total entry count
        # so lists that grow one caller at a time across migrations are rolled up instead of
        # accumulating dozens of leaf :__pkg__ entries.
        sprawling_vis = bool(new_entries) and (
            (len(entries) > max_rollup_entries and (is_globally_used or len(area_rollup_vis) < len(entries)))
            or (
                len(entries) > 10
                and len(distinct_areas) >= 3
                and len(area_rollup_vis) < len(entries)
            )
        )
        if sprawling_vis:
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
                top_roots = {r.split("/")[0] for r in target_rdeps if r.strip("/")}
                top_rollup_hint = (
                    ' (or ["//src:__subpackages__"] when widely used across //src)'
                    if top_roots == {"src"} and len(distinct_areas) >= 3
                    else ""
                )
                findings.append({
                    "source": "visibility_audit",
                    "category": "overly_narrow_global_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": vis_kw.lineno,
                    "message": (
                        f"Target '{target_name}' enumerates {len(entries)} narrow visibility entries across "
                        f"{len(distinct_areas)} areas. Roll up visibility to subsystem or top-level subtrees "
                        f"instead of appending individual leaf packages."
                    ),
                    "remediation": (
                        f"Replace visibility on '{target_name}' with rollup entries "
                        f"{json.dumps(area_rollup_vis)}{top_rollup_hint} and re-run fx bazel2gn."
                    ),
                })
            continue

        findings_before_target = len(findings)
        allow_broad_rollup = (is_globally_used or len(narrow_vis) > 5 or len(distinct_areas) >= 3 or allow_broad_test_vis) and len(entries) <= max_rollup_entries
        top_level_subpkgs = []
        for val, lineno in entries:
            if val in preexisting_vis:
                continue
            if val == "//visibility:private":
                # Check redundant //visibility:private when package has no default_visibility.
                # In a dual-build package with bazel2gn, a sole "//visibility:private" entry on a
                # synced target emits `visibility = [ ":*" ]` in BUILD.gn (whereas omitting
                # visibility leaves GN visibility unrestricted), so allow it when sole on a
                # bazel2gn-synced target.
                if not has_pkg_default_vis and not (is_gn_synced_target and len(entries) == 1):
                    findings.append({
                        "source": "visibility_audit",
                        "category": "redundant_private_visibility",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Target '{target_name}' specifies redundant "
                            "`\"//visibility:private\"` in `visibility` (which is already the "
                            "Bazel default when `default_visibility` is not set, and `bazel2gn` "
                            "automatically adds `\":*\"` to non-empty visibility lists)."
                        ),
                        "remediation": (
                            f"Remove `\"//visibility:private\"` from `visibility` on '{target_name}' "
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
            elif val == "//sdk:__pkg__" and func_name.startswith("idk_") and "sdk" not in target_rdeps:
                findings.append({
                    "source": "visibility_audit",
                    "category": "redundant_idk_sdk_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"`{func_name}(name = \"{target_name}\")` lists '//sdk:__pkg__' in visibility, but "
                        f"`{func_name}` already appends it to the target and its `_idk` atom, and nothing in "
                        "//sdk depends on the library target itself in GN (GN `sdk_*` templates only forward "
                        "`visibility` to the library, not to the `<name>_sdk` atom)."
                    ),
                    "remediation": f"Remove '//sdk:__pkg__' from the visibility of '{target_name}' and re-run fx bazel2gn.",
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

                if preserved_gn_vis and val in preserved_gn_vis:
                    continue

                m_sub = re.match(r"^//([^:]+):__subpackages__$", val)
                m_pkg = re.match(r"^//([^:]+):__pkg__$", val)

                if m_sub:
                    prefix = m_sub.group(1).strip("/")
                    if prefix == VG_ROOT:
                        has_vg_rdep = any(
                            r == VG_ROOT or r.startswith(VG_ROOT + "/") for r in target_rdeps
                        )
                        if (
                            not has_vg_rdep
                            and not has_macro_injected_rdeps
                            and not is_subrepo_origin_main_stale(VG_ROOT)
                        ):
                            findings.append({
                                "source": "visibility_audit",
                                "category": "false_positive_rdep_visibility",
                                "severity": "error",
                                "file": rel_path,
                                "line": lineno,
                                "message": (
                                    f"Target '{target_name}' grants visibility to '{val}', "
                                    "but no package in that vendor tree actually depends on "
                                    f"'{target_name}'."
                                ),
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

                    same_subtree = pkg_path == prefix or pkg_path.startswith(prefix + "/")
                    if depth >= 3 and not same_subtree and not prefix.startswith(("build/bazel/", "build/images/", "bundles/")):
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
                    # Allow package-scoped visibility to an ancestor test_suite package on test
                    # targets before outside_changes wires the test into that ancestor BUILD.bazel.
                    if is_pending_ancestor_test_suite_vis(
                        caller_pkg,
                        pkg_path,
                        is_test_rule,
                        per_target_direct_rdeps.get(target_name, {}),
                    ):
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

        if (
            preserved_gn_vis
            and new_entries
            and only_scope_entries
            and len(findings) == findings_before_target
        ):
            check_visibility_tightened_from_gn(
                target_name, rel_path, vis_kw.lineno, vis_vals, vis_scopes, preserved_gn_vis, pkg_path
            )

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
                    if kind == "pkg" and is_pending_ancestor_test_suite_vis(
                        sp,
                        pkg_path,
                        is_test_rule,
                        per_target_direct_rdeps.get(target_name, {}),
                    ):
                        continue
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

        has_target_err = any(
            f.get("severity", "").lower() != "info" for f in findings[target_start_findings:]
        )
        if only_scope_entries and not has_target_err and (not preserved_gn_vis or new_entries):
            clean_audited_targets.add((rel_path, target_name))
            record_intentional_visibility_narrowing(
                target_name, rel_path, node.lineno, vis_scopes, orig_gn_vis_map, pkg_path
            )

record_diff_visibility_narrowed_info(clean_audited_targets)
print(json.dumps(findings, indent=2))
PYEOF
