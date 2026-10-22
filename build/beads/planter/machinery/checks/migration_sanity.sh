#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check for GN-to-Bazel migration completeness and dual-build invariants:
# 1. skipped_convertible_target (error): flags any convertible library/binary/code target in
#    BUILD.bazel (e.g., fx_cc_library, cc_library, rustc_library, go_library, fidl_library)
#    annotated with `# @bazel2gn:skip` when BUILD.gn is present (especially reference or
#    bitrot-prevention libraries depended upon by `group("tests")` or duplicated above the
#    `## BAZEL2GN SENTINEL` marker in BUILD.gn), except package-private `testonly = True`
#    helper targets whose only consumers are Bazel-only tests in the same package.
# 2. duplicate_target_above_sentinel (error): flags any convertible target in BUILD.gn defined
#    above `## BAZEL2GN SENTINEL` when a target of the same name is defined in BUILD.bazel or
#    should be generated below the sentinel by `fx bazel2gn`. bazel2gn generates the GN copy of
#    a Bazel test, so a hand-written GN test (`rustc_test`, `go_test`) above the sentinel is
#    duplicate_test_above_sentinel (error) when BUILD.bazel defines the same test (name, output
#    name or `rustc_*` `<name>_test`), otherwise gn_test_kept_above_sentinel (info: allowed while
#    Bazel cannot run it yet).
# 3. gn_test_package_not_migrated / duplicate_gn_test_package (error): when the checkout has
#    `//build/bazel/rules/testing:fx_test.bzl`, flags a GN `fuchsia_unittest_package` /
#    `fuchsia_test_package` that only packages C/C++ tests Bazel can build (a BUILD.bazel
#    cc/fx_cc binary, or a GN test executable with only C/C++ sources whose deps all have a
#    Bazel build) and uses nothing Bazel device tests cannot express yet (other test_specs than
#    log_settings.max_severity, test_type, subpackages, data_deps, ...), or that duplicates a
#    Bazel `fx_package`. Rust device tests migrate the same way (explicit `.cml` with the Rust
#    test runner shard, `fx_packaged_binary` of the `rustc_*` `:<name>_test` or a `rustc_test`).
#    Tests whose language is unknown never trigger it. A GN package named `<bazel package>-gn`
#    is the hand-written GN copy of a Bazel test package and is not flagged.
# 4. defs_bzl_reexport_load (error): flags any BUILD.bazel `load()` from a `*:defs.bzl` file
#    (such as `//build/bazel/rules/rust:defs.bzl` or `@rules_cc//cc:defs.bzl`) of a symbol that
#    `defs.bzl` only re-exports from another `.bzl` file in the same directory. Symbols defined
#    directly in `defs.bzl` (including `//build/bazel/rules/host:defs.bzl`) or loaded from a
#    `private/` or other subdirectory are allowed.
# While a library has users in both build systems its tests run in both: tests are defined in
# Bazel and bazel2gn generates the GN copy (docs/development/build/bazel_concepts/tests.md,
# "Testing libraries in both build systems"), so tests without `# @bazel2gn:skip` are expected.
# 5. unjustified_test_skip (error): flags `# @bazel2gn:skip` on a test target or test attribute
#    (`with_unit_tests`, `with_host_unit_tests`, `test_deps`) in a sentinel-synced BUILD.bazel
#    unless one of the valid conditions holds: (T1) the target under test in the same
#    BUILD.bazel is itself skipped, (T2) `fx_unittest_package` sets `resources = [...]`,
#    (T3) a host test sets `data`/`test_data`/`test_label`, (T4) the rule is an unsupported
#    test binary rule (`cc_test`, `host_cc_test`, `host_py_test`, `host_shac_test`, `host_test`)
#    or `rustc_test` with cross-package `srcs`/`crate` or conflicting `rustc_env`, or (T5) a
#    human explanatory comment precedes `# @bazel2gn:skip`.
# 6. test_skip_needs_reviewer_attention (info): a test skip whose comment starts with
#    `# Needs reviewer attention (bazel2gn skip):` uses the dual-testing cost exception, which only
#    a human can confirm. It is never rejected, but always reported for the reviewer.

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

CONVERTIBLE_BAZEL_RULES = {
    "fx_cc_library",
    "cc_library",
    "fx_cc_library_headers",
    "cc_binary",
    "cc_shared_library_zx",
    "cc_source_library_zx",
    "cc_static_library_zx",
    "idk_cc_shared_library",
    "idk_cc_shared_library_zx",
    "idk_cc_source_library",
    "idk_cc_source_library_zx",
    "idk_cc_static_library",
    "idk_cc_static_library_zx",
    "rustc_library",
    "rust_library",
    "rustc_binary",
    "rust_binary",
    "rustc_proc_macro",
    "rust_proc_macro",
    "rustc_check_cfg_build_flags",
    "go_library",
    "go_binary",
    "py_library",
    "py_binary",
    "fidl_library",
    "zither_fidl_library",
}

# Plain GN `shared_library()` is intentionally excluded: `build/tools/bazel2gn/constants.go`
# maps `cc_library` and `fx_cc_library` only to `static_library` or `source_set` (and
# `idk_cc_shared_library` / `cc_shared_library_zx` only to `sdk_shared_library` / `zx_library`),
# so a plain `shared_library()` in a dual-build package has no bazel2gn equivalent and must
# remain above `## BAZEL2GN SENTINEL` to preserve GN target type parity.
CONVERTIBLE_GN_TEMPLATES = {
    "static_library",
    "source_set",
    "executable",
    "library_headers",
    "rustc_library",
    "rustc_binary",
    "rustc_macro",
    "rustc_dylib",
    "go_library",
    "go_binary",
    "python_library",
    "python_binary",
    "fidl",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
}

# A hand-written GN test above the sentinel either duplicates the GN copy bazel2gn generates
# from the Bazel test or is a GN-only test, never an unmigrated convertible library
# (duplicate_test_above_sentinel / gn_test_kept_above_sentinel).
GN_TEST_TEMPLATES = {"rustc_test", "go_test"}

BAZEL_TEST_RULES = {
    "rustc_test",
    "go_test",
    "host_go_test",
    "host_rustc_test",
    "fx_unittest_package",
    "cc_test",
    "host_cc_test",
    "host_py_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
}
UNSUPPORTED_TEST_BINARY_RULES = {
    "cc_test",
    "host_cc_test",
    "host_py_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
}
COMMENT_REASON_RE = re.compile(r"^#(?!\s*@bazel2gn\b)\s*\S")
# Marker for a `# @bazel2gn:skip` on a test that a coder justified with exception (c) of
# docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems (dual-testing cost
# outweighs the risk). It is never rejected, but always surfaced as INFO for a human to confirm.
NEEDS_ATTENTION_RE = re.compile(r"^#\s*Needs reviewer attention \(bazel2gn skip\):", re.IGNORECASE)

EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
    "third_party/rust_crates",
)

RUST_UNIT_TEST_OWNERS = {"rustc_library", "rustc_binary", "rustc_proc_macro", "rust_library", "rust_binary", "rust_proc_macro"}
UNIT_TEST_ATTRS = ("with_unit_tests", "with_host_unit_tests", "test_deps", "test_compile_data", "test_rustc_flags")

# With fx_test() in the checkout, GN device test packages migrate to Bazel unless they use
# something Bazel device tests cannot express yet.
FX_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl"))
GN_TEST_PACKAGE_TEMPLATES = ("fuchsia_unittest_package", "fuchsia_test_package")
# C/C++ and Rust device tests migrate (Rust via fx_packaged_binary of a rust_test).
MIGRATABLE_TEST_LANGUAGES = ("cc", "rust")
CC_TEST_BAZEL_RULES = {"fx_cc_binary", "cc_binary", "cc_test", "fx_cc_test"}
GN_CC_TEST_TEMPLATES = {"executable", "test", "cc_test_executable"}
CC_SOURCE_SUFFIXES = (".cc", ".cpp", ".cxx", ".c", ".h", ".hpp")
GN_TEST_PACKAGE_BLOCKERS = (
    "test_type",
    "subpackages",
    "renameable_subpackages",
    "data_deps",
    "required_offers",
    "required_uses",
    "restricted_features",
    "deprecated_legacy_test_execution",
    "manifest_deps",
)


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


candidate_dirs = set()
if target_dir:
    candidate_dirs.add(target_dir)

def normalize_git_rev(rev):
    """Map jj relative revsets (@, @-, @--) to git HEAD equivalents for git commands."""
    rev = (rev or "").strip() or "HEAD"
    m = re.fullmatch(r"@(-*)", rev)
    if m:
        return f"HEAD~{len(m.group(1))}" if m.group(1) else "HEAD"
    return rev


changed = set()
# The task's change: everything since PLANTER_CHANGE_BASE ("HEAD~1" when HEAD is the task's own
# commit, "HEAD" when HEAD is unrelated upstream history) plus untracked files.
change_base = normalize_git_rev(os.environ.get("PLANTER_CHANGE_BASE", ""))
changed.update(git_lines(["diff", "--name-only", change_base]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
        ):
            candidate_dirs.add(d)


def parse_bazel_targets(bazel_path, pkg_dir=""):
    try:
        text = open(bazel_path, encoding="utf-8").read()
    except Exception:
        return []
    lines = text.splitlines()
    targets = []
    try:
        tree = ast.parse(text, filename=bazel_path)
    except Exception:
        return []
    for node in tree.body:
        call = None
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            call = node.value
        if not call or not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        tname = None
        binary_ref = None
        pkg_name = None
        unit_tests = False
        unit_tests_refs = []
        host_only = False
        testonly = False
        vis_items = []
        dep_refs = set()
        gn_dep_refs = set()
        is_test_pkg = rule == "fx_unittest_package"
        has_resources = False
        has_test_data = False
        has_rustc_env = False
        crate_ref = None
        cross_pkg_srcs_or_crate = False
        local_test_refs = []
        skipped_test_attrs = []
        attention_test_attrs = []
        for kw in call.keywords:
            if kw.arg == "target_compatible_with":
                tcw_seg = ast.get_source_segment(text, kw.value) or ""
                if "HOST_OS_CONSTRAINTS" in tcw_seg or "HOST_CONSTRAINTS" in tcw_seg:
                    host_only = True
            if kw.arg == "test_components" and not (
                isinstance(kw.value, ast.List) and not kw.value.elts
            ):
                is_test_pkg = True
            if (
                kw.arg == "testonly"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value is True
            ):
                is_test_pkg = True
            attr_skipped = kw.arg in UNIT_TEST_ATTRS and has_skip_comment(lines, kw.lineno)
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                tname = kw.value.value
            elif kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
                testonly = True
            elif kw.arg == "visibility" and isinstance(kw.value, ast.List):
                for elt in kw.value.elts:
                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                        vis_items.append(elt.value)
            elif kw.arg == "binary" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                binary_ref = kw.value.value.lstrip(":")
            elif kw.arg == "package_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                pkg_name = kw.value.value
            elif kw.arg == "resources":
                has_resources = True
            elif kw.arg in ("data", "test_data", "test_label"):
                has_test_data = True
            elif kw.arg == "rustc_env":
                has_rustc_env = True
            elif kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
                testonly = True
            elif kw.arg == "crate":
                crate_seg = ast.get_source_segment(text, kw.value) or ""
                if '"//' in crate_seg:
                    cross_pkg_srcs_or_crate = True
                if isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    if kw.value.value.startswith(":"):
                        crate_ref = kw.value.value[1:]
                        local_test_refs.append(crate_ref)
            elif kw.arg == "srcs":
                srcs_seg = ast.get_source_segment(text, kw.value) or ""
                if '"//' in srcs_seg:
                    cross_pkg_srcs_or_crate = True
            elif kw.arg == "unit_tests":
                if isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            unit_tests_refs.append(elt.value.lstrip(":"))
                            if elt.value.startswith(":"):
                                local_test_refs.append(elt.value[1:])
                elif isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    unit_tests_refs.append(kw.value.value.lstrip(":"))
                    if kw.value.value.startswith(":"):
                        local_test_refs.append(kw.value.value[1:])
            elif kw.arg in ("with_unit_tests", "with_host_unit_tests") and isinstance(kw.value, ast.Constant):
                unit_tests = unit_tests or kw.value.value is True or kw.value.value in ("fuchsia", "host", "both")
            if kw.arg not in ("name", "visibility", "package_name"):
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        sval = sub.value
                        local_ref = None
                        if sval.startswith(":"):
                            local_ref = sval[1:]
                        elif pkg_dir and sval.startswith(f"//{pkg_dir}:"):
                            local_ref = sval.split(":", 1)[1]
                        if local_ref:
                            dep_refs.add(local_ref)
                            if not attr_skipped and not has_skip_comment(lines, sub.lineno):
                                gn_dep_refs.add(local_ref)
            if kw.arg in ("embed", "deps"):
                dep_seg = ast.get_source_segment(text, kw.value) or ""
                local_test_refs.extend(re.findall(r'":([^"]+)"', dep_seg))
            if kw.arg in UNIT_TEST_ATTRS:
                attr_skip = None
                attr_reason = False
                attr_attention = False
                if 1 <= kw.lineno <= len(lines) and "@bazel2gn:skip" in lines[kw.lineno - 1]:
                    attr_skip = kw.lineno
                kidx = kw.lineno - 2
                while kidx >= node.lineno - 1:
                    ks = lines[kidx].strip()
                    if not ks:
                        kidx -= 1
                        continue
                    if ks.startswith("#"):
                        if "@bazel2gn:skip" in ks:
                            attr_skip = kidx + 1
                        if COMMENT_REASON_RE.search(ks):
                            attr_reason = True
                        if NEEDS_ATTENTION_RE.search(ks):
                            attr_attention = True
                        kidx -= 1
                        continue
                    break
                if attr_skip is not None:
                    skipped_test_attrs.append((kw.arg, attr_skip, attr_reason))
                    if attr_attention:
                        attention_test_attrs.append((kw.arg, attr_skip))
        if not tname:
            continue
        start_line = node.lineno
        skip_line = None
        has_skip_reason = False
        has_attention_marker = False
        idx = start_line - 2
        while idx >= 0:
            s = lines[idx].strip()
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    skip_line = idx + 1
                if COMMENT_REASON_RE.search(s):
                    has_skip_reason = True
                if NEEDS_ATTENTION_RE.search(s):
                    has_attention_marker = True
                idx -= 1
                continue
            break
        targets.append({
            "rule": rule,
            "name": tname,
            "line": start_line,
            "skip_line": skip_line,
            "has_skip_reason": has_skip_reason,
            "has_attention_marker": has_attention_marker,
            "binary_ref": binary_ref,
            "package_name": pkg_name or tname,
            "unit_tests": unit_tests,
            "unit_tests_refs": unit_tests_refs,
            "host_only": host_only,
            "testonly": testonly,
            "vis_items": vis_items,
            "dep_refs": dep_refs,
            "gn_dep_refs": gn_dep_refs,
            "is_test_pkg": is_test_pkg,
            "has_resources": has_resources,
            "has_test_data": has_test_data,
            "has_rustc_env": has_rustc_env,
            "crate_ref": crate_ref,
            "cross_pkg_srcs_or_crate": cross_pkg_srcs_or_crate,
            "local_test_refs": local_test_refs,
            "skipped_test_attrs": skipped_test_attrs,
            "attention_test_attrs": attention_test_attrs,
        })
    return targets


# Check whether a convertible Bazel target is a helper or sub-library
# whose only consumers in BUILD.bazel are Bazel-only targets (leaving any
# bazel2gn-generated GN target orphaned in BUILD.gn).
def is_bazel_only_private_test_dep(
    target, bazel_targets, gn_info, pkg_dir, packaged_binaries, gn_kept_pkg_test_deps=()
):
    tname = target["name"]
    vis = target.get("vis_items") or []
    priv_vis = {"//visibility:private", ":__pkg__", f"//{pkg_dir}:__pkg__"}
    if not (target["skip_line"] is not None and target.get("has_skip_reason")) and (
        not target.get("testonly") or not vis or not all(v in priv_vis for v in vis)
    ):
        return False
    if tname in gn_kept_pkg_test_deps:
        return False
    if gn_info:
        if tname in gn_info["pre_targets"]:
            return False
        ref_pat = re.compile(
            r'"(?::|//'
            + re.escape(pkg_dir)
            + r':)'
            + re.escape(tname)
            + r'(?:_[A-Za-z0-9_]+)?(?:"|\()'
        )
        for ptmpl, _, pbody in gn_info["pre_targets"].values():
            if ptmpl not in ("bazel_test_suite", "bazel_host_tool") and ref_pat.search(pbody):
                return False
    has_bazel_test_consumer = False
    for other in bazel_targets:
        if other["name"] == tname:
            continue
        is_gn_converted_consumer = (
            other["skip_line"] is None
            and (other["rule"] in CONVERTIBLE_BAZEL_RULES or other["rule"] in BAZEL_TEST_RULES)
            and (other["name"] not in packaged_binaries or other["rule"] in BAZEL_TEST_RULES)
        )
        if is_gn_converted_consumer and any(
            r == tname or r.startswith(tname + "_")
            for r in other.get("gn_dep_refs", ())
        ):
            return False
        if any(
            r == tname or r.startswith(tname + "_")
            for r in other.get("dep_refs", ())
        ):
            has_bazel_test_consumer = True
    return has_bazel_test_consumer


# GN-only target attributes and constructs bazel2gn cannot emit; such a target stays
# hand-written above the sentinel.
GN_ONLY_ATTR_RE = re.compile(
    r"(?<![\w.])(?:exclude_toolchain_tags\s*\+?=|configs\s*-=|disable_syslog_backend\s*=)"
)
GN_BLOCKED_CONSTRUCT_RE = re.compile(
    r"\b(?:loadable_module|fuchsia_structured_config_[a-z0-9_]+)\s*\("
    r"|\(\$[a-zA-Z0-9_]*toolchain\)"
)


# Check whether a pre-sentinel GN target is a testonly binary packaged by a GN test
# package or component above the sentinel (never converted below the sentinel by bazel2gn).
def is_packaged_testonly_gn_binary(gname, gtmpl, gbody, pre_targets, bazel_names):
    if gtmpl not in ("executable", "rustc_binary", "go_binary", "python_binary"):
        return False
    if gname in bazel_names or not re.search(r"\btestonly\s*=\s*true\b", gbody):
        return False
    pkg_tmpls = (
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "fuchsia_component",
        "bootfs_test",
    )
    ref_re = re.compile(r'":' + re.escape(gname) + r'(?:"|\()')
    return any(
        ptmpl in pkg_tmpls and ref_re.search(pbody)
        for ptmpl, _, pbody in pre_targets.values()
    )


# Inspect a pre-sentinel GN target's local and external dependencies for unmigrated
# packages, declare_args() packages, and GN-only constructs that block Bazel conversion.
def inspect_gn_target_deps(gbody, pkg_dir, pre_targets, bazel_names):
    unmigrated_deps = []
    declare_args_deps = []
    gn_blocked_dep = False
    vis_re = re.compile(r"\bvisibility\s*\+?=\s*\[[^\]]*\]")
    local_ref_re = re.compile(
        r'"(?::([A-Za-z0-9_.-]+)|//'
        + re.escape(pkg_dir)
        + r'(?::([A-Za-z0-9_.-]+))?)"'
    )
    seen_local = set()
    body_queue = [gbody]
    while body_queue:
        cur_body = vis_re.sub("", re.sub(r"#[^\n]*", "", body_queue.pop(0)))
        for lm in local_ref_re.finditer(cur_body):
            lname = lm.group(1) or lm.group(2) or os.path.basename(pkg_dir)
            if lname in seen_local or lname in bazel_names or lname not in pre_targets:
                continue
            seen_local.add(lname)
            ltmpl, _, lbody = pre_targets[lname]
            if (
                GN_ONLY_ATTR_RE.search(lbody)
                or GN_BLOCKED_CONSTRUCT_RE.search(lbody)
                or (ltmpl not in CONVERTIBLE_GN_TEMPLATES and ltmpl != "group")
            ):
                gn_blocked_dep = True
            else:
                body_queue.append(lbody)
        for dm in re.finditer(r'"//([^":(\s]+)(?::[^"(\s]+)?"', cur_body):
            dep_pkg = dm.group(1).strip("/")
            if (
                not dep_pkg
                or dep_pkg == pkg_dir
                or dep_pkg.startswith(("build/", "prebuilt/", "third_party/", "out/"))
            ):
                continue
            dep_gn = os.path.join(workdir, dep_pkg, "BUILD.gn")
            dep_bazel = os.path.join(workdir, dep_pkg, "BUILD.bazel")
            if os.path.isfile(dep_gn) and not os.path.isfile(dep_bazel):
                if f"//{dep_pkg}" not in unmigrated_deps:
                    unmigrated_deps.append(f"//{dep_pkg}")
                try:
                    dep_gn_txt = open(dep_gn, encoding="utf-8").read()
                    if "declare_args(" in dep_gn_txt and f"//{dep_pkg}" not in declare_args_deps:
                        declare_args_deps.append(f"//{dep_pkg}")
                    if GN_ONLY_ATTR_RE.search(dep_gn_txt) or GN_BLOCKED_CONSTRUCT_RE.search(
                        dep_gn_txt
                    ):
                        gn_blocked_dep = True
                    else:
                        for sub_dm in re.finditer(
                            r'"//([^":(\s]+)(?::[^"(\s]+)?"', dep_gn_txt
                        ):
                            sub_pkg = sub_dm.group(1).strip("/")
                            sub_gn = os.path.join(workdir, sub_pkg, "BUILD.gn")
                            sub_bazel = os.path.join(workdir, sub_pkg, "BUILD.bazel")
                            if os.path.isfile(sub_gn) and not os.path.isfile(sub_bazel):
                                sub_txt = open(sub_gn, encoding="utf-8").read()
                                if GN_ONLY_ATTR_RE.search(
                                    sub_txt
                                ) or GN_BLOCKED_CONSTRUCT_RE.search(sub_txt):
                                    gn_blocked_dep = True
                                    break
                except Exception:
                    pass
    return unmigrated_deps, declare_args_deps, gn_blocked_dep


def has_skip_comment(lines, lineno):
    """Whether the 1-based line has a trailing `# @bazel2gn:skip` or the line above is one."""
    if 0 < lineno <= len(lines) and "#" in lines[lineno - 1]:
        if lines[lineno - 1].split("#", 1)[1].strip() == "@bazel2gn:skip":
            return True
    return lineno >= 2 and lines[lineno - 2].strip() == "# @bazel2gn:skip"


def find_closing_delim(text, open_idx, open_ch="{", close_ch="}"):
    depth, in_str, i, n = 0, False, open_idx, len(text)
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == "#":
            nl = text.find("\n", i)
            i = n if nl < 0 else nl
            continue
        elif c == open_ch:
            depth += 1
        elif c == close_ch:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return n


def find_closing_brace(text, open_idx):
    return find_closing_delim(text, open_idx, "{", "}")


def parse_gn_file(gn_path):
    try:
        text = open(gn_path, encoding="utf-8").read()
    except Exception:
        return None
    sentinel_re = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)
    m = sentinel_re.search(text)
    has_sentinel = bool(m)
    pre_text = text[: m.start()] if m else text
    post_text = text[m.end() :] if m else ""
    pre_targets = {}
    target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for tm in target_re.finditer(pre_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = pre_text.count("\n", 0, tm.start()) + 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(pre_text, brace_idx)
        body = pre_text[brace_idx + 1 : end_idx]
        pre_targets[name] = (tmpl, line_no, body)
    return {
        "has_sentinel": has_sentinel,
        "pre_text": pre_text,
        "post_text": post_text,
        "pre_targets": pre_targets,
    }


findings = []

for pkg_dir in sorted(candidate_dirs):
    bazel_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    gn_abs = os.path.join(workdir, pkg_dir, "BUILD.gn")
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")

    if not os.path.isfile(bazel_abs):
        continue

    bazel_targets = parse_bazel_targets(bazel_abs, pkg_dir)
    gn_info = parse_gn_file(gn_abs) if os.path.isfile(gn_abs) else None
    packaged_binaries = {
        t["binary_ref"]
        for t in bazel_targets
        if t["rule"] in ("fx_packaged_binary", "fx_unittest_component") and t.get("binary_ref")
    }
    for t in bazel_targets:
        if t["rule"] == "fx_unittest_package":
            packaged_binaries.update(t.get("unit_tests_refs", []))
    fx_all_pkgs = [
        t
        for t in bazel_targets
        if t["rule"] in ("fx_package", "fuchsia_package", "fx_unittest_package")
    ]
    # Non-test fx_package targets (excludes fx_unittest_package and test_components packages).
    fx_pkgs = [t for t in fx_all_pkgs if not t.get("is_test_pkg")]

    # Check whether a GN target above the sentinel is a check-cfg config() that bazel2gn
    # natively generates from rustc_check_cfg_build_flags().
    def is_convertible_check_cfg_gn_config(gname, gtmpl, gbody, bazel_rules_by_name):
        if gtmpl != "config":
            return False
        if bazel_rules_by_name.get(gname) == "rustc_check_cfg_build_flags":
            return True
        cleaned = re.sub(r"#[^\n]*", "", gbody)
        attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", cleaned))
        if not attrs or not (attrs <= {"rustflags", "visibility"}) or "rustflags" not in attrs:
            return False
        flags = []
        for m in re.finditer(r"\brustflags\s*\+?=\s*\[([^\]]*)\]", cleaned):
            flags.extend(re.findall(r'"((?:[^"\\]|\\.)*)"', m.group(1)))
        return bool(flags) and all(f.startswith("--check-cfg=cfg(") for f in flags)

    # Record non-blocking INFO alignment entries for rustc_check_cfg_build_flags targets so
    # review_diff does not flag the config() -> rustc_check_cfg_build_flags rule/rustflags mapping.
    def record_check_cfg_alignment_info(btargets, brel):
        for bt in btargets:
            if bt["rule"] != "rustc_check_cfg_build_flags" or bt["skip_line"] is not None:
                continue
            bname = bt["name"]
            bline = bt["line"]
            rule_msg = (
                f'Target "{bname}" (Rule & Directives): '
                "Unusual rule mapping from config to rustc_check_cfg_build_flags"
            )
            attr_msg = (
                f'Target "{bname}" (Target Attributes): '
                "rustflags in GN not carried over to Bazel"
            )
            rem = (
                "bazel2gn natively converts rustc_check_cfg_build_flags(cfgs, features) "
                "into a GN config() with --check-cfg rustflags."
            )
            for msg in (rule_msg, attr_msg):
                findings.append({
                    "source": "migration_sanity",
                    "category": "attribute_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": msg,
                    "remediation": rem,
                })

    record_check_cfg_alignment_info(bazel_targets, bazel_rel)

    # Record non-blocking INFO dep_alignment entries when `gn format` wraps a GN
    # dependency assignment onto the next line after `deps =` (e.g. inside `if (is_host)`)
    # and the full multi-line-expanded GN dependencies match the Bazel target's deps.
    def record_multiline_gn_dep_alignment_info(b_abs, brel, g_info):
        if not g_info or not g_info.get("post_text"):
            return
        try:
            b_src = open(b_abs, encoding="utf-8").read()
            b_tree = ast.parse(b_src, filename=b_abs)
        except Exception:
            return
        bz_vars = {}

        def eval_bz_list(node):
            if isinstance(node, ast.List):
                return [
                    e.value
                    for e in node.elts
                    if isinstance(e, ast.Constant) and isinstance(e.value, str)
                ]
            if isinstance(node, ast.Name):
                return list(bz_vars.get(node.id, ()))
            if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
                return eval_bz_list(node.left) + eval_bz_list(node.right)
            if (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Name)
                and node.func.id == "select"
                and node.args
                and isinstance(node.args[0], ast.Dict)
            ):
                out = []
                for v in node.args[0].values:
                    out.extend(eval_bz_list(v))
                return out
            return []

        bz_target_deps = {}
        for stmt in b_tree.body:
            if (
                isinstance(stmt, ast.Assign)
                and len(stmt.targets) == 1
                and isinstance(stmt.targets[0], ast.Name)
            ):
                bz_vars[stmt.targets[0].id] = eval_bz_list(stmt.value)
            elif (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
            ):
                call = stmt.value
                t_name = None
                deps_acc = set()
                for kw in call.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        t_name = kw.value.value
                    elif kw.arg in (
                        "deps",
                        "proc_macro_deps",
                        "implementation_deps",
                        "link_deps",
                        "public_deps",
                    ):
                        deps_acc.update(eval_bz_list(kw.value))
                if t_name:
                    bz_target_deps[t_name] = (stmt.lineno, deps_acc)

        gn_post = g_info["post_text"]
        gn_vars = {}

        def skip_gn_ws(txt, idx):
            n = len(txt)
            while idx < n:
                if txt[idx].isspace():
                    idx += 1
                elif txt[idx] == "#":
                    nl = txt.find("\n", idx)
                    idx = n if nl < 0 else nl + 1
                else:
                    break
            return idx

        def parse_gn_dep_rhs(txt, start_idx):
            items = []
            idx = skip_gn_ws(txt, start_idx)
            n = len(txt)
            while idx < n:
                if txt[idx] == "[":
                    end_b = find_closing_delim(txt, idx, "[", "]")
                    items.extend(re.findall(r'"((?:[^"\\]|\\.)*)"', txt[idx + 1 : end_b]))
                    idx = end_b + 1
                else:
                    id_m = re.match(r"[A-Za-z_]\w*", txt[idx:])
                    if not id_m:
                        break
                    items.extend(gn_vars.get(id_m.group(0), ()))
                    idx += id_m.end()
                idx = skip_gn_ws(txt, idx)
                if idx < n and txt[idx] == "+":
                    idx = skip_gn_ws(txt, idx + 1)
                else:
                    break
            return items

        # Collect GN variable definitions and conditional `+=` appends generated from
        # top-level Starlark list/select() variables in BUILD.bazel.
        for vm in re.finditer(r"(?m)^[ \t]*([A-Za-z_]\w*)[ \t]*(\+?=)[ \t]*", gn_post):
            var_name = vm.group(1)
            if var_name not in bz_vars:
                continue
            gn_vars.setdefault(var_name, []).extend(parse_gn_dep_rhs(gn_post, vm.end()))

        def gn_to_bz_label(gl):
            return re.sub(
                r"^//third_party/rust_crates:(.+)$",
                r"//third_party/rust_crates/vendor:\1",
                gl,
            )

        dep_attr_re = re.compile(
            r"(?m)^[ \t]*(deps|public_deps|proc_macro_deps|non_rust_deps|link_deps)[ \t]*\+?=[ \t]*"
        )
        target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
        for tm in target_re.finditer(gn_post):
            gname = tm.group(2)
            if gname not in bz_target_deps:
                continue
            bline, bz_deps = bz_target_deps[gname]
            brace_idx = tm.end() - 1
            end_idx = find_closing_brace(gn_post, brace_idx)
            gbody = gn_post[brace_idx + 1 : end_idx]
            full_gn = set()
            single_line_gn = set()
            has_wrapped = False
            for am in dep_attr_re.finditer(gbody):
                nl_pos = gbody.find("\n", am.end())
                same_line = gbody[am.end() : len(gbody) if nl_pos < 0 else nl_pos]
                same_line = same_line.split("#", 1)[0].strip()
                rhs_items = parse_gn_dep_rhs(gbody, am.end())
                full_gn.update(rhs_items)
                if same_line:
                    single_line_gn.update(rhs_items)
                else:
                    has_wrapped = True
            if not has_wrapped:
                continue
            full_mapped = {gn_to_bz_label(d) for d in full_gn}
            single_mapped = {gn_to_bz_label(d) for d in single_line_gn}
            if full_mapped == bz_deps and single_mapped < bz_deps:
                vendor_cnt = sum(
                    1 for d in single_line_gn if d.startswith("//third_party/rust_crates:")
                )
                unmatched_bz = sorted(bz_deps - single_mapped)
                parts = []
                if vendor_cnt > 0:
                    parts.append(f"{vendor_cnt} vendor crate mapping(s)")
                if unmatched_bz:
                    parts.append(f"unmatched in Bazel: [{', '.join(unmatched_bz)}]")
                msg = (
                    f'Target "{gname}" dependencies differ between GN and Bazel: '
                    + "; ".join(parts)
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "dep_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": msg,
                    "remediation": (
                        "Dependencies match between BUILD.bazel and BUILD.gn once "
                        "multi-line `gn format`-wrapped `deps =` expressions are expanded."
                    ),
                })

    record_multiline_gn_dep_alignment_info(bazel_abs, bazel_rel, gn_info)

    if gn_info and gn_info["has_sentinel"]:
        bazel_names = {t["name"] for t in bazel_targets}
        bazel_rules_by_name = {t["name"]: t["rule"] for t in bazel_targets}
        targets_by_name = {t["name"]: t for t in bazel_targets}
        for t in bazel_targets:
            tname = t["name"]
            rule = t["rule"]
            is_test_bin = rule in ("fx_cc_binary", "cc_binary") and t.get("testonly")
            attention = [(f"`{a}` on `{rule}(name = \"{tname}\")`", ln) for a, ln in t.get("attention_test_attrs", [])]
            if t["skip_line"] is not None and t.get("has_attention_marker") and (rule in BAZEL_TEST_RULES or is_test_bin):
                attention.append((f"`{rule}(name = \"{tname}\")`", t["skip_line"]))
            for what, ln in attention:
                findings.append({
                    "source": "migration_sanity",
                    "category": "test_skip_needs_reviewer_attention",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": ln,
                    "message": (
                        f"{what} in '{bazel_rel}' skips dual-running its test in GN because the coder judged that "
                        "dual-testing cost outweighs the risk (marked `# Needs reviewer attention (bazel2gn skip): ...`)."
                    ),
                    "remediation": (
                        "A human reviewer must confirm this exception (see the cost criteria in "
                        "docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems) and the CL "
                        "description must list it under \"Needs reviewer attention\". Do not remove the skip just "
                        "to clear this finding."
                    ),
                })
            if t["skip_line"] is None:
                for attr_name, attr_skip_line, attr_reason in t.get("skipped_test_attrs", []):
                    if t.get("has_test_data") or attr_reason:
                        continue
                    findings.append({
                        "source": "migration_sanity",
                        "category": "unjustified_test_skip",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": attr_skip_line,
                        "message": (
                            f"Test attribute `{attr_name}` on `{rule}(name = \"{tname}\")` in '{bazel_rel}' is annotated "
                            "with `# @bazel2gn:skip`, which prevents bazel2gn from generating the GN unit test while "
                            f"'{gn_rel}' still dual-builds `{tname}`."
                        ),
                        "remediation": (
                            f"Remove `# @bazel2gn:skip` from `{attr_name}` on `{tname}` in '{bazel_rel}', re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire `:{tname}_test` into `group(\"tests\")` in '{gn_rel}' "
                            "(see \"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md)."
                        ),
                    })
            is_test_bin = rule in ("fx_cc_binary", "cc_binary") and t.get("testonly")
            if t["skip_line"] is not None and (rule in BAZEL_TEST_RULES or is_test_bin):
                tut_refs = [t["crate_ref"]] if t.get("crate_ref") else t.get("local_test_refs", [])
                local_refs = [r for r in tut_refs if r in targets_by_name]
                t1_tut_skipped = bool(local_refs) and all(
                    targets_by_name[r]["skip_line"] is not None for r in local_refs
                )
                t2_fx_unittest_resources = rule == "fx_unittest_package" and t.get("has_resources")
                t3_host_test_data = rule in ("host_go_test", "go_test", "host_rustc_test") and t.get("has_test_data")
                t4_unsupported_rule = rule in UNSUPPORTED_TEST_BINARY_RULES or (
                    rule == "rustc_test"
                    and (
                        t.get("cross_pkg_srcs_or_crate")
                        or (
                            t.get("has_rustc_env")
                            and t.get("crate_ref") in targets_by_name
                            and targets_by_name[t["crate_ref"]].get("has_rustc_env")
                        )
                    )
                )
                t5_owner_reason = t.get("has_skip_reason")
                if not (t1_tut_skipped or t2_fx_unittest_resources or t3_host_test_data or t4_unsupported_rule or t5_owner_reason):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "unjustified_test_skip",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": t["skip_line"],
                        "message": (
                            f"Test target '{tname}' ({rule}) in '{bazel_rel}' is annotated with `# @bazel2gn:skip` in a "
                            "dual-build package without a technical skip condition or explanatory comment (target under "
                            "test is not skipped, and bazel2gn supports translating this test target)."
                        ),
                        "remediation": (
                            f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in '{bazel_rel}', delete any "
                            f"duplicate hand-written GN definition above `## BAZEL2GN SENTINEL` in '{gn_rel}', re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire the generated GN test into `group(\"tests\")` "
                            "(see \"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md)."
                        ),
                    })
                    continue
            in_pre_gn = (
                tname in gn_info["pre_targets"]
                and (
                    gn_info["pre_targets"][tname][0] in CONVERTIBLE_GN_TEMPLATES
                    or is_convertible_check_cfg_gn_config(
                        tname,
                        gn_info["pre_targets"][tname][0],
                        gn_info["pre_targets"][tname][2],
                        bazel_rules_by_name,
                    )
                )
            )
            ref_in_pre_gn = rule in CONVERTIBLE_BAZEL_RULES and any(
                ptmpl not in ("bazel_test_suite", "bazel_host_tool")
                and bool(
                    re.search(
                        r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(tname) + r'(?:_[A-Za-z0-9_]+)?(?:"|\()',
                        pbody,
                    )
                )
                for ptmpl, _, pbody in gn_info["pre_targets"].values()
            )
            if tname in packaged_binaries and not in_pre_gn and not ref_in_pre_gn:
                continue
            if rule in BAZEL_TEST_RULES or is_test_bin:
                continue
            if is_bazel_only_private_test_dep(
                t, bazel_targets, gn_info, pkg_dir, packaged_binaries
            ):
                continue
            if tname in gn_info["pre_targets"] and (
                gn_info["pre_targets"][tname][0] not in CONVERTIBLE_GN_TEMPLATES
                or GN_ONLY_ATTR_RE.search(gn_info["pre_targets"][tname][2])
            ):
                continue  # bazel2gn cannot emit its GN-only template or attributes (see gn_attr_parity).
            if t["skip_line"] is not None and (
                rule in CONVERTIBLE_BAZEL_RULES or in_pre_gn or ref_in_pre_gn
            ):
                if rule == "rustc_check_cfg_build_flags":
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with "
                        "`# @bazel2gn:skip`"
                        + (
                            f" and duplicated as `config(\"{tname}\")` above the "
                            f"BAZEL2GN SENTINEL in '{gn_rel}'"
                            if in_pre_gn
                            else ""
                        )
                        + ". `bazel2gn` natively converts `rustc_check_cfg_build_flags` into "
                        "a GN `config()` below the sentinel, so it must not be skipped."
                    )
                    skip_rem = (
                        f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in "
                        f"'{bazel_rel}', delete any manual `config(\"{tname}\")` definition "
                        f"above `## BAZEL2GN SENTINEL` in '{gn_rel}', and re-run "
                        f"`fx bazel2gn -d {pkg_dir}`."
                    )
                else:
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with `# @bazel2gn:skip`"
                        + (f" and duplicated as `{gn_info['pre_targets'][tname][0]}(\"{tname}\")` above the BAZEL2GN SENTINEL in '{gn_rel}'" if in_pre_gn else "")
                        + (f" while referenced by `:{tname}` in '{gn_rel}'" if ref_in_pre_gn else "")
                        + ". Convertible libraries and reference/bitrot-prevention targets must not be skipped."
                    )
                    skip_rem = (
                        f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in '{bazel_rel}', "
                        f"delete any manual `{tname}` target definition above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                        "(keeping only helper `config(...)` or test package definitions above the sentinel), "
                        "use `# @bazel2gn:raw_overwrite:[ \"<gn_config_1>\", ... ]` on the closing `],` of `copts = [...]` "
                        "if C/C++ compiler flags need to map to GN `configs`, and re-run `fx bazel2gn -d "
                        + pkg_dir
                        + "`."
                    )
                findings.append({
                    "source": "migration_sanity",
                    "category": "skipped_convertible_target",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": t["skip_line"],
                    "message": skip_msg,
                    "remediation": skip_rem,
                })

        rust_unit_test_names = {t["name"] + "_test" for t in bazel_targets if t.get("unit_tests")}
        for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
            # Reject hand-written check-cfg GN config() targets above the sentinel when
            # bazel2gn natively converts rustc_check_cfg_build_flags() below the sentinel.
            if is_convertible_check_cfg_gn_config(gname, gtmpl, gbody, bazel_rules_by_name):
                findings.append({
                    "source": "migration_sanity",
                    "category": "duplicate_target_above_sentinel",
                    "severity": "error",
                    "file": gn_rel,
                    "line": gline,
                    "message": (
                        f"Convertible GN `config(\"{gname}\")` with `--check-cfg=cfg(...)` "
                        f"rustflags remains above `## BAZEL2GN SENTINEL` in '{gn_rel}'"
                        + (
                            f" (duplicating `rustc_check_cfg_build_flags(name = \"{gname}\")` "
                            f"in '{bazel_rel}')"
                            if gname in bazel_names
                            else ""
                        )
                        + ". `bazel2gn` natively converts `rustc_check_cfg_build_flags` into "
                        "a GN `config()` below the sentinel."
                    ),
                    "remediation": (
                        f"Define `rustc_check_cfg_build_flags(name = \"{gname}\", cfgs = [...], "
                        f"features = [...])` in '{bazel_rel}' without `# @bazel2gn:skip` "
                        "(loaded from `//build/bazel/rules/rust:rustc_check_cfg_build_flags.bzl`), "
                        f"delete `config(\"{gname}\")` from above `## BAZEL2GN SENTINEL` in "
                        f"'{gn_rel}', and run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })
                continue
            if gtmpl in GN_TEST_TEMPLATES:
                out_m = re.search(r'\boutput_name\s*=\s*"([^"]+)"', gbody)
                bazel_dups = sorted(
                    n
                    for n in {gname, out_m.group(1) if out_m else gname}
                    if n in bazel_names or n in rust_unit_test_names
                )
                if bazel_dups:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "duplicate_test_above_sentinel",
                        "severity": "error",
                        "file": gn_rel,
                        "line": gline,
                        "message": (
                            f"GN test `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' duplicates the Bazel test "
                            + ", ".join(f"`:{n}`" for n in bazel_dups)
                            + f" of '{bazel_rel}', and bazel2gn generates the GN copy of that test below the sentinel."
                        ),
                        "remediation": (
                            f"Delete `{gtmpl}(\"{gname}\")` from '{gn_rel}', re-run `fx bazel2gn -d {pkg_dir}`, list the generated GN "
                            "test in `group(\"tests\")` and export the Bazel test through a hand-written `bazel_test_suite` (see "
                            "\"Testing libraries in both build systems\" in docs/development/build/bazel_concepts/tests.md). If the "
                            "Bazel test cannot pass without a source change, remove the Bazel test (and its `with_unit_tests`/"
                            "`test_deps`) and keep this GN test."
                        ),
                    })
                else:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "gn_test_kept_above_sentinel",
                        "severity": "INFO",
                        "file": gn_rel,
                        "line": gline,
                        "message": (
                            f"GN test `{gtmpl}(\"{gname}\")` stays hand-written above `## BAZEL2GN SENTINEL` in '{gn_rel}'. "
                            "This is allowed while "
                            "Bazel cannot express or pass the test without a source change (e.g. GN-only test_specs, or test code "
                            "reading files at a path relative to the GN build directory, which a Bazel host test's runtime "
                            "directory cannot reproduce)."
                        ),
                        "remediation": (
                            "If Bazel can run it unchanged, move it to Bazel (`with_unit_tests`/`with_host_unit_tests` on the "
                            "`rustc_*` target, or `rustc_test`/`go_test`, without `# @bazel2gn:skip`), delete it here, re-run "
                            f"`fx bazel2gn -d {pkg_dir}` so the GN copy is generated, list that GN copy in `group(\"tests\")` and "
                            "export the Bazel test with a `bazel_test_suite`. Otherwise keep it with its GN deps and output name, depending on "
                            "the generated library, and name the blocker in the summary and commit message."
                        ),
                    })
                continue
            if gtmpl in CONVERTIBLE_GN_TEMPLATES and not GN_ONLY_ATTR_RE.search(gbody):
                if is_packaged_testonly_gn_binary(
                    gname, gtmpl, gbody, gn_info["pre_targets"], bazel_names
                ):
                    continue
                unmigrated_deps, declare_args_deps, gn_blocked_dep = inspect_gn_target_deps(
                    gbody, pkg_dir, gn_info["pre_targets"], bazel_names
                )
                if (
                    gname not in bazel_names
                    and gn_blocked_dep
                    and any(
                        t["rule"] in CONVERTIBLE_BAZEL_RULES and t["skip_line"] is None
                        for t in bazel_targets
                    )
                ):
                    continue

                attr_hints = []
                if gtmpl == "rustc_binary":
                    out_m = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', gbody)
                    if out_m and out_m.group(1) != gname:
                        attr_hints.append(
                            f"set `crate_name = \"{out_m.group(1)}\"` on `rustc_binary(name = \"{gname}\")` in '{bazel_rel}' "
                            f"(bazel2gn maps `crate_name` on `rustc_binary` to `output_name = \"{out_m.group(1)}\"` in BUILD.gn; "
                            "omit dummy `version` strings)"
                        )
                    src_m = re.search(r'\bsource_root\s*=\s*"([^"]+)"', gbody)
                    if src_m and src_m.group(1) != "src/main.rs":
                        attr_hints.append(f"set `crate_root = \"{src_m.group(1)}\"`")
                if gtmpl.startswith("rustc_"):
                    cfg_m = re.search(r'\bconfigs\s*\+=\s*\[\s*"([^"]+)"\s*\]', gbody)
                    if cfg_m:
                        attr_hints.append(f"map `configs += [ \"{cfg_m.group(1)}\" ]` to `lint_config = \"{cfg_m.group(1)}\"`")

                dep_msg = (
                    f" Unmigrated first-party dependencies referenced by `{gname}` that also need a BUILD.bazel in this change: "
                    + ", ".join(unmigrated_deps)
                    + "."
                    if unmigrated_deps
                    else ""
                )
                dep_rem = ""
                if unmigrated_deps:
                    dep_rem = (
                        " First migrate unmigrated dependency package(s) "
                        + ", ".join(unmigrated_deps)
                        + " (create their BUILD.bazel and sync with `fx bazel2gn -d <dep_dir>`)."
                    )
                    if declare_args_deps:
                        dep_rem += (
                            " For `declare_args()` in "
                            + ", ".join(declare_args_deps)
                            + ", keep `declare_args() { ... }` in a `.gni` file (imported above `## BAZEL2GN SENTINEL` in its BUILD.gn), "
                            "export `<arg>` via `generated_file(\"gn_build_variables_for_bazel\")` in `//build/bazel/BUILD.gn` with "
                            "`# LINT.IfChange` / `# LINT.ThenChange(...)` on both sides, `load(\"@fuchsia_build_info//:args.bzl\", \"<arg>\")` "
                            "in its BUILD.bazel, and translate `if (<arg>) { features = [ ... ] }` as `crate_features = [\"...\"] if <arg> else []`."
                        )
                hint_rem = (" In '" + bazel_rel + "', " + "; ".join(attr_hints) + ".") if attr_hints else ""

                findings.append({
                    "source": "migration_sanity",
                    "category": "duplicate_target_above_sentinel",
                    "severity": "error",
                    "file": gn_rel,
                    "line": gline,
                    "message": (
                        f"Convertible GN target `{gtmpl}(\"{gname}\")` remains above `## BAZEL2GN SENTINEL` "
                        f"in '{gn_rel}'"
                        + (" (duplicating the target in BUILD.bazel)" if gname in bazel_names else "")
                        + ". All convertible library/binary targets (including benchmark binaries and reference/bitrot-prevention libraries) "
                        "must be migrated to BUILD.bazel and generated below the sentinel by `fx bazel2gn`."
                        + dep_msg
                    ),
                    "remediation": (
                        f"Define `{gname}` in '{bazel_rel}' without `# @bazel2gn:skip`, delete `{gtmpl}(\"{gname}\")` "
                        f"from above `## BAZEL2GN SENTINEL` in '{gn_rel}', and run `fx bazel2gn -d {pkg_dir}`."
                        + dep_rem
                        + hint_rem
                    ),
                })

    gn_kept_pkg_test_deps = set()
    if gn_info and FX_TEST_AVAILABLE:
        bazel_test_pkg_names = {t["package_name"] for t in fx_all_pkgs}
        bazel_rules = {t["name"]: t["rule"] for t in bazel_targets}
        bazel_untranslatable_test_pkg_names = {
            t["package_name"]
            for t in bazel_targets
            if t["rule"] in ("fx_package", "fuchsia_package")
            or (
                t["rule"] == "fx_unittest_package"
                and (
                    t.get("has_resources")
                    or any(
                        bazel_rules.get(u) in UNSUPPORTED_TEST_BINARY_RULES
                        for u in t.get("unit_tests_refs", [])
                    )
                )
            )
        }
        bazel_host_only = {t["name"] for t in bazel_targets if t.get("host_only")}
        rust_unit_tests = {t["name"] + "_test" for t in bazel_targets if t["unit_tests"] and not t.get("host_only")}
        third_party_gn_labels = {
            "//third_party/googletest",
            "//third_party/googletest:gtest",
            "//third_party/googletest:gtest_main",
            "//third_party/googletest:gtest_prod",
            "//third_party/googletest:gmock",
            "//third_party/googletest:gmock_main",
        }
        try:
            with open(os.path.join(workdir, "build/tools/bazel2gn/third_party_target_map.json"), encoding="utf-8") as f:
                third_party_gn_labels.update(json.load(f).values())
        except Exception:
            pass

        def bazel_buildable(label):
            """Whether a GN dep label names a target with a Bazel build (conservative)."""
            if "(" in label or "$" in label:
                return False
            if label.startswith(":"):
                return label[1:] in bazel_rules
            if not label.startswith("//"):
                return False
            if label in third_party_gn_labels:
                return True
            path, _, name = label[2:].partition(":")
            try:
                with open(os.path.join(workdir, path, "BUILD.bazel"), encoding="utf-8") as f:
                    text = f.read()
            except OSError:
                return False
            return re.search(r'\bname\s*=\s*"' + re.escape(name or os.path.basename(path)) + '"', text) is not None

        def test_language(label):
            """Returns "rust", "cc" (a C/C++ test Bazel can build), or None if unknown/unbuildable."""
            if not label.startswith(":") or "(" in label:
                return None
            name = label[1:]
            if name in rust_unit_tests:
                return "rust"
            if name.endswith("_test"):
                base_n = name[: -len("_test")]
                if bazel_rules.get(base_n, "") in RUST_UNIT_TEST_OWNERS:
                    return None if base_n in bazel_host_only else "rust"
            rule = bazel_rules.get(name)
            if rule:
                if name in bazel_host_only:
                    return None
                if rule.startswith(("rust", "rustc")):
                    return "rust"
                return "cc" if rule in CC_TEST_BAZEL_RULES else None
            if name not in gn_info["pre_targets"]:
                return None
            ttmpl, _, tbody = gn_info["pre_targets"][name]
            tbody = re.sub(r"#[^\n]*", "", tbody)
            if ttmpl.startswith("rustc_"):
                return "rust"
            if ttmpl not in GN_CC_TEST_TEMPLATES or "$" in tbody or re.search(r"\b(?:if|foreach)\s*\(", tbody):
                return None
            srcs = []
            for sm in re.finditer(r"\bsources\s*\+?=\s*\[([^\]]*)\]", tbody):
                srcs += re.findall(r'"([^"]+)"', sm.group(1))
            if any(f.endswith(".rs") for f in srcs):
                return "rust"
            if not srcs or not all(f.endswith(CC_SOURCE_SUFFIXES) for f in srcs):
                return None
            deps = []
            for dm in re.finditer(r"\b(?:public_deps|deps)\s*\+?=\s*\[([^\]]*)\]", tbody):
                deps += re.findall(r'"([^"]+)"', dm.group(1))
            return "cc" if all(bazel_buildable(d) for d in deps) else None

        def depends_on_rustc_dylib(seed_texts):
            """Whether any direct or transitive first-party dep defines a rustc_dylib / rust_dylib_library."""
            dylib_re = re.compile(r"\b(?:rustc_dylib|rust_dylib_library)\s*\(")
            dep_pkg_re = re.compile(r'"//([^":(\s]+)(?::([^"(\s]+))?"')
            vis_re = re.compile(r"\b(?:default_)?visibility\s*\+?=\s*\[[^\]]*\]")
            trans_strip_re = re.compile(
                r"\b(?:(?:default_)?visibility|[A-Za-z0-9_]*test_deps|[A-Z0-9_]*TEST_DEPS)\s*\+?=\s*\[[^\]]*\]"
            )
            queue = []
            seen = {pkg_dir}
            for raw in seed_texts:
                txt = vis_re.sub("", re.sub(r"#[^\n]*", "", raw))
                if dylib_re.search(txt):
                    return True
                for m in dep_pkg_re.finditer(txt):
                    p, t = m.group(1).strip("/"), m.group(2) or ""
                    if (
                        p
                        and t not in ("__pkg__", "__subpackages__", "*")
                        and not p.startswith(("build/", "prebuilt/", "third_party/", "out/"))
                        and p not in seen
                    ):
                        seen.add(p)
                        queue.append(p)
            visited = 0
            while queue and visited < 256:
                cur = queue.pop(0)
                visited += 1
                for fn in ("BUILD.bazel", "BUILD.gn"):
                    fp = os.path.join(workdir, cur, fn)
                    if not os.path.isfile(fp):
                        continue
                    try:
                        txt = trans_strip_re.sub("", re.sub(r"#[^\n]*", "", open(fp, encoding="utf-8").read()))
                    except OSError:
                        continue
                    if dylib_re.search(txt):
                        return True
                    for m in dep_pkg_re.finditer(txt):
                        p, t = m.group(1).strip("/"), m.group(2) or ""
                        if (
                            p
                            and t not in ("__pkg__", "__subpackages__", "*")
                            and not p.startswith(("build/", "prebuilt/", "third_party/", "out/"))
                            and p not in seen
                        ):
                            seen.add(p)
                            queue.append(p)
            return False

        for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
            if gtmpl not in GN_TEST_PACKAGE_TEMPLATES:
                continue
            gbody = re.sub(r"#[^\n]*", "", gbody)
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"]+)"', gbody)
            gn_pkg_name = pn_m.group(1) if pn_m else gname
            if gn_pkg_name in bazel_test_pkg_names:
                findings.append({
                    "source": "migration_sanity",
                    "category": "duplicate_gn_test_package",
                    "severity": "error",
                    "file": gn_rel,
                    "line": gline,
                    "message": (
                        f"GN `{gtmpl}(\"{gname}\")` in '{gn_rel}' builds package `{gn_pkg_name}`, which "
                        f"`fx_package` in '{bazel_rel}' also builds; both would publish the same test package."
                    ),
                    "remediation": (
                        f"Delete `{gtmpl}(\"{gname}\")` (and test executables only it used) from '{gn_rel}' and "
                        f"export the Bazel `fx_test` with `bazel_test_suite(\"{gname}\") {{ target_tests = [ ... ] }}` "
                        "listed in `group(\"tests\")`."
                    ),
                })
                continue
            if gn_pkg_name.endswith("-gn") and gn_pkg_name[: -len("-gn")] in bazel_test_pkg_names:
                base_pkg_name = gn_pkg_name[: -len("-gn")]
                if base_pkg_name in bazel_untranslatable_test_pkg_names:
                    # The GN copy of a Bazel test package that bazel2gn can't translate (explicit
                    # fx_package chain, fx_unittest_package with resources, or unsupported test binary).
                    continue
                if gn_info["has_sentinel"]:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "duplicate_test_above_sentinel",
                        "severity": "error",
                        "file": gn_rel,
                        "line": gline,
                        "message": (
                            f"GN `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' duplicates "
                            f"`fx_unittest_package` `{base_pkg_name}` in '{bazel_rel}', which has no `resources` "
                            "and is translated directly by `bazel2gn` below the sentinel."
                        ),
                        "remediation": (
                            f"Delete `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}', remove any "
                            f"`# @bazel2gn:skip` on `fx_unittest_package(name = \"{base_pkg_name}\")` in '{bazel_rel}', "
                            f"and re-run `fx bazel2gn -d {pkg_dir}`."
                        ),
                    })
                    continue
            blockers = [k for k in GN_TEST_PACKAGE_BLOCKERS if re.search(r"\b" + k + r"\s*\+?=", gbody)]
            if "$" in gbody or re.search(r"\b(?:if|foreach)\s*\(", gbody):
                blockers.append("variables/conditionals")
            if re.search(r"\b(?:deps|test_components)\s*\+?=\s*[A-Za-z_]", gbody):
                blockers.append("deps from a variable")
            ts_m = re.search(r"\btest_specs\s*=\s*\{", gbody)
            if ts_m:
                specs = gbody[ts_m.end() : find_closing_brace(gbody, ts_m.end() - 1)]
                log_m = re.search(r"\blog_settings\s*=\s*\{([^{}]*)\}", specs)
                flat = re.sub(r"\{[^{}]*\}", "{}", specs)
                keys = set(re.findall(r"\b([A-Za-z_]\w*)\s*=", flat))
                if keys - {"log_settings"} or (
                    log_m and set(re.findall(r"\b([A-Za-z_]\w*)\s*=", log_m.group(1))) - {"max_severity"}
                ):
                    blockers.append("test_specs")
            labels = []
            for lm in re.finditer(r"\b(?:deps|test_components)\s*\+?=\s*\[([^\]]*)\]", gbody):
                labels += re.findall(r'"([^"]+)"', lm.group(1))
            pkg_test_deps = set()
            seed_texts = [gbody, open(bazel_abs, encoding="utf-8").read()]
            for l in labels:
                local = l[1:] if l.startswith(":") and "(" not in l else ""
                if local:
                    pkg_test_deps.add(local)
                    comp = gn_info["pre_targets"].get(local)
                    if comp and comp[0] in ("fuchsia_test_component", "fuchsia_unittest_component", "fuchsia_component"):
                        for cm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", comp[2]):
                            for cl in re.findall(r'"([^"]+)"', cm.group(1)):
                                if cl.startswith(":") and "(" not in cl:
                                    pkg_test_deps.add(cl[1:])
                owner = gn_info["pre_targets"].get(local) or gn_info["pre_targets"].get(
                    re.sub(r"_test$", "", local)
                )
                if owner:
                    seed_texts.append(owner[2])
                    if GN_ONLY_ATTR_RE.search(owner[2]):
                        blockers.append("GN-only attributes (see gn_attr_parity)")
                    _, _, owner_blocked = inspect_gn_target_deps(
                        owner[2],
                        pkg_dir,
                        gn_info["pre_targets"],
                        {t["name"] for t in bazel_targets},
                    )
                    if owner_blocked:
                        blockers.append("GN-blocked dependency")
            # Unknown or unbuildable tests never trigger it.
            langs = {test_language(l) for l in labels}
            if "rust" in langs and depends_on_rustc_dylib(seed_texts):
                blockers.append("rustc_dylib dependency (duplicate default_runfiles .so in fx_packaged_binary)")
            if blockers or not labels or not langs <= set(MIGRATABLE_TEST_LANGUAGES):
                gn_kept_pkg_test_deps.update(pkg_test_deps)
                continue
            is_rust = "rust" in langs
            findings.append({
                "source": "migration_sanity",
                "category": "gn_test_package_not_migrated",
                "severity": "error",
                "file": gn_rel,
                "line": gline,
                "message": (
                    f"GN `{gtmpl}(\"{gname}\")` in '{gn_rel}' only packages tests Bazel can build "
                    f"({', '.join(labels)}) and uses nothing Bazel device tests cannot express, but it stays "
                    "in GN although this checkout has `//build/bazel/rules/testing:fx_test.bzl`."
                ),
                "remediation": (
                    f"Migrate it as in \"Migrating Tests\" of the coder instructions: in '{bazel_rel}' declare "
                    + (
                        "an explicit `rustc_test(...)`, "
                        if is_rust
                        else "`fx_cc_binary(testonly = True, tags = [\"manual\"])` for the test executable, "
                    )
                    + f"and package it with `fx_unittest_package(name = \"{gname}\", "
                    + (f"package_name = \"{gn_pkg_name}\", " if gn_pkg_name != gname else "")
                    + "unit_tests = [\":<test_bin>\"])` "
                    f"(bazel2gn generates the GN copy as package `{gn_pkg_name}-gn`), delete the GN package and "
                    "executable, and re-run `fx bazel2gn` "
                    "(or `fx_unittest_component` + `fx_package` + `fx_test` when packaging extra `resource()`s/components; "
                    "only unroll into `fx_packaged_binary` + `fx_component_manifest` + `fx_test_component` + `fx_package` + `fx_test` "
                    "with a `meta/<component>.cml` when the test requires a custom `.cml` manifest or non-default capability shards, in "
                    f"which case keep this GN package above the sentinel as the GN copy with `package_name = \"{gn_pkg_name}-gn\"` and its "
                    "executable replaced by the generated test binary while the library has GN users). "
                    "Export the Bazel test with a hand-written "
                    f"`bazel_test_suite(\"bazel_tests\") {{ target_tests = [ \"//{pkg_dir}:{gname}\" ] }}` and list it and "
                    "the GN copy in `group(\"tests\")`. If a real blocker keeps it in GN only, name it in the summary and "
                    "dispute this finding."
                ),
            })

    if gn_info and gn_info["has_sentinel"]:
        # A package-private testonly helper that only skipped (Bazel-only) tests use would
        # otherwise be generated into BUILD.gn with no GN consumer.
        for t in bazel_targets:
            if (
                t["rule"] in CONVERTIBLE_BAZEL_RULES
                and t["skip_line"] is None
                and is_bazel_only_private_test_dep(
                    t, bazel_targets, gn_info, pkg_dir, packaged_binaries, gn_kept_pkg_test_deps
                )
            ):
                findings.append({
                    "source": "migration_sanity",
                    "category": "orphaned_test_helper",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": t["line"],
                    "message": (
                        f"Package-private test-only target `{t['rule']}(name = \"{t['name']}\")` in '{bazel_rel}' "
                        "is only consumed by Bazel tests in the same package that have `# @bazel2gn:skip`, but has no "
                        f"`# @bazel2gn:skip` itself, so bazel2gn generates an orphaned GN target in '{gn_rel}'."
                    ),
                    "remediation": (
                        f"Put `# @bazel2gn:skip` directly above `{t['rule']}(name = \"{t['name']}\")` in "
                        f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })

    if fx_pkgs:
        fx_pkg_name = fx_pkgs[0]["name"]
        bazel_text_raw = open(bazel_abs, encoding="utf-8").read()
        migrated_pkg_names = {t["name"] for t in bazel_targets} | {t["package_name"] for t in fx_pkgs}
        migrated_manifests = set(re.findall(r'\bmanifest\s*=\s*"([^"]+)"', bazel_text_raw))
        has_b2gn_convertible = any(
            t["rule"] in CONVERTIBLE_BAZEL_RULES and t["name"] not in packaged_binaries and t["skip_line"] is None
            for t in bazel_targets
        )
        if gn_info and gn_info["has_sentinel"] and not has_b2gn_convertible:
            findings.append({
                "source": "migration_sanity",
                "category": "unnecessary_bazel2gn_on_package_only_bazel",
                "severity": "error",
                "file": gn_rel,
                "line": 1,
                "message": (
                    f"'{bazel_rel}' only defines `fx_package` and its component/manifest/binary/resource targets, "
                    f"which `bazel2gn` does not convert, but '{gn_rel}' has `## BAZEL2GN SENTINEL`."
                ),
                "remediation": (
                    f"Remove `## BAZEL2GN SENTINEL` from '{gn_rel}', leaving only the GN-only test/library targets in "
                    f"'{gn_rel}' (or delete '{gn_rel}' if no GN targets remain)."
                ),
            })

        if gn_info:
            gn_pkg_TMPLS = ("fuchsia_package", "fuchsia_package_with_single_component", "fuchsia_component")
            dup_gn_pkg_names = set()
            for gname, (gtmpl, gline, gbody) in gn_info["pre_targets"].items():
                if gtmpl not in gn_pkg_TMPLS:
                    continue
                gn_p_m = re.search(r'\b(?:package_name|component_name)\s*=\s*"([^"]+)"', gbody)
                gn_m_m = re.search(r'\bmanifest\s*=\s*"([^"]+)"', gbody)
                if (
                    gname in migrated_pkg_names
                    or (gn_p_m and gn_p_m.group(1) in migrated_pkg_names)
                    or (gn_m_m and gn_m_m.group(1) in migrated_manifests)
                ):
                    dup_gn_pkg_names.add(gname)

            remaining_non_pkg = [
                (gn_n, gn_t)
                for gn_n, (gn_t, _, gn_b) in gn_info["pre_targets"].items()
                if gn_n not in dup_gn_pkg_names
                and gn_t != "resource"
                and not (
                    gn_t in ("executable", "rustc_binary")
                    and not re.search(r"\btestonly\s*=\s*true\b", gn_b)
                    and (gn_n in packaged_binaries or any(t["name"] == gn_n for t in bazel_targets))
                )
            ]
            for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
                is_dup_pkg = gname in dup_gn_pkg_names
                is_dup_bin = (
                    not gn_info["has_sentinel"]
                    and gtmpl in ("executable", "rustc_binary", "resource")
                    and not re.search(r"\btestonly\s*=\s*true\b", gbody)
                    and (
                        gname in packaged_binaries
                        or any(t["name"] == gname for t in bazel_targets)
                        or any(
                            dup_n in gn_info["pre_targets"]
                            and re.search(r'":' + re.escape(gname) + r'(?:"|\()', gn_info["pre_targets"][dup_n][2])
                            for dup_n in dup_gn_pkg_names
                        )
                    )
                    and not any(
                        other_n != gname
                        and other_n not in dup_gn_pkg_names
                        and re.search(r'":' + re.escape(gname) + r'(?:"|\()', other_b)
                        for other_n, (other_t, _, other_b) in gn_info["pre_targets"].items()
                    )
                )
                if is_dup_pkg or is_dup_bin:
                    rem = (
                        f"Delete '{gn_rel}' (`git rm {gn_rel}`) now that all targets in '//{pkg_dir}' are migrated to '{bazel_rel}'."
                        if not remaining_non_pkg and not gn_info["has_sentinel"]
                        else (
                            f"Delete `{gtmpl}(\"{gname}\")` (and any dedicated package component/binary/resource targets "
                            f"not used by remaining GN test targets) from '{gn_rel}', keeping only the GN-only test or "
                            f"library targets in '{gn_rel}'."
                        )
                    )
                    findings.append({
                        "source": "migration_sanity",
                        "category": "duplicate_gn_fuchsia_package",
                        "severity": "error",
                        "file": gn_rel,
                        "line": gline,
                        "message": (
                            f"GN target `{gtmpl}(\"{gname}\")` remains in '{gn_rel}' after migrating the package "
                            f"to `fx_package(name = \"{fx_pkg_name}\")` in '{bazel_rel}'."
                        ),
                        "remediation": rem,
                    })

        bridge_dir_rel = f"bundles/assembly/bazel_inputs/{pkg_dir}"
        bridge_gn_rel = f"{bridge_dir_rel}/BUILD.gn"
        bridge_bazel_rel = f"{bridge_dir_rel}/BUILD.bazel"
        assembly_bazel_rel = "bundles/assembly/BUILD.bazel"
        assembly_inputs_gn_rel = "bundles/assembly/bazel_inputs/BUILD.gn"
        has_bridge_files = os.path.isfile(os.path.join(workdir, bridge_gn_rel)) or os.path.isfile(
            os.path.join(workdir, bridge_bazel_rel)
        )
        in_assembly_bazel = False
        if os.path.isfile(os.path.join(workdir, assembly_bazel_rel)):
            try:
                in_assembly_bazel = f"//{bridge_dir_rel}" in open(
                    os.path.join(workdir, assembly_bazel_rel), encoding="utf-8"
                ).read()
            except Exception:
                pass
        in_assembly_inputs_gn = False
        if os.path.isfile(os.path.join(workdir, assembly_inputs_gn_rel)):
            try:
                in_assembly_inputs_gn = f"//{bridge_dir_rel}" in open(
                    os.path.join(workdir, assembly_inputs_gn_rel), encoding="utf-8"
                ).read()
            except Exception:
                pass
        # Verify whether an assembly bazel_inputs bridge points to a GN package target
        # that remains unmigrated in BUILD.gn (e.g. a componentless shell-tool package).
        def is_bridged_gn_package_kept(b_gn_rel, p_dir, g_info, dup_names):
            if not g_info or not os.path.isfile(os.path.join(workdir, b_gn_rel)):
                return False
            try:
                b_txt = open(os.path.join(workdir, b_gn_rel), encoding="utf-8").read()
            except Exception:
                return False
            bm = re.search(
                r'\bpackage\s*=\s*"//' + re.escape(p_dir) + r'(?::([^"]+))?"', b_txt
            )
            if not bm:
                return False
            bridged_t = bm.group(1) or os.path.basename(p_dir)
            return bridged_t in g_info["pre_targets"] and bridged_t not in dup_names

        bridge_kept = gn_info and is_bridged_gn_package_kept(
            bridge_gn_rel, pkg_dir, gn_info, dup_gn_pkg_names
        )
        if (has_bridge_files or in_assembly_bazel or in_assembly_inputs_gn) and not bridge_kept:
            findings.append({
                "source": "migration_sanity",
                "category": "stale_assembly_bazel_inputs_bridge",
                "severity": "error",
                "file": bridge_gn_rel if has_bridge_files else (assembly_bazel_rel if in_assembly_bazel else assembly_inputs_gn_rel),
                "line": 1,
                "message": (
                    f"Package '//{pkg_dir}:{fx_pkg_name}' is defined as `fx_package` in '{bazel_rel}', "
                    f"but the legacy GN-to-Bazel assembly bridge under '//{bridge_dir_rel}' is still present or referenced."
                ),
                "remediation": (
                    f"Complete the assembly switchover for '//{pkg_dir}:{fx_pkg_name}': "
                    f"(1) in `{assembly_bazel_rel}`, replace `\"//{bridge_dir_rel}...\"` with `\"//{pkg_dir}:{fx_pkg_name}\"`; "
                    f"(2) in `{assembly_inputs_gn_rel}`, remove the `\"//{bridge_dir_rel}...\"` entry; "
                    f"(3) delete `{bridge_gn_rel}` and `{bridge_bazel_rel}` (`git rm -f {bridge_dir_rel}/BUILD.*`); "
                    f"and (4) ensure `visibility = [\"//bundles/assembly:__subpackages__\"]` is set on `{fx_pkg_name}` in `{bazel_rel}`."
                ),
            })

        parent_dir = os.path.dirname(pkg_dir).strip("/")
        child_name = os.path.basename(pkg_dir)
        parent_gn_rel = f"{parent_dir}/BUILD.gn" if parent_dir else ""
        parent_gn_abs = os.path.join(workdir, parent_gn_rel) if parent_gn_rel else ""
        if parent_dir and parent_dir not in ("src", "sdk", "tools") and os.path.isfile(parent_gn_abs):
            parent_info = parse_gn_file(parent_gn_abs)
            gn_now_targets = set(gn_info["pre_targets"].keys()) if gn_info else set()
            if gn_info and gn_info["post_text"]:
                for pm in re.finditer(r'^\s*[a-zA-Z0-9_]+\(\s*"([^"]+)"\s*\)\s*\{', gn_info["post_text"], re.M):
                    gn_now_targets.add(pm.group(1))
            base_gn_targets = set()
            try:
                base_gn_text = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                for bm in re.finditer(r'^\s*[a-zA-Z0-9_]+\(\s*"([^"]+)"\s*\)\s*\{', base_gn_text, re.M):
                    base_gn_targets.add(bm.group(1))
            except Exception:
                pass
            if parent_info and base_gn_targets:
                ref_re = re.compile(
                    r'"((?://' + re.escape(pkg_dir) + r'|' + re.escape(child_name) + r')(?::([A-Za-z0-9_.-]+))?)"'
                )
                for pname, (ptmpl, pline, pbody) in sorted(parent_info["pre_targets"].items()):
                    for rm in ref_re.finditer(pbody):
                        raw_ref = rm.group(1)
                        sub_t = rm.group(2) or child_name
                        if sub_t in base_gn_targets and sub_t not in gn_now_targets:
                            findings.append({
                                "source": "migration_sanity",
                                "category": "dangling_parent_gn_package_dep",
                                "severity": "error",
                                "file": parent_gn_rel,
                                "line": pline,
                                "message": (
                                    f"Target `{ptmpl}(\"{pname}\")` in '{parent_gn_rel}' still references `\"{raw_ref}\"`, "
                                    f"which points to deleted GN target `//{pkg_dir}:{sub_t}` (migrated to `fx_package` in '{bazel_rel}')."
                                ),
                                "remediation": (
                                    f"Remove `\"{raw_ref}\"` from `{ptmpl}(\"{pname}\")` in '{parent_gn_rel}'."
                                ),
                            })

ALLOWED_DEFS_BZL_LABELS = {
    "//build/bazel/rules/host:defs.bzl",
    "//build/bazel/rules/host/defs.bzl",
}
_defs_bzl_reexport_cache = {}


def _resolve_defs_bzl_label(mod_label):
    """Resolve a //pkg:defs.bzl or @repo//pkg:defs.bzl label to (abs_path, canonical_pkg, repo_name, pkg_path)."""
    if mod_label in ALLOWED_DEFS_BZL_LABELS:
        return None
    if mod_label.endswith(":defs.bzl"):
        prefix = mod_label[: -len(":defs.bzl")]
    elif ":" not in mod_label and mod_label.endswith("/defs.bzl"):
        prefix = mod_label[: -len("/defs.bzl")]
    else:
        return None
    if "//" not in prefix:
        return None
    repo_part, pkg_path = prefix.split("//", 1)
    pkg_path = pkg_path.strip("/")
    repo_name = repo_part.lstrip("@")
    if repo_name:
        defs_abs = os.path.join(
            workdir, "third_party", "bazel_vendor", f"{repo_name}+", pkg_path, "defs.bzl"
        )
        canonical_pkg = f"@{repo_name}//{pkg_path}" if pkg_path else f"@{repo_name}//"
    else:
        defs_abs = os.path.join(workdir, pkg_path, "defs.bzl")
        canonical_pkg = f"//{pkg_path}"
    return defs_abs, canonical_pkg, repo_name, pkg_path


def _same_dir_bzl_target(inner_label, canonical_pkg, repo_name, pkg_path):
    """Return canonical replacement label if inner_label is a .bzl in the same directory."""
    if inner_label.startswith(":"):
        rel = inner_label[1:]
        if "/" not in rel and rel.endswith(".bzl") and rel != "defs.bzl":
            sep = "" if canonical_pkg.endswith("//") else ":"
            return f"{canonical_pkg}{sep}{rel}"
        return None
    if "//" not in inner_label:
        return None
    in_repo, in_rest = inner_label.split("//", 1)
    in_repo_name = in_repo.lstrip("@")
    if in_repo_name not in ("", repo_name):
        return None
    if ":" in in_rest:
        in_pkg, in_file = in_rest.split(":", 1)
    else:
        in_pkg, _, in_file = in_rest.rpartition("/")
    in_pkg = in_pkg.strip("/")
    if in_pkg == pkg_path and "/" not in in_file and in_file.endswith(".bzl") and in_file != "defs.bzl":
        sep = "" if canonical_pkg.endswith("//") else ":"
        return f"{canonical_pkg}{sep}{in_file}"
    return None


def get_defs_bzl_reexports(mod_label):
    """Return (canonical_pkg, {exported_symbol: (target_bzl_label, orig_symbol)}) for a *:defs.bzl label."""
    resolved = _resolve_defs_bzl_label(mod_label)
    if resolved is None:
        return None
    defs_abs, canonical_pkg, repo_name, pkg_path = resolved
    cache_key = (defs_abs, canonical_pkg, repo_name, pkg_path)
    if cache_key in _defs_bzl_reexport_cache:
        return _defs_bzl_reexport_cache[cache_key]
    if not os.path.isfile(defs_abs):
        _defs_bzl_reexport_cache[cache_key] = None
        return None
    try:
        dtext = open(defs_abs, encoding="utf-8").read()
        dtree = ast.parse(dtext, filename=defs_abs)
    except Exception:
        _defs_bzl_reexport_cache[cache_key] = None
        return None

    loaded_same_dir = {}
    reexports = {}
    for stmt in dtree.body:
        if (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "load"
            and stmt.value.args
            and isinstance(stmt.value.args[0], ast.Constant)
            and isinstance(stmt.value.args[0].value, str)
        ):
            target_bzl = _same_dir_bzl_target(
                stmt.value.args[0].value, canonical_pkg, repo_name, pkg_path
            )
            if target_bzl:
                for arg in stmt.value.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        sym = arg.value
                        loaded_same_dir[sym] = (target_bzl, sym)
                        if not sym.startswith("_"):
                            reexports[sym] = (target_bzl, sym)
                for kw in stmt.value.keywords:
                    if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        loaded_same_dir[kw.arg] = (target_bzl, kw.value.value)
        elif isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            lhs = stmt.targets[0].id
            if isinstance(stmt.value, ast.Name) and stmt.value.id in loaded_same_dir:
                reexports[lhs] = loaded_same_dir[stmt.value.id]
            else:
                reexports.pop(lhs, None)
        elif isinstance(stmt, ast.FunctionDef):
            reexports.pop(stmt.name, None)

    result = (canonical_pkg, reexports)
    _defs_bzl_reexport_cache[cache_key] = result
    return result


# 4. rust_unit_test_layout & unrolled_unittest_package: a Rust unit test that isn't
#    host-only must be packaged for the device (fx_unittest_package, or
#    fx_unittest_component + fx_package + fx_test when packaging extra resources;
#    unrolled fx_packaged_binary + fx_component_manifest + fx_test_component is only
#    allowed when the test requires a custom .cml manifest).
HAS_FX_UNITTEST_PACKAGE = os.path.isfile(
    os.path.join(workdir, "build/bazel/rules/testing/fx_unittest_package.bzl")
)
STANDARD_UNITTEST_CML_SHARDS = {
    "//src/sys/test_runners/rust/default.shard.cml",
    "//src/sys/test_runners/gtest/default.shard.cml",
    "//src/sys/test_runners/gtest/zxtest.shard.cml",
    "//sdk/lib/sys/testing/elf_test_runner.shard.cml",
    "sys/testing/elf_test_runner.shard.cml",
    "syslog/use.shard.cml",
    "//sdk/lib/syslog/use.shard.cml",
    "syslog/client.shard.cml",
    "//sdk/lib/syslog/client.shard.cml",
    "syslog/offer.shard.cml",
    "//sdk/lib/syslog/offer.shard.cml",
}


def is_standard_unittest_cml(cml_abs):
    try:
        raw = open(cml_abs, encoding="utf-8").read()
    except Exception:
        return False
    # Strip /* ... */ and // ... comments outside string literals
    cleaned = re.sub(
        r'("(?:\\.|[^"\\])*")|/\*.*?\*/|//[^\n]*',
        lambda m: m.group(1) if m.group(1) is not None else "",
        raw,
        flags=re.S,
    )
    cleaned = re.sub(r"([{,]\s*)([A-Za-z_][A-Za-z0-9_-]*)\s*:", r'\1"\2":', cleaned)
    cleaned = re.sub(r",\s*([\]}])", r"\1", cleaned)
    try:
        data = json.loads(cleaned)
    except Exception:
        return False
    if not isinstance(data, dict) or not (set(data.keys()) <= {"include", "program"}):
        return False
    prog = data.get("program", {})
    if not isinstance(prog, dict) or not (set(prog.keys()) <= {"binary"}):
        return False
    inc = data.get("include", [])
    if not isinstance(inc, list) or not all(isinstance(s, str) and s in STANDARD_UNITTEST_CML_SHARDS for s in inc):
        return False
    return True
for pkg_dir in sorted(candidate_dirs):
    bazel_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    if not os.path.isfile(bazel_abs):
        continue
    try:
        btext = open(bazel_abs, encoding="utf-8").read()
        btree = ast.parse(btext, filename=bazel_abs)
    except Exception:
        continue
    rules_used = set()
    rust_tests = []
    for node in btree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            mod_label = node.value.args[0].value
            reexport_info = get_defs_bzl_reexports(mod_label)
            if reexport_info is not None:
                canonical_pkg, reexports = reexport_info
                loaded_items = [
                    (None, a.value)
                    for a in node.value.args[1:]
                    if isinstance(a, ast.Constant) and isinstance(a.value, str)
                ] + [
                    (kw.arg, kw.value.value)
                    for kw in node.value.keywords
                    if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)
                ]
                bad_syms = []
                repl_loads = []
                for alias, sym in loaded_items:
                    if sym in reexports:
                        target_bzl, orig_sym = reexports[sym]
                        bad_syms.append(sym)
                        if alias and alias != orig_sym:
                            repl_loads.append(f'`load("{target_bzl}", {alias} = "{orig_sym}")`')
                        else:
                            repl_loads.append(f'`load("{target_bzl}", "{orig_sym}")`')
                if bad_syms:
                    repls = ", ".join(repl_loads)
                    findings.append({
                        "source": "migration_sanity",
                        "category": "defs_bzl_reexport_load",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": node.lineno,
                        "message": (
                            f"'{bazel_rel}' loads {', '.join(f'`{s}`' for s in bad_syms)} from "
                            f"`{mod_label}`, which only re-exports symbols from individual `.bzl` "
                            "files in the same directory."
                        ),
                        "remediation": (
                            f"Replace `load(\"{mod_label}\", ...)` in '{bazel_rel}' with direct "
                            f"loads from the defining files in `{canonical_pkg}`: {repls}."
                        ),
                    })
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)):
            continue
        call = node.value
        rule = call.func.id
        rules_used.add(rule)
        kws = {kw.arg: kw for kw in call.keywords if kw.arg}
        name_kw = kws.get("name")
        tname = name_kw.value.value if name_kw and isinstance(name_kw.value, ast.Constant) else "?"
        if rule == "rustc_test":
            seg = ast.get_source_segment(btext, node) or ""
            rust_tests.append((tname, node.lineno, "HOST_OS_CONSTRAINTS" in seg))
    device_tests = [t for t in rust_tests if not t[2]]
    if device_tests and not ({"fx_test", "fx_unittest_package"} & rules_used):
        tname, tline, _ = device_tests[0]
        findings.append({
            "source": "migration_sanity",
            "category": "rust_unit_test_layout",
            "severity": "error",
            "file": bazel_rel,
            "line": tline,
            "message": (
                f"`rustc_test(name = \"{tname}\")` in '{bazel_rel}' is not host-only but the package declares "
                "no `fx_unittest_package` or `fx_test`, so the Fuchsia (target-side) test is missing."
            ),
            "remediation": (
                f"Add `fx_unittest_package(name = \"<test-pkg>\", unit_tests = [\":{tname}\"])` "
                "(use `fx_unittest_component` + `fx_package` + `fx_test` if packaging extra resources, or unroll into "
                "`fx_packaged_binary` + `fx_component_manifest` + `fx_test_component` only if a custom `.cml` is needed), "
                "or restrict the test with `target_compatible_with = HOST_OS_CONSTRAINTS` if GN only built it for the host."
            ),
        })

    # Test target hygiene: package-private plumbing, no "manual" tag, crate deps
    # reused, <crate>_host_test naming, and no unrolled unit-test manifests when
    # fx_unittest_package / fx_unittest_component suffices.
    calls = {}
    for node in btree.body:
        if (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)):
            kws = {kw.arg: kw.value for kw in node.value.keywords if kw.arg}
            n = kws.get("name")
            if isinstance(n, ast.Constant) and isinstance(n.value, str):
                calls[n.value] = (node.value.func.id, kws, node.lineno)

    def str_list(v):
        if isinstance(v, ast.BinOp):
            return str_list(v.left) + str_list(v.right)
        if isinstance(v, ast.List):
            return [e.value for e in v.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)]
        return []

    def str_val(v):
        return v.value if isinstance(v, ast.Constant) and isinstance(v.value, str) else None

    if HAS_FX_UNITTEST_PACKAGE:
        for tname, (rule, kws, line) in calls.items():
            if rule != "fx_test_component":
                continue
            cm_ref = (
                str_val(kws.get("compiled_manifest") or kws.get("component_manifest") or kws.get("manifest")) or ""
            ).lstrip(":")
            if not cm_ref or cm_ref not in calls or calls[cm_ref][0] != "fx_component_manifest":
                continue
            cm_kws = calls[cm_ref][1]
            manifest_rel = str_val(cm_kws.get("manifest"))
            if not manifest_rel:
                continue
            cml_rel = os.path.normpath(os.path.join(pkg_dir, manifest_rel))
            cml_abs = os.path.join(workdir, cml_rel)
            if not os.path.isfile(cml_abs):
                continue
            # Pre-existing .cml manifests in git at change_base are intentional custom manifests.
            if (
                subprocess.call(
                    ["git", "-C", workdir, "cat-file", "-e", f"{change_base}:{cml_rel}"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                == 0
            ):
                continue
            if not is_standard_unittest_cml(cml_abs):
                continue
            comp_deps = [d.lstrip(":") for d in str_list(kws.get("deps"))]
            pkg_bins = [d for d in comp_deps if d in calls and calls[d][0] == "fx_packaged_binary"]
            extra_comp_deps = [d for d in comp_deps if d not in pkg_bins]
            raw_bin = (
                (str_val(calls[pkg_bins[0]][1].get("binary")) or f":{pkg_bins[0]}")
                if pkg_bins
                else ":<test_bin>"
            )
            enclosing_pkg = None
            enclosing_pkg_kws = {}
            for pname, (prule, pkws, _) in calls.items():
                if prule == "fx_package" and (f":{tname}" in str_list(pkws.get("test_components")) or tname in str_list(pkws.get("test_components"))):
                    enclosing_pkg = pname
                    enclosing_pkg_kws = pkws
                    break
            enclosing_test = None
            if enclosing_pkg:
                for xname, (xrule, xkws, _) in calls.items():
                    if xrule == "fx_test" and (str_val(xkws.get("package")) or "").lstrip(":") == enclosing_pkg:
                        enclosing_test = xname
                        break
            has_extra_pkg_items = bool(
                extra_comp_deps
                or str_list(enclosing_pkg_kws.get("resources"))
                or str_list(enclosing_pkg_kws.get("components"))
                or str_list(enclosing_pkg_kws.get("subpackages"))
                or str_list(enclosing_pkg_kws.get("tools"))
            )
            target_test_name = enclosing_test or enclosing_pkg or tname
            if has_extra_pkg_items:
                rem = (
                    f"Delete the newly added standard unit-test manifest '{cml_rel}' (`git rm -f {cml_rel}`) and "
                    f"replace `fx_packaged_binary`, `fx_component_manifest(\"{cm_ref}\")`, and "
                    f"`fx_test_component(\"{tname}\")` in '{bazel_rel}' with "
                    f"`fx_unittest_component(name = \"{tname}\", binary = \"{raw_bin}\")` "
                    "(load from `//build/bazel/rules/testing:fx_unittest_component.bzl`, moving any `resource()` deps into "
                    "`fx_package(resources = [...])`), or attach `resource()` targets via `data = [...]` on "
                    f"`{raw_bin}` and replace the entire package/test chain with "
                    f"`fx_unittest_package(name = \"{target_test_name}\", unit_tests = [\"{raw_bin}\"])`."
                )
            else:
                rem = (
                    f"Delete the newly added standard unit-test manifest '{cml_rel}' (`git rm -f {cml_rel}`) and "
                    f"replace the unrolled `fx_packaged_binary`, `fx_component_manifest(\"{cm_ref}\")`, "
                    f"`fx_test_component(\"{tname}\")`, `fx_package`, and `fx_test` in '{bazel_rel}' with a single "
                    f"`fx_unittest_package(name = \"{target_test_name}\", unit_tests = [\"{raw_bin}\"])` "
                    "(load from `//build/bazel/rules/testing:fx_unittest_package.bzl`). "
                    "Also revert any `exports_files` edits in `src/sys/test_runners/` or "
                    "`sdk/lib/` that were only added for the deleted `.cml`."
                )
            findings.append({
                "source": "migration_sanity",
                "category": "unrolled_unittest_package",
                "severity": "error",
                "file": bazel_rel,
                "line": line,
                "message": (
                    f"`fx_test_component(name = \"{tname}\")` in '{bazel_rel}' unrolls `fx_packaged_binary` + "
                    f"`fx_component_manifest` with a newly added manifest '{cml_rel}' that only uses standard "
                    "unit-test runner/syslog shards and `program.binary`. Unit tests must default to "
                    "`fx_unittest_package` (or `fx_unittest_component`) and only use the unrolled rules when a "
                    "custom `.cml` manifest is required."
                ),
                "remediation": rem,
            })

    def is_true(v):
        return isinstance(v, ast.Constant) and v.value is True

    def hygiene(line, message, remediation):
        findings.append({
            "source": "migration_sanity",
            "category": "rust_test_target_hygiene",
            "severity": "error",
            "file": bazel_rel,
            "line": line,
            "message": message,
            "remediation": remediation,
        })

    crate_of_test = {}
    for tname, (rule, kws, line) in calls.items():
        if rule != "rustc_test":
            continue
        crate = kws.get("crate")
        crate_name = crate.value.lstrip(":") if isinstance(crate, ast.Constant) and isinstance(crate.value, str) else None
        crate_of_test[tname] = crate_name
        if "manual" in str_list(kws.get("tags")):
            hygiene(line, f"`rustc_test(name = \"{tname}\")` sets `tags = [\"manual\"]`.",
                    "Remove the `manual` tag: unit tests must run with `fx bazel test //<dir>:all`.")
        if crate_name and crate_name in calls:
            lib_deps = set(str_list(calls[crate_name][1].get("deps")))
            dup = sorted(lib_deps & set(str_list(kws.get("deps"))))
            if dup:
                hygiene(line,
                        f"`rustc_test(name = \"{tname}\")` repeats deps of its crate `:{crate_name}`: {', '.join(dup)}.",
                        "List only the GN test-only deps (test_deps) in the rustc_test. If it does not inherit "
                        "the crate deps, hoist them into a module-level list (e.g. `_DEPS = [...]`) used as "
                        "`deps = _DEPS` in the library and `deps = _DEPS + [<test-only deps>]` in the test.")

    def vis_of(kws):
        return str_list(kws.get("visibility")) if "visibility" in kws else None

    for tname, (rule, kws, line) in calls.items():
        plumbing = (rule in ("rustc_test", "fx_test_component", "fx_unittest_component")
                    or (rule in ("fx_packaged_binary", "fx_component_manifest") and is_true(kws.get("testonly")))
                    or (rule == "fx_package" and "test_components" in kws))
        if plumbing and vis_of(kws) not in (None, [":__pkg__"], ["//visibility:private"]):
            hygiene(line, f"Test target `{rule}(name = \"{tname}\")` is not restricted to its package.",
                    "Omit `visibility` (defaulting to package-private) on internal test plumbing targets.")
        if rule.startswith("wrap_host_"):
            bn = kws.get("binary_name")
            test = bn.value if isinstance(bn, ast.Constant) and isinstance(bn.value, str) else None
            crate_name = crate_of_test.get(test)
            if crate_name and tname not in (f"{crate_name}_host_test", f"{crate_name}-host_test"):
                hygiene(line, f"Host test wrapper `{tname}` does not follow the `<crate>_host_test` naming.",
                        f"Rename it to `{crate_name}_host_test` and update the BUILD.gn host_tests reference.")

print(json.dumps(findings, indent=2))
PYEOF
