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
import shutil
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])


def normalize_target_pkg(raw_dir, base_workdir):
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


target_pkg = normalize_target_pkg(sys.argv[2], workdir)
default_target = os.path.basename(target_pkg)

bazel_file = os.path.join(workdir, target_pkg, "BUILD.bazel")
gn_file = os.path.join(workdir, target_pkg, "BUILD.gn")

bazel_targets = []
package_targets = set()
non_gn_synced_targets = set()
test_rule_targets = set()
testonly_targets = set()
alias_map = {}

BAZEL_NON_GN_SYNCED_RULES = {
    "cc_test",
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
    "go_test",
}

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


if os.path.isfile(bazel_file):
    try:
        with open(bazel_file, "r", encoding="utf-8") as f:
            bazel_src = f.read()
        bazel_src_lines = bazel_src.splitlines()
        tree = ast.parse(bazel_src, filename=bazel_file)
        for node in ast.walk(tree):
            if isinstance(node, ast.Call):
                func = (
                    node.func.id
                    if isinstance(node.func, ast.Name)
                    else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
                )
                tname = None
                actual = None
                is_to = False
                for kw in node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        tname = kw.value.value
                    elif kw.arg == "actual" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        actual = kw.value.value
                    elif kw.arg == "testonly":
                        if isinstance(kw.value, ast.Constant) and kw.value.value is True:
                            is_to = True
                        elif isinstance(kw.value, ast.Name) and kw.value.id in ("True", "true"):
                            is_to = True
                if tname:
                    bazel_targets.append(tname)
                    if is_to:
                        testonly_targets.add(tname)
                    if func in ("fx_package", "fuchsia_package", "fx_unittest_package"):
                        package_targets.add(tname)
                    if func in BAZEL_NON_GN_SYNCED_RULES:
                        non_gn_synced_targets.add(tname)
                        test_rule_targets.add(tname)
                    if has_bazel2gn_skip_comment(bazel_src_lines, node.lineno) and func not in (
                        "fx_package",
                        "fuchsia_package",
                    ):
                        non_gn_synced_targets.add(tname)
                    if func == "alias" and actual and actual.startswith(":"):
                        alias_map[tname] = actual[1:]
    except Exception:
        pass

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

gn_only_targets = {"tests", "benchmarks"}
gn_nonconvertible_targets = {"tests", "benchmarks"}
gn_forwarding_groups = {}


# Map non-testonly GN forwarding group('<g>') targets above the sentinel to the Bazel
# targets ':bt' they forward in deps/public_deps.
def extract_gn_forwarding_groups(gn_text):
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


if os.path.isfile(gn_file):
    try:
        with open(gn_file, "r", encoding="utf-8", errors="ignore") as f:
            gn_src = f.read()
        for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE):
            if m.group(1) in GN_PACKAGE_TEMPLATES:
                package_targets.add(m.group(2))
        if bazel_targets:
            has_sentinel = "BAZEL2GN SENTINEL" in gn_src
            above_sentinel, below_sentinel = (
                gn_src.split("BAZEL2GN SENTINEL", 1) if has_sentinel else (gn_src, "")
            )
            env_target_dir = os.environ.get("PLANTER_TARGET_DIR", "").strip().strip("/")
            gn_forwarding_groups = extract_gn_forwarding_groups(above_sentinel)
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', above_sentinel, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if t in gn_forwarding_groups:
                    continue
                if t not in bazel_targets:
                    gn_only_targets.add(t)
                if (
                    tmpl not in CONVERTIBLE_GN_TEMPLATES
                    and (
                        t not in bazel_targets
                        or t in ("tests", "benchmarks")
                        or tmpl == "bazel_test_suite"
                    )
                ) or (not has_sentinel and env_target_dir and target_pkg != env_target_dir):
                    gn_nonconvertible_targets.add(t)
            below_synced_targets = {
                m.group(2)
                for m in re.finditer(
                    r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                    below_sentinel,
                    flags=re.MULTILINE,
                )
                if m.group(2) in bazel_targets
            }
            non_gn_synced_targets.difference_update(below_synced_targets)
            package_targets.difference_update(below_synced_targets)
        else:
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if tmpl in CONVERTIBLE_GN_TEMPLATES and t not in bazel_targets:
                    bazel_targets.append(t)
            gn_forwarding_groups = extract_gn_forwarding_groups(gn_src)
            for m in re.finditer(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)', gn_src, flags=re.MULTILINE):
                tmpl, t = m.group(1), m.group(2)
                if t in gn_forwarding_groups:
                    continue
                if tmpl not in CONVERTIBLE_GN_TEMPLATES and t not in bazel_targets:
                    gn_only_targets.add(t)
                    gn_nonconvertible_targets.add(t)
    except Exception:
        pass

VG_ROOT = "vendor/" + "google"
VG_SUBPKGS = "//" + VG_ROOT + ":__subpackages__"


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


# Discover all build files mentioning target_pkg across all repos and ancestor BUILD.gn files.
# Using git grep per repository avoids false-empty results when workdir is a git worktree under
# .jiri_root/worktrees/<slot> (where the parent checkout's .gitignore ignores /.jiri_root/) or
# when sub-repositories are listed in the root .gitignore.
def discover_referencing_build_files(pkg_path):
    label_prefix = f"//{pkg_path}"
    pattern_args = ["-e", label_prefix]
    if default_target:
        pattern_args.extend(["-e", f"../{default_target}"])
    found = []
    repos = discover_git_repos()
    change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
    used_git_grep = False
    for repo in repos:
        if not os.path.exists(os.path.join(repo, ".git")):
            continue
        grep_cmd = (
            [
                "git",
                "-C",
                repo,
                "grep",
                "--untracked",
                "-l",
                "--fixed-strings",
            ]
            + pattern_args
            + [
                "--",
                "*BUILD.gn",
                "*BUILD.bazel",
                "*.gni",
                "*.bzl",
            ]
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
                    found.append(os.path.join(repo, line.strip()))
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
                        found.append(direct_cand)
                    d = os.path.dirname(rel_line)
                    for bname in ("BUILD.bazel", "BUILD.gn"):
                        cand = os.path.join(repo, d, bname) if d else os.path.join(repo, bname)
                        if os.path.isfile(cand):
                            found.append(cand)
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
                + pattern_args
                + rg_globs
                + repos
            )
            try:
                proc = subprocess.run(
                    rg_cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
                )
                if proc.returncode in (0, 1):
                    found.extend(line.strip() for line in proc.stdout.splitlines() if line.strip())
            except OSError:
                pass
    pkg_parts = pkg_path.split("/")
    for i in range(1, len(pkg_parts)):
        anc_gn = os.path.join(workdir, "/".join(pkg_parts[:i]), "BUILD.gn")
        if os.path.isfile(anc_gn):
            found.append(anc_gn)
    return found


files = discover_referencing_build_files(target_pkg)

abs_label_re = re.compile(r'"//' + re.escape(target_pkg) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')

true_rdep_packages = set()
visibility_only_packages = set()
gn_only_ref_packages = set()
excluded_gn_only_references = set()
per_target_rdeps = {t: set() for t in bazel_targets}


# Map referenced GN/Bazel target labels to their owning BUILD.bazel target
def owning_bazel_target(sub):
    if sub in per_target_rdeps:
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
def is_gn_only_target_ref(rel, caller_pkg, sub, owner, is_pkg_target):
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

# Collect .gni templates and list variables that expand to dependency references on targets in
# target_pkg so callers that import the .gni and invoke the template or variable are recognized as GN rdeps.
def collect_gni_template_matches(files_list, label_re):
    out = {}
    for fp in sorted(set(files_list)):
        g_rel = os.path.relpath(fp, workdir)
        if not g_rel.endswith(".gni") or os.path.basename(g_rel) == "visibility.gni":
            continue
        try:
            raw = open(fp, "r", encoding="utf-8", errors="ignore").read()
        except Exception:
            continue
        strp = re.sub(
            r"\b(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]",
            "",
            re.sub(r"#[^\n]*", "", raw),
            flags=re.DOTALL,
        )
        t_names = set(re.findall(r'\btemplate\(\s*"([^"]+)"\s*\)', strp))
        v_names = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=\s*\[", strp)) - {
            "visibility",
            "packages",
        }
        m_list = list(label_re.finditer(strp))
        if (t_names or v_names) and m_list:
            out[f"//{g_rel}"] = (t_names, v_names, m_list)
    if out:
        gni_pat_args = [arg for lbl in sorted(out) for arg in ("-e", lbl)]
        for repo in discover_git_repos():
            if not os.path.exists(os.path.join(repo, ".git")):
                continue
            try:
                proc = subprocess.run(
                    ["git", "-C", repo, "grep", "--untracked", "-l", "--fixed-strings"]
                    + gni_pat_args
                    + ["--", "*BUILD.gn", "*.gni"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                for line in proc.stdout.splitlines():
                    if line.strip():
                        files_list.append(os.path.join(repo, line.strip()))
            except Exception:
                pass
    return out

gni_template_map = collect_gni_template_matches(files, abs_label_re)

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
    stripped = re.sub(r"\b(?:visibility|packages)\s*\+?=\s*\[[^\]]*\]", "", no_comments, flags=re.DOTALL)

    # Match both ancestor-to-descendant ("sub/pkg") and sibling/cousin ("../pkg") relative GN labels
    rel_sub = None
    if rel.endswith((".gn", ".gni")) and pkg and target_pkg:
        try:
            if os.path.commonpath([target_pkg, pkg]):
                cand_rel = os.path.relpath(target_pkg, pkg)
                if not cand_rel.startswith("../../.."):
                    rel_sub = cand_rel
        except ValueError:
            pass
    elif target_pkg.startswith(pkg + "/"):
        rel_sub = target_pkg[len(pkg) + 1 :]
    rel_label_re = (
        re.compile(r'"' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"')
        if rel_sub
        else None
    )

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
        visibility_only_packages.add(pkg)
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
            caller_pkg = (
                "bundles/assembly"
                if (pkg == "bundles/assembly" or pkg.startswith("bundles/assembly/"))
                else pkg
            )
            true_rdep_packages.add(caller_pkg)
            for bt in gn_forwarding_groups[sub]:
                per_target_rdeps[bt].add(caller_pkg)
            continue
        if sub in gn_only_targets and sub not in bazel_targets:
            excluded_gn_only_references.add(f"//{pkg} (:{sub})")
            gn_only_ref_packages.add(pkg)
            continue
        is_pkg_target = sub in package_targets or (
            sub == default_target and len(bazel_targets) == 1 and bazel_targets[0] in package_targets
        )
        owner = owning_bazel_target(sub)
        if owner is None and sub == default_target and len(bazel_targets) == 1:
            owner = bazel_targets[0]
        if is_gn_only_target_ref(rel, pkg, sub, owner, is_pkg_target):
            excluded_gn_only_references.add(f"//{pkg} (:{sub})")
            gn_only_ref_packages.add(pkg)
            continue
        caller_pkg = (
            "bundles/assembly"
            if (pkg == "bundles/assembly" or pkg.startswith("bundles/assembly/"))
            else pkg
        )
        true_rdep_packages.add(caller_pkg)
        if owner is not None:
            per_target_rdeps[owner].add(caller_pkg)

for alias_name, actual_name in alias_map.items():
    if alias_name in per_target_rdeps and actual_name in per_target_rdeps:
        combined = per_target_rdeps[alias_name] | per_target_rdeps[actual_name]
        per_target_rdeps[alias_name] = set(combined)
        per_target_rdeps[actual_name] = set(combined)

visibility_only_packages -= true_rdep_packages
gn_only_ref_packages -= true_rdep_packages

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
                if d2 in BANNED_UMBRELLA_AREAS:
                    for item in recommend_narrow_visibility(d2_group):
                        result.add(item)
                elif len(d2_group) >= 2 or d2 == "bundles/assembly":
                    result.add(f"//{d2}:__subpackages__")
                else:
                    result.add(f"//{d2_group[0]}:__pkg__")
    return sorted(result)

def is_globally_used_target(rdep_pkgs):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    narrow = recommend_narrow_visibility(pkgs)
    areas = {get_pkg_area(p) for p in pkgs if get_pkg_area(p)}
    return len(areas) >= 6 and (len(narrow) > 15 or len(pkgs) >= 20)

is_test_pkg = (
    target_pkg.startswith("src/testing/")
    or target_pkg.startswith("src/lib/testing/")
    or target_pkg.startswith("testing/")
    or "/testing/" in target_pkg
)

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
def has_only_test_or_bench_rdeps(rdep_pkgs):
    pkgs = [
        p.strip("/")
        for p in rdep_pkgs
        if p.strip("/")
        and p.strip("/") != target_pkg
        and p.strip("/") != VG_ROOT
        and not p.strip("/").startswith(VG_ROOT + "/")
    ]
    return bool(pkgs) and all(is_test_or_bench_caller_pkg(p) for p in pkgs)


def collapse_src_rollup_visibility(rdep_pkgs):
    raw_pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    if not raw_pkgs:
        return []
    has_src = any(p == "src" or p.startswith("src/") for p in raw_pkgs)
    non_src = [p for p in raw_pkgs if p != "src" and not p.startswith("src/")]
    res = set(recommend_narrow_visibility(non_src))
    if has_src:
        res.add("//src:__subpackages__")
    return sorted(res)

BANNED_TOP_LEVEL_SUBPKGS = {
    "src",
    "sdk",
    "build",
    "zircon",
    "third_party",
}

def get_orig_gn_visibility_scopes(pkg_path):
    """Returns {target_name: [(scope_pkg, 'pkg'|'subpackages'), ...]} for targets in the pre-migration
    BUILD.gn (above BAZEL2GN SENTINEL) that had an explicit non-wildcard visibility = [...] restriction."""
    gn_rel = f"{pkg_path}/BUILD.gn" if pkg_path else "BUILD.gn"
    change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"

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
        for m in target_re.finditer(above_sentinel):
            tname = m.group(2)
            if tname in out:
                continue
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
            if re.search(r"\bvisibility\s*\+=", tbody):
                continue
            vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
            if vm:
                scopes = parse_gn_vis_list(vm.group(1))
                if scopes:
                    out[tname] = scopes
    return out


orig_gn_vis_map = get_orig_gn_visibility_scopes(target_pkg)


def get_preserved_gn_visibility(tname, rdep_pkgs):
    if not tname or tname in non_gn_synced_targets:
        return None
    gn_scopes = orig_gn_vis_map.get(tname)
    if not gn_scopes:
        return None
    pkgs = sorted(
        set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg)
    )
    has_vg = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in pkgs)
    valid_scopes = []
    for sp, kind in gn_scopes:
        if sp == target_pkg and kind == "pkg":
            continue
        if sp == VG_ROOT or sp.startswith(VG_ROOT + "/"):
            if has_vg:
                valid_scopes.append((VG_ROOT, "subpackages"))
            continue
        if kind == "subpackages":
            if not sp or sp in BANNED_TOP_LEVEL_SUBPKGS or sp in BANNED_UMBRELLA_AREAS:
                return None
            parts = sp.split("/")
            if len(parts) == 2 and parts[0] in ("src", "sdk"):
                if not (target_pkg == sp or target_pkg.startswith(sp + "/")):
                    return None
            if not os.path.isdir(os.path.join(workdir, sp)):
                continue
            valid_scopes.append((sp, "subpackages"))
        else:
            if (
                not sp
                or not os.path.isdir(os.path.join(workdir, sp))
                or sp in visibility_only_packages
                or sp in gn_only_ref_packages
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
        preserved.update(recommend_narrow_visibility(uncovered))
    if not preserved or len(preserved) > 15:
        return None
    return sorted(preserved)


def recommend_visibility(rdep_pkgs, is_test_support=False, target_name=None):
    pkgs = sorted(set(p.strip("/") for p in rdep_pkgs if p.strip("/") and p.strip("/") != target_pkg))
    narrow = recommend_narrow_visibility(pkgs)
    globally_used = is_globally_used_target(pkgs)
    if target_name and not globally_used:
        preserved = get_preserved_gn_visibility(target_name, pkgs)
        if preserved:
            return preserved
    is_shared_test_bench = is_test_support or (
        target_name not in non_gn_synced_targets and has_only_test_or_bench_rdeps(pkgs)
    )
    if is_shared_test_bench and len(get_distinct_areas(pkgs)) >= 2:
        return ["//:__subpackages__"]
    if globally_used or len(narrow) > 8:
        rollup = recommend_area_rollup_visibility(pkgs)
        distinct_areas = get_distinct_areas(pkgs)
        if globally_used and len(rollup) > 15 and (len(pkgs) >= 50 or len(distinct_areas) > 15):
            if len(pkgs) >= 50:
                return ["//:__subpackages__"]
            src_collapsed = collapse_src_rollup_visibility(pkgs)
            if len(src_collapsed) <= 15:
                return src_collapsed
            return ["//:__subpackages__"]
        if len(rollup) <= len(narrow):
            return rollup
    return narrow

print(json.dumps({
    "target_package": f"//{target_pkg}",
    "true_rdep_packages": sorted({f"//{sanitize_rdep_pkg(p)}" for p in true_rdep_packages}),
    "distinct_rdep_areas": get_distinct_areas(true_rdep_packages),
    "is_globally_used": is_globally_used_target(true_rdep_packages),
    "recommended_visibility": recommend_visibility(
        true_rdep_packages,
        is_test_support=is_test_pkg or any(t in testonly_targets for t in bazel_targets),
        target_name=default_target if default_target in orig_gn_vis_map else None,
    ),
    "per_target_rdeps": {
        t: sorted({f"//{sanitize_rdep_pkg(p)}" for p in rset}) for t, rset in per_target_rdeps.items()
    },
    "per_target_is_globally_used": {
        t: is_globally_used_target(rset) for t, rset in per_target_rdeps.items()
    },
    "per_target_recommended_visibility": {
        t: recommend_visibility(rset, is_test_support=is_test_pkg or t in testonly_targets, target_name=t)
        for t, rset in per_target_rdeps.items()
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
