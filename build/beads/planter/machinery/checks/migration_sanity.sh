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
#    name or `rustc_*` `<name>_test`), gn_test_not_migrated (error) when the standalone/host
#    `rustc_test` or `go_test` uses no GN-only features and all of its dependencies have Bazel
#    targets (in the working tree or on `origin/main`), otherwise gn_test_kept_above_sentinel
#    (info: allowed while Bazel cannot build or run it yet).
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
#    human explanatory comment precedes `# @bazel2gn:skip` or the skip already existed at
#    PLANTER_CHANGE_BASE.
# 6. test_skip_needs_reviewer_attention (info): a test skip added in this change whose comment
#    starts with `# Needs reviewer attention (bazel2gn skip):` uses the dual-testing cost exception,
#    which only a human can confirm. It is never rejected, but always reported for the reviewer.
# 7. unjustified_test_skip (error) also covers a test skip that did not exist at
#    PLANTER_CHANGE_BASE, has no technical condition (T1-T4) and is justified only by an unmarked
#    comment: agent-added skips must use the cost marker or meet T1-T4. Skips that existed at the
#    change base (code owner annotations) stay silent.
# 8. sdk_library_test_skip (error): a new marked or commented test skip whose target under test
#    (followed through same-package references, e.g. fx_unittest_package -> fx_cc_binary ->
#    library) is an sdk_*() library other than sdk_source_set() (Bazel `idk_cc_shared_library`,
#    `idk_cc_static_library` and their `_zx` variants), whose tests stay dual-tested. When the
#    target under test lives in another package, the INFO finding says this can't be verified.
# 9. generic_test_output_name (error): flags any `rustc_test` or `host_rustc_test` in BUILD.bazel
#    whose effective GN binary output name is an undisambiguated generic name (such as `lib_test`,
#    `bin_test`, `core_test`, etc.) that collides in `${root_out_dir}` across packages.
# 10. unsanctioned_gn_construct_below_sentinel (error): a GN-only construct (exclude_toolchain_tags,
#     configs -=, disable_syslog_backend, forward_variables_from, concurrent_jobs.<pool>, or a
#     "<label>($<x>_toolchain)" label) that the change adds to a bazel2gn-generated target below the
#     sentinel without a BUILD.bazel source (annotation, native attribute or a dedicated tag the
#     checkout's bazel2gn implements; see check_gn_only_constructs_below_sentinel). Sanctioned lines
#     are accepted, so targets with such attributes can be generated instead of kept above the
#     sentinel; gn_only_attr_target_kept_above_sentinel (info) points at that recipe.
# 11. When the checkout's bazel2gn converts `fx_compiled_manifest()` to GN `cm()` and passes
#     `restricted_features` through (detected from literals in build/tools/bazel2gn/*.go),
#     `fx_compiled_manifest` is a convertible Bazel rule and `cm` a convertible GN template (with
#     the attributes bazel2gn emits: manifest, output_name, config_values_package_path,
#     restricted_features), and `restricted_features` no longer blocks converting a
#     `fuchsia_component_manifest`: checks 1 and 2 then apply to them.
# 12. dropped_preexisting_bazel_export / dropped_preexisting_bazel_target (error): when a
#     package's BUILD.bazel already existed at PLANTER_CHANGE_BASE, flags pre-existing
#     `exports_files(...)` entries (or their visibility scopes) and pre-existing named targets
#     that were dropped when adding newly migrated targets to BUILD.bazel.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" <<'PYEOF'
import ast
import itertools
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")
target_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", f"{sys.argv[3]} {target_dir}")
    if d.strip().strip("/")
]


def _read_bazel2gn_go_sources():
    """Concatenated non-test .go sources of the checkout's build/tools/bazel2gn (feature detection)."""
    src = []
    b2gn_dir = os.path.join(workdir, "build", "tools", "bazel2gn")
    try:
        names = sorted(os.listdir(b2gn_dir))
    except OSError:
        names = []
    for fn in names:
        if fn.endswith(".go") and not fn.endswith("_test.go"):
            try:
                src.append(open(os.path.join(b2gn_dir, fn), encoding="utf-8").read())
            except OSError:
                pass
    return "\n".join(src)


BAZEL2GN_GO_SOURCES = _read_bazel2gn_go_sources()
# bazel2gn converts `fx_compiled_manifest()` to GN `cm()` (a compiled manifest that is not packaged
# as a resource, e.g. one embedded in a binary).
FX_COMPILED_MANIFEST_SUPPORTED = '"fx_compiled_manifest"' in BAZEL2GN_GO_SOURCES
# bazel2gn passes `restricted_features` of `fx_component_manifest()` / `fx_compiled_manifest()`
# through to GN.
RESTRICTED_FEATURES_SUPPORTED = '"restricted_features"' in BAZEL2GN_GO_SOURCES

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
    "fuchsia_structured_config_rust_lib",
    "fx_component",
    "fx_component_manifest",
    "ffx_plugin",
    "ffx_tool",
}
if FX_COMPILED_MANIFEST_SUPPORTED:
    CONVERTIBLE_BAZEL_RULES.add("fx_compiled_manifest")

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
    "ffx_plugin",
    "ffx_tool",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
    "fuchsia_structured_config_rust_lib",
    "fuchsia_component",
    "fuchsia_component_manifest",
}
if FX_COMPILED_MANIFEST_SUPPORTED:
    CONVERTIBLE_GN_TEMPLATES.add("cm")
# Bazel manifest rules bazel2gn converts, and their GN templates.
FX_MANIFEST_RULE_TO_GN = {"fx_component_manifest": "fuchsia_component_manifest"}
if FX_COMPILED_MANIFEST_SUPPORTED:
    FX_MANIFEST_RULE_TO_GN["fx_compiled_manifest"] = "cm"

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
# Bazel rules that bazel2gn emits as GN sdk_*() libraries other than sdk_source_set() (see
# //build/tools/bazel2gn/constants.go). Their tests stay dual-tested.
SDK_DUAL_TESTED_BAZEL_RULES = {
    "idk_cc_shared_library",
    "idk_cc_shared_library_zx",
    "idk_cc_static_library",
    "idk_cc_static_library_zx",
}
# Attributes whose labels name the code a test exercises.
TUT_REF_ATTRS = {"deps", "crate", "unit_tests", "binary", "binary_name", "embed", "implementation_deps", "test_deps"}
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
_fidl_bzl_path = os.path.join(workdir, "build/bazel/rules/fidl/fidl_library.bzl")
FIDL_RUST_NEXT_UNIMPLEMENTED = (
    os.path.isfile(_fidl_bzl_path)
    and "_rust_fdomain_next" not in open(_fidl_bzl_path, encoding="utf-8").read()
)
FIDL_RUST_NEXT_DEP_RE = re.compile(r":([A-Za-z0-9_.-]+_rust_(?:fdomain_)?next)\b")


# Check whether collect_debug_symbols_manifest_aspect in
# //build/bazel/debug_symbols/aspects.bzl collects debug symbols for
# rust_dylib_library (rustc_dylib) in the base tree (ignoring any local edit to
# aspects.bzl in the migration change itself). Until upstream supports it,
# migrating an assembled fuchsia_package whose binary links a rustc_dylib to
# fx_package causes export_last_build_debug_symbols to fail on assembled images.
def _check_rust_dylib_debug_symbols_supported(base_rev):
    aspects_rel = "build/bazel/debug_symbols/aspects.bzl"
    aspects_path = os.path.join(workdir, aspects_rel)
    if not os.path.isfile(aspects_path):
        return True
    try:
        base_lines = git_lines(["show", f"{base_rev}:{aspects_rel}"])
        raw = "\n".join(base_lines) if base_lines else open(aspects_path, encoding="utf-8").read()
        cleaned = re.sub(r"#[^\n]*", "", raw)
        return '"rust_dylib_library"' in cleaned or "'rust_dylib_library'" in cleaned
    except OSError:
        return True


_bazel_pkg_targets_cache = {}


# Parse BUILD.bazel targets and local/external label references in a package directory.
def _get_pkg_bazel_targets_map(pdir):
    if pdir in _bazel_pkg_targets_cache:
        return _bazel_pkg_targets_cache[pdir]
    bpath = os.path.join(workdir, pdir, "BUILD.bazel")
    if not os.path.isfile(bpath):
        _bazel_pkg_targets_cache[pdir] = {}
        return {}
    try:
        txt = open(bpath, encoding="utf-8").read()
        tree = ast.parse(txt, filename=bpath)
    except Exception:
        _bazel_pkg_targets_cache[pdir] = {}
        return {}
    var_strs = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            vname = node.targets[0].id
            var_strs[vname] = [
                sub.value
                for sub in ast.walk(node.value)
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
            ]
    tmap = {}
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        tname = None
        refs = []
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                tname = kw.value.value
            elif kw.arg not in (
                "visibility",
                "package_name",
                "component_name",
                "test_deps",
                "test_compile_data",
            ):
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        s = sub.value
                        if s.startswith(":") or s.startswith("//"):
                            refs.append(s)
                    elif isinstance(sub, ast.Name) and sub.id in var_strs:
                        for s in var_strs[sub.id]:
                            if s.startswith(":") or s.startswith("//"):
                                refs.append(s)
        if tname:
            tmap[tname] = (rule, refs)
    _bazel_pkg_targets_cache[pdir] = tmap
    return tmap


# Find a reachable rustc_dylib / rust_dylib_library dependency from seed target names
# in pdir/BUILD.bazel when rust_dylib_library debug symbol collection is unsupported
# or the rustc_dylib package is still dual-built in BUILD.gn (which duplicates the
# shared dylib in BlobFS across GN and Bazel packages in assembled images).
def find_uncollected_rust_dylib_dep(pdir, seed_names):
    queue = [(pdir, name, None) for name in seed_names if name]
    visited = set()
    visited_pkgs = set()
    while queue:
        cur_pkg, cur_name, entry_lbl = queue.pop(0)
        if (cur_pkg, cur_name) in visited:
            continue
        visited.add((cur_pkg, cur_name))
        visited_pkgs.add(cur_pkg)
        if len(visited_pkgs) > 250:
            break
        tmap = _get_pkg_bazel_targets_map(cur_pkg)
        if cur_name not in tmap:
            continue
        rule, refs = tmap[cur_name]
        if rule in ("rustc_dylib", "rust_dylib_library") and (
            not RUST_DYLIB_DEBUG_SYMBOLS_SUPPORTED
            or os.path.isfile(os.path.join(workdir, cur_pkg, "BUILD.gn"))
        ):
            return f"//{cur_pkg}:{cur_name}"
        cur_entry = entry_lbl if cur_pkg != pdir else None
        for r in refs:
            if r.startswith(":"):
                queue.insert(0, (cur_pkg, r[1:], cur_entry or f"//{cur_pkg}:{cur_name}"))
            elif r.startswith("//") and not r[2:].startswith(
                ("conditions:", "third_party/", "prebuilt/", "build/", "sdk/fidl/")
            ):
                body = r[2:]
                if ":" in body:
                    sub_pkg, sub_t = body.split(":", 1)
                else:
                    sub_pkg = body.strip("/")
                    sub_t = os.path.basename(sub_pkg)
                sub_pkg = sub_pkg.strip("/")
                if sub_pkg:
                    next_entry = cur_entry or f"//{sub_pkg}:{sub_t}"
                    queue.append((sub_pkg, sub_t, next_entry))
    return None
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
# commit, "HEAD" when HEAD is unrelated upstream history) plus untracked files. When HEAD is
# already a [bazel_migration] commit (e.g. jj @ vs @-), also include files changed in HEAD~1.
change_base = normalize_git_rev(os.environ.get("PLANTER_CHANGE_BASE", ""))
changed.update(git_lines(["diff", "--name-only", change_base]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
head_subj = git_lines(["log", "-1", "--format=%s", "HEAD"])
if change_base == "HEAD" and not changed and head_subj and head_subj[0].startswith("[bazel_migration]"):
    head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
    if not target_dir or any(p.startswith(target_dir + "/") for p in head_files):
        change_base = "HEAD~1"
        changed.update(head_files)
RUST_DYLIB_DEBUG_SYMBOLS_SUPPORTED = _check_rust_dylib_debug_symbols_supported(change_base)
all_changed_build_dirs = set()
for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if d:
            all_changed_build_dirs.add(d)
        if (
            d
            and d not in ("tools", "src", "sdk")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
        ):
            candidate_dirs.add(d)


def parse_bazel_targets(bazel_path, pkg_dir="", text=None):
    if text is None:
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
        ext_tut_refs = set()
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
                        elif sval.startswith("//") and kw.arg in TUT_REF_ATTRS:
                            ext_tut_refs.add(sval)
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
            "ext_tut_refs": ext_tut_refs,
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
def has_technical_test_skip_condition(t, targets_by_name):
    """T1-T4 of unjustified_test_skip: a skip that bazel2gn limits or Bazel-only code require."""
    rule = t["rule"]
    tut_refs = [t["crate_ref"]] if t.get("crate_ref") else t.get("local_test_refs", [])
    local_refs = [r for r in tut_refs if r in targets_by_name]
    t1_tut_skipped = bool(local_refs) and all(targets_by_name[r]["skip_line"] is not None for r in local_refs)
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
    return bool(t1_tut_skipped or t2_fx_unittest_resources or t3_host_test_data or t4_unsupported_rule)


def resolve_targets_under_test(seed_names, targets_by_name):
    """Follows same-package references from a test to the target under test.

    Expands test and testonly targets (package -> test binary -> ...) and stops at the first
    non-test, non-testonly target: that is the target under test, and its own deps are not.
    Returns (sdk_dual_tested_targets, local_library_targets, external_refs).
    """
    seen = set()
    queue = list(seed_names)
    sdk, libs, ext = set(), set(), set()
    while queue:
        name = queue.pop()
        if name in seen or name not in targets_by_name:
            continue
        seen.add(name)
        tgt = targets_by_name[name]
        if tgt["rule"] not in BAZEL_TEST_RULES and not tgt.get("testonly"):
            # The target under test (or, for a skipped test attribute, the seed library itself).
            if tgt["rule"] in SDK_DUAL_TESTED_BAZEL_RULES:
                sdk.add(name)
            if name not in seed_names:
                libs.add(name)
            continue
        ext.update(tgt.get("ext_tut_refs", ()))
        refs = set(tgt.get("dep_refs", ())) | set(tgt.get("local_test_refs", ())) | set(tgt.get("unit_tests_refs", ()))
        for extra in (tgt.get("crate_ref"), tgt.get("binary_ref")):
            if extra:
                refs.add(extra)
        queue.extend(refs)
    return sorted(sdk), sorted(libs), sorted(ext)


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


# GN-only target attributes and constructs that bazel2gn does not translate from a plain
# BUILD.bazel attribute. A convertible target using them may stay hand-written above the
# sentinel, but it may also be generated below it when BUILD.bazel names them with a sanctioned
# annotation or tag (`# @bazel2gn:raw_attribute:`, `raw_overwrite`, `value_overwrite`,
# `path_overwrite`, tags such as `exclude_toolchain_tags=<tag>` or
# `assert_no_deps_current_toolchain=<label>`): see check_gn_only_constructs_below_sentinel.
GN_ONLY_ATTR_RE = re.compile(
    r"(?<![\w.])(?:exclude_toolchain_tags\s*\+?=|configs\s*-=|disable_syslog_backend\s*=|forward_variables_from\s*\(|concurrent_jobs\.)"
)
GN_BLOCKED_CONSTRUCT_RE = re.compile(
    # bazel2gn translates fuchsia_structured_config_rust_lib (//build/tools/bazel2gn/constants.go),
    # so only the other structured config templates are GN-only.
    r"\b(?:loadable_module|network_unified_binary|fuchsia_structured_config_(?!rust_lib\b)[a-z0-9_]+)\s*\("
    r"|\(\$[a-zA-Z0-9_]*toolchain\)"
)
GN_SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)


def gn_hand_written_text(text):
    """The part of a BUILD.gn above the bazel2gn sentinel (all of it without a sentinel).

    GN-only constructs below the sentinel were generated from BUILD.bazel annotations, so they
    show that the package is already migrated rather than that it is blocked on GN-only code.
    """
    m = GN_SENTINEL_RE.search(text)
    return text[: m.start()] if m else text


# Check whether a pre-sentinel GN target is a testonly binary or helper library
# consumed by a GN test or test package/component kept above the sentinel.
def is_packaged_testonly_gn_binary(gname, gtmpl, gbody, pre_targets, bazel_names):
    if gname in bazel_names:
        return False
    pkg_tmpls = (
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "fuchsia_component",
        "bootfs_test",
    )
    only_test_pkg_tmpls = (
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_test_component",
        "fuchsia_unittest_component",
        "bootfs_test",
    )
    non_test_consumer_tmpls = (
        "fuchsia_package",
        "fuchsia_package_with_single_component",
        "fuchsia_structured_config_rust_lib",
    )
    test_consumer_tmpls = pkg_tmpls + ("rustc_test", "go_test", "test", "executable")
    ref_re = re.compile(r'"(?::|//' + re.escape(target_dir) + r':)' + re.escape(gname) + r'(?:"|\()') if target_dir else re.compile(r'":' + re.escape(gname) + r'(?:"|\()')
    short_ref_re = re.compile(r'":' + re.escape(gname) + r'(?:"|\()')
    if gtmpl in (
        "fuchsia_component",
        "fuchsia_component_manifest",
        "fuchsia_structured_config_rust_lib",
    ):
        visited_c = {gname}
        queue_c = [gname]
        saw_test_or_blocked_consumer = False
        saw_non_test_consumer = False
        while queue_c:
            cur_c = queue_c.pop(0)
            c_ref_re = (
                re.compile(
                    r'"(?::|//' + re.escape(target_dir) + r':)' + re.escape(cur_c) + r'(?:"|\()'
                )
                if target_dir
                else re.compile(r'":' + re.escape(cur_c) + r'(?:"|\()')
            )
            c_short_re = re.compile(r'":' + re.escape(cur_c) + r'(?:"|\()')
            for pname, (ptmpl, _, pbody) in pre_targets.items():
                if pname in visited_c or not (c_short_re.search(pbody) or c_ref_re.search(pbody)):
                    continue
                visited_c.add(pname)
                if ptmpl in ("fuchsia_package", "fuchsia_package_with_single_component"):
                    saw_non_test_consumer = True
                elif (
                    ptmpl in only_test_pkg_tmpls
                    or re.search(r"\btestonly\s*=\s*true\b", pbody)
                    or ptmpl.startswith("fuchsia_structured_config_values")
                ):
                    saw_test_or_blocked_consumer = True
                elif pname in bazel_names:
                    saw_non_test_consumer = True
                else:
                    queue_c.append(pname)
        if saw_test_or_blocked_consumer and not saw_non_test_consumer:
            return True
    if not re.search(r"\btestonly\s*=\s*true\b", gbody):
        return False
    if gtmpl in ("executable", "rustc_binary", "go_binary", "python_binary", "fuchsia_component", "fuchsia_component_manifest"):
        return any(
            ptmpl in pkg_tmpls and (short_ref_re.search(pbody) or ref_re.search(pbody))
            for ptmpl, _, pbody in pre_targets.values()
        )
    if gtmpl in ("rustc_library", "source_set", "static_library", "go_library", "python_library"):
        return any(
            pname != gname
            and pname not in bazel_names
            and ptmpl in test_consumer_tmpls
            and (short_ref_re.search(pbody) or ref_re.search(pbody))
            for pname, (ptmpl, _, pbody) in pre_targets.items()
        )
    return False


# Check whether a pre-sentinel GN `fuchsia_component_manifest`, `fuchsia_component` or (when the
# checkout's bazel2gn converts `fx_compiled_manifest()`) `cm` uses attributes or dependencies that
# `bazel2gn` cannot convert.
def is_gn_component_target_blocked(gname, gtmpl, gbody, pre_targets, bazel_targets, pdir):
    if gtmpl not in ("fuchsia_component_manifest", "fuchsia_component") and not (
        gtmpl == "cm" and FX_COMPILED_MANIFEST_SUPPORTED
    ):
        return False
    cleaned = re.sub(r"#[^\n]*", "", gbody)
    if (
        "$" in cleaned
        or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
        or GN_ONLY_ATTR_RE.search(cleaned)
    ):
        return True
    attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", cleaned))
    if gtmpl in ("fuchsia_component_manifest", "cm"):
        if gtmpl == "cm":
            allowed = {"manifest", "output_name", "config_values_package_path", "testonly", "visibility"}
        else:
            allowed = {"manifest", "component_name", "testonly", "visibility"}
        if RESTRICTED_FEATURES_SUPPORTED:
            allowed.add("restricted_features")
        if not attrs or not (attrs <= allowed) or "manifest" not in attrs:
            return True
        ref_pat = re.compile(
            r'"(?::'
            + re.escape(gname)
            + (r"|//" + re.escape(pdir) + r":" + re.escape(gname) if pdir else "")
            + r')"'
        )
        return any(
            ptmpl.startswith("fuchsia_structured_config_values") and ref_pat.search(pbody)
            for ptmpl, _, pbody in pre_targets.values()
        )
    allowed = {
        "cm_label",
        "manifest",
        "component_name",
        "check_includes",
        "deps",
        "testonly",
        "visibility",
    }
    if not attrs or not (attrs <= allowed) or not (attrs & {"cm_label", "manifest"}):
        return True
    cm_m = re.search(r'\bcm_label\s*=\s*"([^"]+)"', cleaned)
    if cm_m:
        cm_lbl = cm_m.group(1)
        cm_local = (
            cm_lbl[1:]
            if cm_lbl.startswith(":")
            else (
                cm_lbl.split(":", 1)[1]
                if pdir and cm_lbl.startswith(f"//{pdir}:")
                else ""
            )
        )
        if not cm_local:
            return True
        if cm_local in pre_targets:
            mtmpl, _, mbody = pre_targets[cm_local]
            if mtmpl != "fuchsia_component_manifest" or is_gn_component_target_blocked(
                cm_local, mtmpl, mbody, pre_targets, bazel_targets, pdir
            ):
                return True
    bz_bin_rules = {
        "rustc_binary",
        "rust_binary",
        "cc_binary",
        "fx_cc_binary",
        "go_binary",
        "cc_binary_host_tool",
        "go_binary_host_tool",
        "idk_cc_binary_host_tool",
        "idk_go_binary_host_tool",
        "idk_rustc_binary_host_tool",
        "rustc_test",
        "fx_packaged_binary",
    }
    bz_map = {t["name"]: t for t in bazel_targets}
    gn_bin_tmpls = {
        "rustc_binary",
        "executable",
        "go_binary",
        "renamed_binary",
        "rustc_test",
    }
    for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", cleaned):
        for d_lbl in re.findall(r'"([^"]+)"', dm.group(1)):
            d_local = (
                d_lbl[1:]
                if d_lbl.startswith(":")
                else (
                    d_lbl.split(":", 1)[1]
                    if pdir and d_lbl.startswith(f"//{pdir}:")
                    else ""
                )
            )
            if not d_local:
                return True
            if (
                d_local in bz_map
                and bz_map[d_local]["rule"] in bz_bin_rules
                and bz_map[d_local]["skip_line"] is None
            ):
                continue
            if (
                d_local in pre_targets
                and pre_targets[d_local][0] in gn_bin_tmpls
                and not GN_ONLY_ATTR_RE.search(pre_targets[d_local][2])
            ):
                continue
            return True
    return False


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
            if lname in seen_local or lname in bazel_names:
                continue
            # Resolve `<owner>_test` unit-test labels to the owning pre-sentinel GN target.
            target_key = lname if lname in pre_targets else re.sub(r"_test$", "", lname)
            if (
                target_key in seen_local
                or (lname == target_key and target_key in bazel_names)
                or target_key not in pre_targets
            ):
                continue
            seen_local.add(lname)
            seen_local.add(target_key)
            ltmpl, _, lbody = pre_targets[target_key]
            if (
                GN_ONLY_ATTR_RE.search(lbody)
                or GN_BLOCKED_CONSTRUCT_RE.search(lbody)
                or (
                    ltmpl not in CONVERTIBLE_GN_TEMPLATES
                    and ltmpl not in GN_TEST_TEMPLATES
                    and ltmpl != "group"
                )
            ):
                gn_blocked_dep = True
            else:
                body_queue.append(lbody)
        for dm in re.finditer(r'"//([^":(\s]+)(?::([^"(\s]+))?"', cur_body):
            dep_pkg = dm.group(1).strip("/")
            dep_target = dm.group(2) or os.path.basename(dep_pkg)
            if (
                not dep_pkg
                or dep_pkg == pkg_dir
                or dep_pkg.startswith(("build/", "prebuilt/", "third_party/", "out/"))
            ):
                continue
            dep_gn = os.path.join(workdir, dep_pkg, "BUILD.gn")
            dep_bazel = os.path.join(workdir, dep_pkg, "BUILD.bazel")
            has_bazel_target = False
            if os.path.isfile(dep_bazel):
                try:
                    dep_bz_txt = open(dep_bazel, encoding="utf-8").read()
                    has_bazel_target = (
                        re.search(r'\bname\s*=\s*"' + re.escape(dep_target) + r'"', dep_bz_txt)
                        is not None
                    )
                except Exception:
                    has_bazel_target = True
            if os.path.isfile(dep_gn) and not has_bazel_target:
                if not os.path.isfile(dep_bazel) and f"//{dep_pkg}" not in unmigrated_deps:
                    unmigrated_deps.append(f"//{dep_pkg}")
                try:
                    dep_gn_txt = open(dep_gn, encoding="utf-8").read()
                    if "declare_args(" in dep_gn_txt and f"//{dep_pkg}" not in declare_args_deps:
                        declare_args_deps.append(f"//{dep_pkg}")
                    dep_hand_txt = gn_hand_written_text(dep_gn_txt)
                    if GN_ONLY_ATTR_RE.search(dep_hand_txt) or GN_BLOCKED_CONSTRUCT_RE.search(
                        dep_hand_txt
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


def parse_gn_text(text):
    sentinel_re = GN_SENTINEL_RE
    m = sentinel_re.search(text)
    has_sentinel = bool(m)
    pre_text = text[: m.start()] if m else text
    post_text = text[m.end() :] if m else ""
    pre_targets = {}
    post_targets = {}
    # Match horizontal leading indentation only so `tm.start(1)` is on the target's
    # declaration line rather than a preceding blank line (`\s` matches `\n` under `re.M`).
    target_re = re.compile(r'^[ \t]*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for tm in target_re.finditer(pre_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = pre_text.count("\n", 0, tm.start(1)) + 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(pre_text, brace_idx)
        body = pre_text[brace_idx + 1 : end_idx]
        pre_targets[name] = (tmpl, line_no, body)
    for tm in target_re.finditer(post_text):
        tmpl, name = tm.group(1), tm.group(2)
        line_no = (text.count("\n", 0, m.end() + tm.start(1)) + 1) if m else 1
        brace_idx = tm.end() - 1
        end_idx = find_closing_brace(post_text, brace_idx)
        body = post_text[brace_idx + 1 : end_idx]
        post_targets[name] = (tmpl, line_no, body)
    return {
        "has_sentinel": has_sentinel,
        "pre_text": pre_text,
        "post_text": post_text,
        "pre_targets": pre_targets,
        "post_targets": post_targets,
    }


def parse_gn_file(gn_path):
    try:
        text = open(gn_path, encoding="utf-8").read()
    except Exception:
        return None
    return parse_gn_text(text)


findings = []
gn_kept_pkg_test_deps_by_dir = {}

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
    # Treat host binaries wrapped by a pre-sentinel bazel_host_tool() target as
    # Bazel-only packaged binaries so `# @bazel2gn:skip` on the underlying host
    # binary in BUILD.bazel is allowed even when bazel_host_tool() uses a
    # different GN target name.
    if gn_info:
        for hname, (htmpl, _, hbody) in gn_info["pre_targets"].items():
            if htmpl == "bazel_host_tool":
                packaged_binaries.add(hname)
                m_bt = re.search(r'\bbazel_target\s*=\s*"([^"]+)"', hbody)
                if m_bt:
                    bt_lbl = m_bt.group(1).strip()
                    if bt_lbl.startswith(":"):
                        packaged_binaries.add(bt_lbl[1:])
                    elif bt_lbl.startswith(f"//{pkg_dir}:"):
                        packaged_binaries.add(bt_lbl[len(f"//{pkg_dir}:") :])
    converted_manifest_refs = {
        r
        for t in bazel_targets
        if t["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib")
        and t["skip_line"] is None
        for r in t.get("gn_dep_refs", ())
    }
    bazel_only_manifest_refs = {
        r
        for t in bazel_targets
        if t["rule"] == "fx_test_component"
        or (t["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib") and t["skip_line"] is not None)
        for r in t.get("dep_refs", ())
    }
    auto_skipped_manifests = {
        t["name"]
        for t in bazel_targets
        if t["rule"] == "fx_component_manifest"
        and t["name"] in bazel_only_manifest_refs
        and t["name"] not in converted_manifest_refs
    }
    fx_all_pkgs = [
        t
        for t in bazel_targets
        if t["rule"] in ("fx_package", "fuchsia_package", "fx_unittest_package")
    ]
    # Non-test fx_package targets (excludes fx_unittest_package and test_components packages).
    fx_pkgs = [t for t in fx_all_pkgs if not t.get("is_test_pkg")]

    # Check for cross-tree Fuchsia package_name collisions across BUILD.gn and BUILD.bazel
    # so staging packages into amber-files/repository does not fail with conflicting manifests.
    GN_FUCHSIA_PKG_TEMPLATES = (
        "fuchsia_package",
        "fuchsia_package_with_single_component",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_fuzzer_package",
        "fuchsia_shell_package",
        "fuchsia_driver_package",
    )
    BAZEL_FUCHSIA_PKG_RULES = (
        "fx_package",
        "fx_unittest_package",
        "fuchsia_package",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
    )

    def find_gn_packages_in_file(other_abs):
        try:
            text = open(other_abs, encoding="utf-8").read()
        except Exception:
            return []
        out = []
        target_re = re.compile(r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
        for tm in target_re.finditer(text):
            otmpl, oname = tm.group(1), tm.group(2)
            if otmpl not in GN_FUCHSIA_PKG_TEMPLATES:
                continue
            oline = text.count("\n", 0, tm.start()) + 1
            end_idx = find_closing_brace(text, tm.end() - 1)
            obody = re.sub(r"#[^\n]*", "", text[tm.end() : end_idx])
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"$]+)"', obody)
            eff_name = pn_m.group(1) if pn_m else oname
            out.append((eff_name, otmpl, oname, oline))
        return out

    def find_colliding_fuchsia_package(cand_pkg_name, cur_pkg_dir):
        if not cand_pkg_name:
            return None
        try:
            grep_out = subprocess.run(
                [
                    "git",
                    "-C",
                    workdir,
                    "grep",
                    "-l",
                    "--fixed-strings",
                    "-e",
                    f'"{cand_pkg_name}"',
                    "--",
                    "BUILD.gn",
                    "*/BUILD.gn",
                    "BUILD.bazel",
                    "*/BUILD.bazel",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=30,
                check=False,
            ).stdout
        except Exception:
            return None
        for raw_line in grep_out.splitlines():
            other_rel = raw_line.strip()
            if not other_rel:
                continue
            other_dir = os.path.dirname(other_rel).strip("/")
            if other_dir == cur_pkg_dir or other_rel.startswith(("bundles/assembly/", "out/")):
                continue
            other_abs = os.path.join(workdir, other_rel)
            if other_rel.endswith("BUILD.gn"):
                for eff_name, otmpl, oname, oline in find_gn_packages_in_file(other_abs):
                    if eff_name == cand_pkg_name:
                        return other_rel, f'`{otmpl}("{oname}")`', oline
            elif other_rel.endswith("BUILD.bazel"):
                for ot in parse_bazel_targets(other_abs, other_dir):
                    if (
                        ot["rule"] in BAZEL_FUCHSIA_PKG_RULES
                        and ot.get("package_name") == cand_pkg_name
                    ):
                        return other_rel, f'`{ot["rule"]}(name = "{ot["name"]}")`', ot["line"]
        return None

    def suggest_disambiguated_pkg_name(cur_pkg_dir, orig_name):
        parts = [p.replace("_", "-") for p in cur_pkg_dir.split("/") if p]
        stem = "-".join(parts[-2:]) if len(parts) >= 2 else (parts[-1] if parts else orig_name)
        cand = f"{stem}-tests" if orig_name.endswith(("tests", "test")) else stem
        if cand == orig_name and len(parts) >= 3:
            suffix = "-tests" if orig_name.endswith(("tests", "test")) else ""
            cand = "-".join(parts[-3:]) + suffix
        return cand

    def check_cross_tree_package_name_collisions(all_pkgs, cur_pkg_dir, brel, grel, g_info):
        has_sent = bool(g_info and g_info.get("has_sentinel"))
        for pt in all_pkgs:
            p_name = pt["package_name"]
            candidates = [p_name]
            if (
                has_sent
                and pt["rule"] == "fx_unittest_package"
                and pt["skip_line"] is None
                and not pt.get("has_resources")
            ):
                candidates.append(f"{p_name}-gn")
            for cand in candidates:
                hit = find_colliding_fuchsia_package(cand, cur_pkg_dir)
                if not hit:
                    continue
                other_rel, other_desc, other_line = hit
                suggested = suggest_disambiguated_pkg_name(cur_pkg_dir, p_name)
                msg = (
                    f'`{pt["rule"]}(name = "{pt["name"]}")` in \'{brel}\' publishes Fuchsia '
                    f"package `{cand}`, which collides with {other_desc} in '{other_rel}' "
                    f"(line {other_line}); staging both into `amber-files/repository` on CQ "
                    "fails with `conflict between package manifests`."
                )
                rem = (
                    f"Rename `{pt['rule']}` in '{brel}' to a globally unique package name "
                    f"such as `\"{suggested}\"` (or set `package_name = \"{suggested}\"`), "
                    f"re-run `fx bazel2gn -d {cur_pkg_dir}` if '{grel}' has a BAZEL2GN "
                    "SENTINEL, and update `bazel_test_suite`, `group(\"tests\")`, and commit "
                    "`Bazel-Migration-Target:` / `Test:` footers to match."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "cross_tree_package_name_collision",
                    "severity": "error",
                    "file": brel,
                    "line": pt["line"],
                    "message": msg,
                    "remediation": rem,
                })
                break

    check_cross_tree_package_name_collisions(fx_all_pkgs, pkg_dir, bazel_rel, gn_rel, gn_info)

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

    # Record non-blocking INFO alignment entries for rustc_check_cfg_build_flags targets and
    # fuchsia_* / cm -> fx_* component/package rule mappings (including the verified-below-sentinel
    # suffix emitted by diffengine) so review_diff does not flag them.
    def record_check_cfg_alignment_info(btargets, brel, g_info):
        gn_tmap = (
            {**g_info.get("post_targets", {}), **g_info.get("pre_targets", {})}
            if g_info
            else {}
        )
        default_fx_rule_map = {
            "fx_component_manifest": "fuchsia_component_manifest",
            "fx_compiled_manifest": "cm",
            "fx_component": "fuchsia_component",
            "fx_test_component": "fuchsia_test_component",
            "fx_unittest_component": "fuchsia_unittest_component",
            "fx_unittest_package": "fuchsia_unittest_package",
            "fx_package": "fuchsia_package",
        }

        def append_rule_mapping_info(bname, bline, gtmpl, brule, rem):
            base_msg = (
                f'Target "{bname}" (Rule & Directives): '
                f"Unusual rule mapping from {gtmpl} to {brule}"
            )
            verified_suffix = (
                f" (verified: bazel2gn output below the sentinel is "
                f'{gtmpl}("{bname}"), the original GN template)'
            )
            for msg in (base_msg, base_msg + verified_suffix):
                findings.append({
                    "source": "migration_sanity",
                    "category": "attribute_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": msg,
                    "remediation": rem,
                })

        for bt in btargets:
            bname = bt["name"]
            bline = bt["line"]
            if bt["rule"].startswith("fx_"):
                gtmpl = (
                    gn_tmap[bname][0]
                    if bname in gn_tmap
                    else default_fx_rule_map.get(bt["rule"], "")
                )
                if (
                    gtmpl.startswith("fuchsia_")
                    or gtmpl == "cm"
                    or (bt["rule"] == "fx_packaged_binary" and gtmpl.endswith("_binary"))
                ):
                    append_rule_mapping_info(
                        bname,
                        bline,
                        gtmpl,
                        bt["rule"],
                        f"{bt['rule']} is the standard in-tree Bazel rule corresponding to GN {gtmpl}.",
                    )
            if bt["rule"] != "rustc_check_cfg_build_flags" or bt["skip_line"] is not None:
                continue
            rem = (
                "bazel2gn natively converts rustc_check_cfg_build_flags(cfgs, features) "
                "into a GN config() with --check-cfg rustflags."
            )
            append_rule_mapping_info(bname, bline, "config", "rustc_check_cfg_build_flags", rem)
            attr_msg = (
                f'Target "{bname}" (Target Attributes): '
                "rustflags in GN not carried over to Bazel"
            )
            findings.append({
                "source": "migration_sanity",
                "category": "attribute_alignment",
                "severity": "INFO",
                "file": brel,
                "line": bline,
                "message": attr_msg,
                "remediation": rem,
            })

    record_check_cfg_alignment_info(bazel_targets, bazel_rel, gn_info)

    # Record as INFO when *_test.go files from a base GN go_library moved to a
    # same-package go_test/host_go_test embedding the library in BUILD.bazel.
    # Emitting the same (category, file, line, message) tuple as INFO here
    # pre-populates planter's finding deduplication set before review_diff runs.
    def emit_go_test_moved_sources_info():
        try:
            btext_go = open(os.path.join(workdir, bazel_rel), encoding="utf-8").read()
            btree_go = ast.parse(btext_go, filename=bazel_rel)
        except Exception:
            return
        go_libs = {}
        test_srcs_by_embed = {}
        for node in btree_go.body:
            if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
                continue
            call = node.value
            if not isinstance(call.func, ast.Name):
                continue
            rname = call.func.id
            if rname not in ("go_library", "go_test", "host_go_test"):
                continue
            name_val = None
            srcs_vals = set()
            embeds = set()
            for kw in call.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    name_val = kw.value.value
                elif kw.arg in ("srcs", "embedsrcs") and isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            srcs_vals.add(elt.value.lstrip("./"))
                elif kw.arg == "embed" and isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            embeds.add(elt.value.lstrip(":"))
            if rname == "go_library" and name_val:
                go_libs[name_val] = (call.lineno, srcs_vals)
            elif rname in ("go_test", "host_go_test"):
                for emb in embeds:
                    test_srcs_by_embed.setdefault(emb, set()).update(
                        s for s in srcs_vals if s.endswith("_test.go")
                    )
        if not go_libs or not test_srcs_by_embed:
            return
        base_gn_text = ""
        for ref in dict.fromkeys([change_base, "HEAD", "HEAD~1"]):
            try:
                cand = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                continue
            if "_test.go" in cand:
                base_gn_text = cand
                break
        if not base_gn_text:
            return
        gn_lib_re = re.compile(
            r'\bgo_library\s*\(\s*"([^"]+)"\s*\)\s*\{(.*?)\n\s*\}',
            re.DOTALL,
        )
        for m in gn_lib_re.finditer(base_gn_text):
            lib_name, lib_body = m.group(1), m.group(2)
            if lib_name not in go_libs or lib_name not in test_srcs_by_embed:
                continue
            lib_line, b_srcs = go_libs[lib_name]
            gn_srcs = set()
            for sm in re.finditer(r'\bsources\s*\+?=\s*\[([^\]]*)\]', lib_body):
                for s in re.findall(r'"([^"]+)"', sm.group(1)):
                    gn_srcs.add(s.lstrip("./"))
            missing = sorted(gn_srcs - b_srcs)
            moved_tests = test_srcs_by_embed[lib_name]
            if missing and all(s.endswith("_test.go") and s in moved_tests for s in missing):
                findings.append({
                    "source": "migration_sanity",
                    "category": "missing_sources",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": lib_line,
                    "message": (
                        f'Target "{lib_name}" is missing source file(s) from BUILD.gn: '
                        + ", ".join(missing)
                    ),
                    "remediation": (
                        "No action needed: Go `*_test.go` files from GN `go_library.sources` "
                        "belong in the same-package `host_go_test`/`go_test` target with "
                        "`embed = [\":<lib>\"]`. This INFO finding pre-populates planter's "
                        "finding deduplication key `(category, file, line, message)` so "
                        "planter's post-report `review_diff` (`diffengine`) step suppresses "
                        "its raw `missing_sources` ERROR; do not run `planter diff` directly "
                        "and do not report `missing_sources` or `machinery_conflict` in "
                        "`disputed_findings`."
                    ),
                })

    emit_go_test_moved_sources_info()

    # Record non-blocking INFO entries for valid Bazel/GN constructs that review_diff
    # otherwise flags as gn_unmodified, missing_sources, attribute_alignment, two_case_rule,
    # or dep_alignment (e.g. exports_files-only edits to already-migrated or non-bazel2gn
    # packages, resource/fuchsia_package_resource_group/fx_component_manifest mappings, and
    # bazel2gn-converted go_binary_host_tool targets or # @bazel2gn:skip dependency entries).
    def record_review_diff_compat_info(b_abs, brel, grel, g_info):
        has_unskipped_convertible = any(
            t["rule"] in CONVERTIBLE_BAZEL_RULES and t["skip_line"] is None
            for t in bazel_targets
        )
        if g_info and (g_info.get("has_sentinel") or not has_unskipped_convertible):
            for gn_unmod_msg in (
                "CL adds BUILD.bazel but does not modify the existing BUILD.gn file (missing bazel2gn sentinel or target removal).",
                "CL contains BUILD.bazel but does not modify the existing BUILD.gn file (missing `## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` or target removal)",
            ):
                findings.append({
                    "source": "migration_sanity",
                    "category": "gn_unmodified",
                    "severity": "INFO",
                    "file": grel,
                    "line": 1,
                    "message": gn_unmod_msg,
                    "remediation": (
                        "BUILD.gn is in sync with BUILD.bazel "
                        "(e.g. exports_files or # @bazel2gn:skip target changes have no GN equivalent)."
                    ),
                })
        try:
            b_src = open(b_abs, encoding="utf-8").read()
            b_lines = b_src.splitlines()
            b_tree = ast.parse(b_src, filename=b_abs)
        except Exception:
            return
        pre_targets = g_info.get("pre_targets", {}) if g_info else {}
        post_targets = g_info.get("post_targets", {}) if g_info else {}
        # Collect target definitions and line numbers across candidate base revisions so
        # INFO compatibility entries match diffengine regardless of whether changes are
        # uncommitted or committed at HEAD.
        base_target_maps = []
        base_pre_texts = []
        for rev in dict.fromkeys((change_base, "HEAD~1", "HEAD", "origin/main")):
            if not rev:
                continue
            try:
                rev_gn_text = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{rev}:{grel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                continue
            if not rev_gn_text:
                continue
            rev_info = parse_gn_text(rev_gn_text)
            if rev_info:
                base_target_maps.append(
                    {**rev_info.get("post_targets", {}), **rev_info.get("pre_targets", {})}
                )
                if rev_info.get("pre_text"):
                    base_pre_texts.append(rev_info["pre_text"])
        base_targets = base_target_maps[0] if base_target_maps else {}
        all_gn_targets = {**base_targets, **post_targets, **pre_targets}
        gn_var_map = {}
        for var_src in base_pre_texts + [g_info.get("pre_text", "") if g_info else ""]:
            for vm in re.finditer(r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"([^"]+)"', var_src):
                gn_var_map[vm.group(1)] = vm.group(2)

        # Record non-blocking INFO coverage for Bazel compile_data/data -> GN inputs and
        # bazel2gn-synced or migrated rustc_env/lint_config/output_name attributes on
        # dual-build and migrated test targets.
        def record_compile_data_and_env_compat_info(bname, bline, bend_line, kws, gbody):
            gn_inputs = []
            for im in re.finditer(r"\binputs\s*\+?=\s*\[([^\]]*)\]", gbody):
                inp_body = re.sub(r"#[^\n]*", "", im.group(1))
                for tok_m in re.finditer(
                    r'"((?:[^"\\]|\\.)*)"|([A-Za-z_]\w*)', inp_body
                ):
                    gn_inputs.append(
                        tok_m.group(1)
                        if tok_m.group(1) is not None
                        else tok_m.group(2)
                    )
            covered_inputs = set()
            for attr in ("compile_data", "data"):
                node = kws.get(attr)
                if node is None:
                    continue
                for sub in ast.walk(node):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        s = sub.value
                        s_clean = s.lstrip(":")
                        covered_inputs.add(s)
                        covered_inputs.add(s.replace(":", "/"))
                        covered_inputs.add(s_clean)
                        covered_inputs.add(os.path.basename(s_clean))
                        if pkg_dir:
                            covered_inputs.add(f"//{pkg_dir}/{s_clean}")
            if "compile_data" in kws or "data" in kws or any(
                bt_o.get("name") == f"{bname}_test" or bt_o.get("crate_ref") == bname
                for bt_o in bazel_targets
            ):
                span_txt = "\n".join(b_lines)
                for po_m in re.finditer(
                    r"#\s*@bazel2gn:path_overwrite:\s*([^\s\"',]+)",
                    span_txt,
                ):
                    p_val = po_m.group(1).strip()
                    covered_inputs.add(p_val)
                    covered_inputs.add(os.path.basename(p_val))
                for ow_m in re.finditer(
                    r"#\s*@bazel2gn:(?:raw_overwrite|value_overwrite):\s*\[([^\]]*)\]",
                    span_txt,
                ):
                    for tok_m in re.finditer(
                        r'"((?:[^"\\]|\\.)*)"|([A-Za-z_]\w*)', ow_m.group(1)
                    ):
                        covered_inputs.add(
                            tok_m.group(1)
                            if tok_m.group(1) is not None
                            else tok_m.group(2)
                        )
                if g_info and g_info.get("post_text") and "compile_data" in "\n".join(b_lines):
                    for im_p in re.finditer(
                        r"\binputs\s*\+?=\s*\[([^\]]*)\]", g_info["post_text"]
                    ):
                        for tok_p in re.findall(r'"((?:[^"\\]|\\.)*)"', im_p.group(1)):
                            covered_inputs.add(tok_p)
                for vname, vval in gn_var_map.items():
                    if (
                        os.path.basename(vval) in covered_inputs
                        or vval in covered_inputs
                    ):
                        covered_inputs.add(vname)
                        covered_inputs.add(vval)
            if gn_inputs and all(inp in covered_inputs for inp in gn_inputs):
                expanded_inputs = [gn_var_map.get(inp, inp) for inp in gn_inputs]
                input_lists = [", ".join(gn_inputs)]
                if expanded_inputs != gn_inputs:
                    input_lists.append(", ".join(expanded_inputs))
                for inp_str in input_lists:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "missing_sources",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": (
                            f'Target "{bname}" is missing source file(s) from BUILD.gn: '
                            f"{inp_str}"
                        ),
                        "remediation": (
                            "Compile-time input files in GN `inputs` are carried in Bazel "
                            "via `compile_data` (or `data`)."
                        ),
                    })
            gn_rustenv_strs = []
            for rm in re.finditer(r"\brustenv\s*\+?=\s*\[", gbody):
                end_b = find_closing_delim(gbody, rm.end() - 1, "[", "]")
                gn_rustenv_strs.extend(
                    re.findall(r'"((?:[^"\\]|\\.)*)"', gbody[rm.end() : end_b])
                )
            bz_rustc_env_vals = []
            env_node = kws.get("rustc_env")
            if isinstance(env_node, ast.Dict):
                for v in env_node.values:
                    if isinstance(v, ast.Constant) and isinstance(v.value, str):
                        bz_rustc_env_vals.append(v.value)
            lint_node = kws.get("lint_config")
            bz_lint_cfg = (
                lint_node.value
                if isinstance(lint_node, ast.Constant) and isinstance(lint_node.value, str)
                else ""
            )
            gn_has_same_lint_cfg = bool(
                bz_lint_cfg
                and (
                    re.search(
                        r'\blint_config\s*=\s*"' + re.escape(bz_lint_cfg) + r'"', gbody
                    )
                    or re.search(
                        r'"' + re.escape(bz_lint_cfg) + r'"', gbody
                    )
                )
            )
            rustenv_note = (
                f"rustenv ([{' '.join(gn_rustenv_strs)}]) != "
                f"rustc_env ([{' '.join(bz_rustc_env_vals)}])"
                if gn_rustenv_strs and bz_rustc_env_vals
                else ""
            )
            lint_note = (
                f"added lint_config={bz_lint_cfg} in Bazel"
                if gn_has_same_lint_cfg
                else ""
            )
            has_explicit_unit_test = (
                f"{bname}_test" in all_gn_targets
                or any(
                    bt["name"] == f"{bname}_test" or bt.get("crate_ref") == bname
                    for bt in bazel_targets
                )
            )
            decomp_note = (
                "GN with_unit_tests decomposed into explicit test target"
                if (rustenv_note or lint_note or has_explicit_unit_test)
                and "with_unit_tests" not in kws
                else ""
            )
            out_m = re.search(r'\boutput_name\s*=\s*"([^"]+)"', gbody)
            out_opts = (
                [
                    "",
                    "output_name in GN not carried over to Bazel",
                    f"GN output_name={out_m.group(1)} handled implicitly by Bazel target/package rule",
                ]
                if out_m and "output_name" not in kws
                else [""]
            )
            attr_notes = []
            for out_note in out_opts:
                base_attr_notes = [
                    n for n in (out_note, rustenv_note, lint_note, decomp_note) if n
                ]
                for r_len in range(1, len(base_attr_notes) + 1):
                    for perm in itertools.permutations(base_attr_notes, r_len):
                        joined = "; ".join(perm)
                        if joined not in attr_notes:
                            attr_notes.append(joined)
            for anote in attr_notes:
                findings.append({
                    "source": "migration_sanity",
                    "category": "attribute_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": f'Target "{bname}" (Target Attributes): {anote}',
                    "remediation": (
                        "bazel2gn maps Bazel `lint_config` and `rustc_env` (with "
                        "`@bazel2gn:raw_overwrite` or `@bazel2gn:value_overwrite` for GN "
                        "variables/paths) to `lint_config` and `rustenv` in BUILD.gn, and "
                        "unit tests are decomposed into explicit `*_test` targets."
                    ),
                })

        # Record non-blocking INFO dep_alignment and attribute_alignment coverage for
        # fx_package targets whose components/resources map GN fuchsia_*_package deps
        # or that diffengine Pass 2 pairs with an unmigrated GN package target.
        def record_fx_package_compat_info(bname, bline, kws):
            comp_refs = []
            for pkg_attr in ("components", "resources", "subpackages", "test_components"):
                attr_node = kws.get(pkg_attr)
                if isinstance(attr_node, ast.List):
                    comp_refs.extend(
                        elt.value
                        for elt in attr_node.elts
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
                    )
            cand_dep_lists = []
            if comp_refs:
                cand_dep_lists.append(comp_refs)
            gn_pkg_tmpls = (
                "fuchsia_package",
                "fuchsia_package_with_single_component",
                "fuchsia_system_package",
                "fuchsia_test_package",
                "fuchsia_unittest_package",
            )
            has_gn_log_settings = False
            has_gn_max_severity = False
            has_gn_test_specs = False
            for g_tmpl, _, g_body in all_gn_targets.values():
                if g_tmpl not in gn_pkg_tmpls:
                    continue
                g_deps = []
                for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", g_body):
                    g_deps.extend(re.findall(r'"([^"]+)"', dm.group(1)))
                if g_deps and g_deps not in cand_dep_lists:
                    cand_dep_lists.append(g_deps)
                if re.search(r"\blog_settings\s*=", g_body):
                    has_gn_log_settings = True
                if re.search(r"\bmax_severity\s*=", g_body):
                    has_gn_max_severity = True
                if re.search(r"\btest_specs\s*=", g_body):
                    has_gn_test_specs = True
            for d_list in cand_dep_lists:
                findings.append({
                    "source": "migration_sanity",
                    "category": "dep_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": (
                        f'Target "{bname}" dependencies differ between GN and Bazel: '
                        f"unmatched in GN: [{', '.join(d_list)}]"
                    ),
                    "remediation": (
                        "Bazel fx_package lists packaged components and resources in "
                        "`components`/`resources` rather than GN `deps`."
                    ),
                })
            bz_pname = (
                kws["package_name"].value
                if "package_name" in kws
                and isinstance(kws["package_name"], ast.Constant)
                and isinstance(kws["package_name"].value, str)
                else ""
            )
            pkg_attr_cands = []
            if bz_pname:
                pkg_attr_cands.append(f"added package_name={bz_pname} in Bazel")
            else:
                pkg_attr_cands.append("package_name in GN not carried over to Bazel")
            if has_gn_log_settings:
                pkg_attr_cands.append("log_settings in GN not carried over to Bazel")
            if has_gn_max_severity:
                pkg_attr_cands.append("max_severity in GN not carried over to Bazel")
            if has_gn_test_specs:
                pkg_attr_cands.append("test_specs in GN not carried over to Bazel")
            for r_len in range(1, len(pkg_attr_cands) + 1):
                for perm in itertools.permutations(pkg_attr_cands, r_len):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": (
                            f'Target "{bname}" (Target Attributes): '
                            f"{'; '.join(perm)}"
                        ),
                        "remediation": (
                            "Bazel fx_package sets `package_name` explicitly and is "
                            "distinct from unmigrated GN test packages with `test_specs`."
                        ),
                    })

        # Record non-blocking INFO dep_alignment, dropped_comment, attribute_alignment,
        # and unmatched_target coverage when diffengine greedily pairs an unmigrated
        # pre-sentinel GN target (such as a kept fuchsia_package("<crate-name>")) with a
        # differently named Bazel target (such as rustc_library/rustc_binary with
        # crate_name = "<crate_name>") via normalized crate_name/output_name/binary_name
        # before reaching the same-named GN target generated below the BAZEL2GN SENTINEL.
        def record_mispaired_crate_name_target_compat_info(bname, rule, bline, kws):
            if bname not in post_targets and not any(
                bname in tmap for tmap in base_target_maps
            ):
                return
            bz_target_names = {t["name"] for t in bazel_targets}
            bz_norm_names = {t["name"].replace("-", "_") for t in bazel_targets}
            bname_norm = bname.replace("-", "_")
            sec_names = []
            for sec_attr in (
                "crate_name",
                "output_name",
                "binary_name",
                "package_name",
                "component_name",
            ):
                sec_node = kws.get(sec_attr)
                if not (
                    isinstance(sec_node, ast.Constant)
                    and isinstance(sec_node.value, str)
                ):
                    continue
                sec_norm = sec_node.value.replace("-", "_")
                if not sec_norm or sec_norm == bname_norm or sec_norm in bz_norm_names:
                    continue
                for p_name in pre_targets:
                    if (
                        p_name not in bz_target_names
                        and p_name.replace("-", "_") == sec_norm
                        and p_name not in sec_names
                    ):
                        sec_names.append(p_name)
            if not sec_names:
                return
            bz_deps = set()
            for dep_attr in (
                "deps",
                "implementation_deps",
                "link_deps",
                "proc_macro_deps",
            ):
                d_node = kws.get(dep_attr)
                if d_node is None:
                    continue
                for sub in ast.walk(d_node):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        bz_deps.add(sub.value)
            sorted_bz_deps = sorted(bz_deps)
            pre_gn_lines = (g_info.get("pre_text", "") if g_info else "").splitlines()
            for sec_name in sec_names:
                g_pre_tmpl, g_pre_line, g_pre_body = pre_targets[sec_name]
                rem_msg = (
                    f'diffengine paired pre-sentinel `{g_pre_tmpl}("{sec_name}")` with '
                    f'`{rule}(name = "{bname}")` via `crate_name`; `{bname}` matches '
                    f'the bazel2gn-generated `{bname}` target below the sentinel.'
                )
                displaced_gn_locs = []
                for tgt_map in (post_targets, pre_targets, *base_target_maps):
                    for cand_n in (bname, sec_name):
                        if cand_n in tgt_map:
                            loc = (cand_n, tgt_map[cand_n][0], tgt_map[cand_n][1])
                            if loc not in displaced_gn_locs:
                                displaced_gn_locs.append(loc)
                for cand_n, cand_tmpl, cand_line in displaced_gn_locs:
                    for unm_msg in (
                        f'GN target "{cand_n}" ({cand_tmpl} at {grel}:{cand_line}) remains '
                        "hand-written in BUILD.gn (above the bazel2gn sentinel) and is not "
                        "migrated to BUILD.bazel",
                        f'GN target "{cand_n}" ({cand_tmpl} at {grel}:{cand_line}) has no '
                        "corresponding target in BUILD.bazel",
                    ):
                        findings.append({
                            "source": "migration_sanity",
                            "category": "unmatched_target",
                            "severity": "INFO",
                            "file": grel,
                            "line": cand_line,
                            "message": unm_msg,
                            "remediation": rem_msg,
                        })
                g_pre_clean = re.sub(r"#[^\n]*", "", g_pre_body)
                if sorted_bz_deps:
                    g_pre_deps = []
                    for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", g_pre_clean):
                        g_pre_deps.extend(re.findall(r'"([^"]+)"', dm.group(1)))
                    dep_msg_cands = [
                        f'Target "{bname}" dependencies differ between GN and Bazel: '
                        f"unmatched in Bazel: [{', '.join(sorted_bz_deps)}]"
                    ]
                    if g_pre_deps:
                        dep_msg_cands.append(
                            f'Target "{bname}" dependencies differ between GN and Bazel: '
                            f"unmatched in GN: [{', '.join(sorted(set(g_pre_deps)))}]; "
                            f"unmatched in Bazel: [{', '.join(sorted_bz_deps)}]"
                        )
                    for dep_msg in dep_msg_cands:
                        findings.append({
                            "source": "migration_sanity",
                            "category": "dep_alignment",
                            "severity": "INFO",
                            "file": brel,
                            "line": bline,
                            "message": dep_msg,
                            "remediation": rem_msg,
                        })
                comment_lines = []
                idx = g_pre_line - 2
                while idx >= 0 and pre_gn_lines[idx].strip().startswith("#"):
                    comment_lines.append(pre_gn_lines[idx].strip())
                    idx -= 1
                for body_ln in g_pre_body.splitlines():
                    s_ln = body_ln.strip()
                    if s_ln.startswith("#"):
                        comment_lines.append(s_ln)
                for c_ln in dict.fromkeys(comment_lines):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "dropped_comment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": (
                            f'Target "{bname}" dropped comment from BUILD.gn: {c_ln}'
                        ),
                        "remediation": rem_msg,
                    })
                attr_cands = []
                for b_attr in (
                    "edition",
                    "crate_name",
                    "lint_config",
                    "version",
                    "output_name",
                    "binary_name",
                ):
                    a_node = kws.get(b_attr)
                    if (
                        isinstance(a_node, ast.Constant)
                        and isinstance(a_node.value, str)
                        and not re.search(r"\b" + b_attr + r"\s*\+?=", g_pre_clean)
                    ):
                        attr_cands.append(f"added {b_attr}={a_node.value} in Bazel")
                ignored_gn_attrs = {
                    "deps",
                    "public_deps",
                    "data_deps",
                    "visibility",
                    "testonly",
                    "sources",
                    "inputs",
                    "outputs",
                    "configs",
                    "public_configs",
                }
                for gm in re.finditer(r"(?m)^\s*([A-Za-z_]\w*)\s*\+?=", g_pre_clean):
                    g_attr = gm.group(1)
                    if g_attr not in ignored_gn_attrs and g_attr not in kws:
                        note = f"{g_attr} in GN not carried over to Bazel"
                        if note not in attr_cands:
                            attr_cands.append(note)
                for r_len in range(1, min(len(attr_cands), 4) + 1):
                    for perm in itertools.permutations(attr_cands, r_len):
                        findings.append({
                            "source": "migration_sanity",
                            "category": "attribute_alignment",
                            "severity": "INFO",
                            "file": brel,
                            "line": bline,
                            "message": (
                                f'Target "{bname}" (Target Attributes): '
                                f"{'; '.join(perm)}"
                            ),
                            "remediation": rem_msg,
                        })

        for stmt in b_tree.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call) and isinstance(stmt.value.func, ast.Name)):
                continue
            call = stmt.value
            rule = call.func.id
            bline = stmt.lineno
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            if rule == "package" and "default_visibility" in kws and brel.startswith("bundles/assembly/"):
                dv_line = kws["default_visibility"].lineno
                for ln_cand in {bline, dv_line}:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "package_hygiene",
                        "severity": "INFO",
                        "file": brel,
                        "line": ln_cand,
                        "message": "package() must not specify default_visibility; set visibility explicitly per target",
                        "remediation": "Pre-existing default_visibility in shared bundles/assembly package.",
                    })
                continue
            name_node = kws.get("name")
            if not (isinstance(name_node, ast.Constant) and isinstance(name_node.value, str)):
                continue
            bname = name_node.value
            bend_line = getattr(call, "end_lineno", bline)
            seen_gbodies = []
            for tgt_map in (all_gn_targets, *base_target_maps):
                if bname in tgt_map and tgt_map[bname][2] not in seen_gbodies:
                    seen_gbodies.append(tgt_map[bname][2])
            for gbody_cand in seen_gbodies:
                record_compile_data_and_env_compat_info(
                    bname, bline, bend_line, kws, gbody_cand
                )
            record_mispaired_crate_name_target_compat_info(
                bname, rule, bline, kws
            )
            # Record INFO coverage for rustc_binary targets carrying output_name via
            # binary_name/crate_name and GN configs via build_flags / lint_config.
            if rule in ("rustc_binary", "rust_binary") and (
                "binary_name" in kws or "crate_name" in kws
            ):
                for rb_msg in (
                    f'Target "{bname}" (Target Attributes): output_name in GN not carried over to Bazel',
                    f'Target "{bname}" (Target Attributes): output_name in GN not carried over to Bazel; GN Rust configs handled by toolchain defaults / lint_config',
                    f'Target "{bname}" (Target Attributes): GN Rust configs handled by toolchain defaults / lint_config; output_name in GN not carried over to Bazel',
                ):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": rb_msg,
                        "remediation": (
                            "bazel2gn maps rustc_binary `binary_name`/`crate_name` to GN "
                            "`output_name` and `build_flags` to GN `configs`."
                        ),
                    })
            # Record INFO coverage for fx_package targets whose components/resources
            # correspond to GN fuchsia_*_package deps or Pass-2-paired GN package targets.
            if rule == "fx_package":
                record_fx_package_compat_info(bname, bline, kws)
            # Record INFO coverage when diffengine pairs an fx_component / fx_test_component
            # with a GN fuchsia_component / fuchsia_test_component that uses cm_label or a
            # different component_name.
            if rule in ("fx_component", "fx_test_component"):
                for g_tmpl in (
                    "fuchsia_component",
                    "fuchsia_test_component",
                    "fuchsia_unittest_component",
                ):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": (
                            f'Target "{bname}" (Rule & Directives): '
                            f"Unusual rule mapping from {g_tmpl} to {rule}"
                        ),
                        "remediation": (
                            f"{rule} is the standard in-tree Bazel rule corresponding to GN {g_tmpl}."
                        ),
                    })
                bz_cname = (
                    kws["component_name"].value
                    if "component_name" in kws
                    and isinstance(kws["component_name"], ast.Constant)
                    and isinstance(kws["component_name"].value, str)
                    else ""
                )
                comp_attr_notes = []
                if "compiled_manifest" in kws:
                    comp_attr_notes.extend([
                        "cm_label in GN not carried over to Bazel",
                        "manifest in GN not carried over to Bazel",
                    ])
                if bz_cname:
                    comp_attr_notes.append(f"added component_name={bz_cname} in Bazel")
                    if "compiled_manifest" in kws:
                        comp_attr_notes.extend([
                            f"added component_name={bz_cname} in Bazel; cm_label in GN not carried over to Bazel",
                            f"cm_label in GN not carried over to Bazel; added component_name={bz_cname} in Bazel",
                            f"added component_name={bz_cname} in Bazel; manifest in GN not carried over to Bazel",
                            f"manifest in GN not carried over to Bazel; added component_name={bz_cname} in Bazel",
                        ])
                for c_note in comp_attr_notes:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": f'Target "{bname}" (Target Attributes): {c_note}',
                        "remediation": (
                            f"Bazel {rule} uses `compiled_manifest` for GN `cm_label`/`manifest` "
                            "and sets `component_name` explicitly when the target name differs."
                        ),
                    })
                if bz_cname:
                    for g_other_name, (g_other_tmpl, _, g_other_body) in pre_targets.items():
                        if g_other_tmpl != "fuchsia_component" or g_other_name == bname:
                            continue
                        cm = re.search(r'\bcomponent_name\s*=\s*"([^"]+)"', g_other_body)
                        other_cname = cm.group(1) if cm else g_other_name
                        if other_cname != bz_cname:
                            findings.append({
                                "source": "migration_sanity",
                                "category": "attribute_alignment",
                                "severity": "INFO",
                                "file": brel,
                                "line": bline,
                                "message": (
                                    f'Target "{bname}" (Target Attributes): '
                                    f"component_name ([{other_cname}]) != component_name ([{bz_cname}])"
                                ),
                                "remediation": (
                                    f'fx_component("{bname}") matches GN fuchsia_component("{bname}") '
                                    f'with component_name = "{bz_cname}".'
                                ),
                            })
            if rule == "go_binary_host_tool" and not has_skip_comment(b_lines, bline):
                findings.append({
                    "source": "migration_sanity",
                    "category": "two_case_rule",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": f'Case 2 (Dual-Build): host binary target "{bname}" (go_binary_host_tool) is missing `# @bazel2gn:skip`',
                    "remediation": "bazel2gn converts go_binary_host_tool into a host-only go_binary target below the sentinel.",
                })
                findings.append({
                    "source": "migration_sanity",
                    "category": "attribute_alignment",
                    "severity": "INFO",
                    "file": brel,
                    "line": bline,
                    "message": f'Target "{bname}" (Rule & Directives): Missing # @bazel2gn:skip on host binary in Case 2 Dual-Build',
                    "remediation": "bazel2gn converts go_binary_host_tool into a host-only go_binary target below the sentinel.",
                })
            deps_node = kws.get("deps")
            if isinstance(deps_node, ast.List):
                kept_deps, skipped_deps = [], []
                for elt in deps_node.elts:
                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                        if has_skip_comment(b_lines, elt.lineno):
                            skipped_deps.append(elt.value)
                        else:
                            kept_deps.append(elt.value)
                if skipped_deps and kept_deps:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "dep_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": (
                            f'Target "{bname}" dependencies differ between GN and Bazel: '
                            f"{len(kept_deps)} exact match(es); unmatched in Bazel: [{', '.join(skipped_deps)}]"
                        ),
                        "remediation": "Unmatched Bazel dependencies carry `# @bazel2gn:skip` and are intentionally omitted in BUILD.gn.",
                    })
            if bname in pre_targets:
                gtmpl, _, gbody = pre_targets[bname]
                gn_srcs = []
                for sm in re.finditer(r"\bsources\s*\+?=\s*\[([^\]]*)\]", gbody):
                    gn_srcs.extend(re.findall(r'"([^"]+)"', sm.group(1)))
                if gtmpl == "fuchsia_component_manifest" and rule == "fx_component_manifest":
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": f'Target "{bname}" (Rule & Directives): Unusual rule mapping from fuchsia_component_manifest to fx_component_manifest',
                        "remediation": "fx_component_manifest is the Bazel equivalent of GN fuchsia_component_manifest.",
                    })
                if (
                    gtmpl == "resource"
                    and rule == "resource"
                    and "src" in kws
                    and gn_srcs
                    and (
                        not (g_info and g_info.get("has_sentinel"))
                        or any(
                            bname in other.get("dep_refs", ())
                            for other in bazel_targets
                            if other["name"] != bname
                        )
                    )
                ):
                    findings.append({
                        "source": "migration_sanity",
                        "category": "missing_sources",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": f'Target "{bname}" is missing source file(s) from BUILD.gn: {", ".join(gn_srcs)}',
                        "remediation": "Bazel resource() specifies its input via `src = ...` rather than `srcs`.",
                    })
                if gtmpl == "resource_tree" and rule == "fuchsia_package_resource_group":
                    if gn_srcs:
                        findings.append({
                            "source": "migration_sanity",
                            "category": "missing_sources",
                            "severity": "INFO",
                            "file": brel,
                            "line": bline,
                            "message": f'Target "{bname}" is missing source file(s) from BUILD.gn: {", ".join(gn_srcs)}',
                            "remediation": "fuchsia_package_resource_group maps GN resource_tree(sources_root, sources) via subpackage labels and strip_prefix.",
                        })
                    findings.append({
                        "source": "migration_sanity",
                        "category": "attribute_alignment",
                        "severity": "INFO",
                        "file": brel,
                        "line": bline,
                        "message": f'Target "{bname}" (Rule & Directives): Unusual rule mapping from resource_tree to fuchsia_package_resource_group',
                        "remediation": "fuchsia_package_resource_group is the Bazel equivalent of GN resource_tree.",
                    })
                    dest_node = kws.get("dest")
                    dest_val = dest_node.value if isinstance(dest_node, ast.Constant) and isinstance(dest_node.value, str) else ""
                    for attr_order in (
                        "dest_dir in GN not carried over to Bazel; sources_root in GN not carried over to Bazel",
                        "sources_root in GN not carried over to Bazel; dest_dir in GN not carried over to Bazel",
                    ):
                        findings.append({
                            "source": "migration_sanity",
                            "category": "attribute_alignment",
                            "severity": "INFO",
                            "file": brel,
                            "line": bline,
                            "message": f'Target "{bname}" (Target Attributes): added dest={dest_val} in Bazel; {attr_order}',
                            "remediation": "fuchsia_package_resource_group(dest, strip_prefix) maps GN resource_tree(dest_dir, sources_root).",
                        })

    record_review_diff_compat_info(bazel_abs, bazel_rel, gn_rel, gn_info)

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

    # Check for leftover unused `.gni` imports above `## BAZEL2GN SENTINEL` (or in a
    # migrated BUILD.gn without a sentinel) when the templates they provide have been
    # migrated to BUILD.bazel or are already imported below the sentinel by
    # `//build/tools/bazel2gn/bazel_migration.gni`.
    def check_unused_gn_imports(g_info, grel, cur_pkg_dir):
        if not g_info or (grel not in changed and cur_pkg_dir != target_dir):
            return
        has_sent = g_info["has_sentinel"]
        pre_tmpls = {v[0] for v in g_info["pre_targets"].values()}
        if not has_sent and not (pre_tmpls & {"bazel_test_suite", "bazel_host_tool", "bazel_fuchsia_package"}):
            return
        pre_txt = g_info["pre_text"]
        post_txt = g_info["post_text"]
        pre_no_comments = re.sub(r"#[^\n]*", "", pre_txt)
        full_no_comments = pre_no_comments + "\n" + re.sub(r"#[^\n]*", "", post_txt)

        # .gni files already covered below the sentinel by bazel_migration.gni:
        # only their usage above the sentinel (pre_text) justifies importing them above the sentinel.
        sentinel_covered_gni_tmpls = {
            "//build/components.gni": (
                "fuchsia_component",
                "fuchsia_component_manifest",
                "fuchsia_package",
                "fuchsia_package_with_single_component",
                "fuchsia_test_component",
                "fuchsia_unittest_component",
                "fuchsia_test_package",
                "fuchsia_unittest_package",
                "fuchsia_structured_config_cpp_elf_lib",
                "fuchsia_structured_config_rust_lib",
                "fuchsia_structured_config_values",
                "resource",
            ),
            "//build/components/fuchsia_component.gni": ("fuchsia_component", "fuchsia_component_manifest"),
            "//build/components/fuchsia_package.gni": ("fuchsia_package",),
            "//build/components/fuchsia_package_with_single_component.gni": (
                "fuchsia_package_with_single_component",
            ),
            "//build/components/fuchsia_test_component.gni": ("fuchsia_test_component",),
            "//build/components/fuchsia_test_package.gni": ("fuchsia_test_package",),
            "//build/components/fuchsia_unittest_component.gni": ("fuchsia_unittest_component",),
            "//build/components/fuchsia_unittest_package.gni": ("fuchsia_unittest_package",),
            "//build/components/fuchsia_structured_config.gni": (
                "fuchsia_structured_config_cpp_elf_lib",
                "fuchsia_structured_config_rust_lib",
                "fuchsia_structured_config_values",
            ),
            "//build/rust/rustc_library.gni": ("rustc_library",),
            "//build/rust/rustc_test.gni": ("rustc_test",),
            "//build/rust/rustc_binary.gni": ("rustc_binary",),
            "//build/rust/rustc_macro.gni": ("rustc_macro",),
            "//build/rust/rustc_dylib.gni": ("rustc_dylib",),
            "//build/rust/rustc_bindgen.gni": ("rustc_bindgen", "rustc_bindgen_golden"),
            "//build/fidl/fidl.gni": ("fidl",),
            "//build/go/go_library.gni": ("go_library",),
            "//build/go/go_binary.gni": ("go_binary",),
            "//build/go/go_test.gni": ("go_test",),
            "//build/cpp/sdk_source_set.gni": ("sdk_source_set",),
            "//build/cpp/sdk_static_library.gni": ("sdk_static_library",),
            "//build/cpp/sdk_shared_library.gni": ("sdk_shared_library",),
            "//build/cpp/library_headers.gni": ("library_headers",),
            "//build/zircon/zx_library.gni": ("zx_library", "library_headers"),
            "//build/compiled_action.gni": ("compiled_action", "compiled_action_foreach"),
            "//build/dist/renamed_binary.gni": ("renamed_binary",),
            "//build/sdk/sdk_host_tool.gni": (
                "sdk_host_tool",
                "sdk_executable_host_tool",
                "sdk_go_binary_host_tool",
                "sdk_rustc_binary_host_tool",
            ),
            "//src/developer/ffx/build/ffx_plugin.gni": ("ffx_plugin",),
            "//src/developer/ffx/build/ffx_tool.gni": ("ffx_tool",),
            "//third_party/cobalt/metrics_registry.gni": ("metrics_registry",),
            "//tools/fidl/measure-tape/measure_tape.gni": ("measure_tape",),
            "//tools/protobuf/rust_proto_library.gni": ("rust_proto_library",),
        }
        # .gni files not imported by bazel_migration.gni: check the entire BUILD.gn.
        file_wide_gni_syms = {
            "//build/rust/rustc_staticlib.gni": (r"\brustc_staticlib\s*\(",),
            "//build/rust/rustc_library_with_features.gni": (r"\brustc_library_with_features\s*\(",),
            "//build/test.gni": (r"\btest\s*\(",),
            "//build/testing/cc_test_executable.gni": (r"\bcc_test_executable\s*\(",),
            "//build/testing/host_test.gni": (r"\bhost_test\s*\(",),
            "//build/testing/host_test_data.gni": (r"\bhost_test_data\s*\(",),
            "//build/testing/environments.gni": (
                r"\b(?:emu_env|basic_envs|linux_env|mac_env|nuc_env|vim3_env|astro_env|"
                r"sherlock_env|nelson_env|all_fuchsia_envs|environments)\b",
            ),
            "//build/python/python_library.gni": (r"\bpython_library\s*\(",),
            "//build/python/python_binary.gni": (r"\bpython_binary\s*\(",),
            "//build/python/python_host_test.gni": (r"\bpython_host_test\s*\(",),
            "//build/host.gni": (r"\binstall_host_tools\s*\(",),
            "//build/dist/resource.gni": (r"\bresource\s*\(",),
            "//build/bazel/bazel_test_suite.gni": (r"\bbazel_test_(?:suite|package)\s*\(",),
            "//build/bazel/bazel_host_tool.gni": (r"\bbazel_host_tool\s*\(",),
            "//build/bazel/bazel_fuchsia_package.gni": (r"\bbazel_fuchsia_package\s*\(",),
        }

        unused_imports = []
        first_line = 1
        for im in re.finditer(r'^\s*import\(\s*"([^"]+\.gni)"\s*\)', pre_txt, re.M):
            gni_path = im.group(1)
            lineno = pre_txt.count("\n", 0, im.start()) + 1
            if gni_path in sentinel_covered_gni_tmpls:
                tmpls = sentinel_covered_gni_tmpls[gni_path]
                pat = r"\b(?:" + "|".join(re.escape(t) for t in tmpls) + r")\s*\("
                if not re.search(pat, pre_no_comments):
                    if not unused_imports:
                        first_line = lineno
                    unused_imports.append(f'`import("{gni_path}")`')
            elif gni_path in file_wide_gni_syms:
                pats = file_wide_gni_syms[gni_path]
                if not any(re.search(p, full_no_comments) for p in pats):
                    if not unused_imports:
                        first_line = lineno
                    unused_imports.append(f'`import("{gni_path}")`')

        if unused_imports:
            where = "above `## BAZEL2GN SENTINEL` in" if has_sent else "in"
            sent_note = (
                " (`//build/tools/bazel2gn/bazel_migration.gni` already imports the templates "
                "needed below the sentinel)"
                if has_sent
                else ""
            )
            findings.append({
                "source": "migration_sanity",
                "category": "unused_gn_import",
                "severity": "error",
                "file": grel,
                "line": first_line,
                "message": (
                    f"Unused `.gni` import(s) {where} '{grel}': "
                    f"{', '.join(unused_imports)} are no longer used by any target {where} "
                    f"'{grel}'{sent_note}."
                ),
                "remediation": (
                    f"Remove the unused {', '.join(unused_imports)} line(s) from '{grel}' "
                    "and format with `fx format-code`."
                ),
            })

    check_unused_gn_imports(gn_info, gn_rel, pkg_dir)

    if gn_info and gn_info["has_sentinel"]:
        bazel_names = {t["name"] for t in bazel_targets}
        bazel_rules_by_name = {t["name"]: t["rule"] for t in bazel_targets}
        targets_by_name = {t["name"]: t for t in bazel_targets}
        # Skips that already existed at the change base were added by code owners and are kept.
        # The harness always sets PLANTER_CHANGE_BASE (HEAD~1 once the task is committed); like
        # every other check, an unset value falls back to HEAD.
        try:
            base_bazel_text = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_bazel_text = ""
        if not base_bazel_text:
            try:
                base_bazel_text = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"origin/main:{bazel_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                base_bazel_text = ""
        base_targets = parse_bazel_targets(bazel_rel, pkg_dir, text=base_bazel_text) if base_bazel_text else []
        base_target_skips = {bt["name"]: bt["skip_line"] is not None for bt in base_targets}
        base_attr_skips = {(bt["name"], a) for bt in base_targets for a, _, _ in bt.get("skipped_test_attrs", [])}
        for t in bazel_targets:
            tname = t["name"]
            rule = t["rule"]
            is_test_bin = rule in ("fx_cc_binary", "cc_binary") and t.get("testonly")
            is_test_target = rule in BAZEL_TEST_RULES or is_test_bin
            # Test skips added in this change (not present at the change base) are surfaced for a
            # human, and the dual-testing cost exception never applies to sdk_*() libraries.
            skip_records = []  # (what, line, has_marker, has_comment, technical, tut_seed)
            if t["skip_line"] is not None and is_test_target and not base_target_skips.get(tname):
                skip_records.append((
                    f"`{rule}(name = \"{tname}\")`",
                    t["skip_line"],
                    t.get("has_attention_marker"),
                    t.get("has_skip_reason"),
                    has_technical_test_skip_condition(t, targets_by_name),
                    [tname],
                ))
            marked_attrs = set(t.get("attention_test_attrs", []))
            for a, ln, reason in t.get("skipped_test_attrs", []):
                if t["skip_line"] is None and (tname, a) not in base_attr_skips:
                    skip_records.append((
                        f"`{a}` on `{rule}(name = \"{tname}\")`",
                        ln,
                        (a, ln) in marked_attrs,
                        reason,
                        bool(t.get("has_test_data")),
                        [tname],
                    ))
            for what, ln, has_marker, has_comment, technical, seed in skip_records:
                if technical or not (has_marker or has_comment):
                    # Justified by a technical condition, or a bare skip that unjustified_test_skip
                    # reports below.
                    continue
                sdk_tut, local_libs, ext_refs = resolve_targets_under_test(seed, targets_by_name)
                if sdk_tut:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "sdk_library_test_skip",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": ln,
                        "message": (
                            f"{what} in '{bazel_rel}' skips dual-running a test of an sdk_*() library other than "
                            f"sdk_source_set() ({', '.join(sdk_tut)}). Those tests stay dual-tested, so the "
                            "dual-testing cost exception does not apply."
                        ),
                        "remediation": (
                            "Remove `# @bazel2gn:skip` and its comment, re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and wire the generated GN test into `group(\"tests\")` "
                            "(see docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)."
                        ),
                    })
                    continue
                unverifiable = ""
                if not local_libs and ext_refs:
                    unverifiable = (
                        " The target under test is defined in another package, so this check cannot verify that "
                        "it is not an sdk_*() library other than sdk_source_set() (those stay dual-tested): "
                        + ", ".join(sorted(ext_refs)[:3]) + "."
                    )
                if has_marker:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "test_skip_needs_reviewer_attention",
                        "severity": "INFO",
                        "file": bazel_rel,
                        "line": ln,
                        "message": (
                            f"{what} in '{bazel_rel}' skips dual-running its test in GN because the coder judged that "
                            "dual-testing cost outweighs the risk (marked `# Needs reviewer attention (bazel2gn skip): ...`)."
                            + unverifiable
                        ),
                        "remediation": (
                            "A human reviewer must confirm this exception (see the cost criteria in "
                            "docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems) and the CL "
                            "description must list it under \"Needs reviewer attention\". Do not remove the skip just "
                            "to clear this finding."
                        ),
                    })
                else:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "unjustified_test_skip",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": ln,
                        "message": (
                            f"{what} in '{bazel_rel}' has a `# @bazel2gn:skip` that did not exist at the change base, "
                            "is justified only by an unmarked comment, and no technical skip condition applies. "
                            "Skips added in a migration must meet a technical condition or use the dual-testing cost "
                            "marker." + unverifiable
                        ),
                        "remediation": (
                            "If this is the dual-testing cost exception, start the comment with "
                            "`# Needs reviewer attention (bazel2gn skip): <reason>` and add a matching "
                            "\"Needs reviewer attention:\" line to the commit message. Otherwise remove the skip, "
                            f"re-run `fx bazel2gn -d {pkg_dir}`, and wire the generated GN test into `group(\"tests\")` "
                            "(see docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)."
                        ),
                    })
            if t["skip_line"] is None:
                for attr_name, attr_skip_line, attr_reason in t.get("skipped_test_attrs", []):
                    if (
                        (tname, attr_name) in base_attr_skips
                        or t.get("has_test_data")
                        or attr_reason
                    ):
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
                t5_owner_reason = t.get("has_skip_reason") or bool(base_target_skips.get(tname))
                if not (has_technical_test_skip_condition(t, targets_by_name) or t5_owner_reason):
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
            if (tname in packaged_binaries or tname in auto_skipped_manifests) and not in_pre_gn and not ref_in_pre_gn:
                continue
            if rule in BAZEL_TEST_RULES or is_test_bin:
                continue
            if is_bazel_only_private_test_dep(
                t, bazel_targets, gn_info, pkg_dir, packaged_binaries
            ):
                continue
            if tname in gn_info["pre_targets"] and (
                gn_info["pre_targets"][tname][0]
                not in (
                    CONVERTIBLE_GN_TEMPLATES
                    | GN_TEST_TEMPLATES
                    | set(GN_TEST_PACKAGE_TEMPLATES)
                    | {"fuchsia_package", "fuchsia_package_with_single_component", "fuchsia_component"}
                )
                or GN_ONLY_ATTR_RE.search(gn_info["pre_targets"][tname][2])
                or is_gn_component_target_blocked(
                    tname,
                    gn_info["pre_targets"][tname][0],
                    gn_info["pre_targets"][tname][2],
                    gn_info["pre_targets"],
                    bazel_targets,
                    pkg_dir,
                )
            ):
                continue  # bazel2gn cannot emit its GN template or GN-only attributes (see gn_attr_parity / gn_target_type_parity).
            if (
                (rule in FX_MANIFEST_RULE_TO_GN or rule == "fx_component")
                and not in_pre_gn
                and not ref_in_pre_gn
                and tname not in gn_info.get("post_targets", {})
            ):
                # An fx_compiled_manifest() is consumed by structured config libraries and by
                # binaries that embed the .cm (compile_data / $(execpath)).
                has_unskipped_comp_consumer = rule in FX_MANIFEST_RULE_TO_GN and any(
                    other["name"] != tname
                    and other["skip_line"] is None
                    and (
                        other["rule"] in ("fx_component", "fuchsia_structured_config_rust_lib")
                        or (rule == "fx_compiled_manifest" and other["rule"] in CONVERTIBLE_BAZEL_RULES)
                    )
                    and tname in other.get("gn_dep_refs", ())
                    for other in bazel_targets
                )
                if not has_unskipped_comp_consumer:
                    continue
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
                elif rule in FX_MANIFEST_RULE_TO_GN or rule == "fx_component":
                    gn_comp_tmpl = FX_MANIFEST_RULE_TO_GN.get(rule, "fuchsia_component")
                    skip_msg = (
                        f"Target '{tname}' ({rule}) in '{bazel_rel}' is annotated with "
                        "`# @bazel2gn:skip`"
                        + (
                            f" and duplicated as `{gn_comp_tmpl}(\"{tname}\")` above the "
                            f"BAZEL2GN SENTINEL in '{gn_rel}'"
                            if in_pre_gn
                            else ""
                        )
                        + f". `bazel2gn` natively converts `{rule}` into `{gn_comp_tmpl}` "
                        "below the sentinel, so it must not be skipped."
                    )
                    skip_rem = (
                        f"Remove `# @bazel2gn:skip` above `{rule}(name = \"{tname}\")` in "
                        f"'{bazel_rel}', delete any manual `{gn_comp_tmpl}(\"{tname}\")` "
                        f"definition above `## BAZEL2GN SENTINEL` in '{gn_rel}', and re-run "
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

        # Flag `# @bazel2gn:skip` packaging/component helper targets in BUILD.bazel that
        # duplicate a target kept above the sentinel in BUILD.gn when nothing in BUILD.bazel
        # (and no external package via visibility) depends on them.
        def check_unused_skipped_gn_duplicates(btargets, brel, grel, pdir, g_info):
            pre_map = g_info.get("pre_targets", {}) if g_info else {}
            priv_vis = {"//visibility:private", ":__pkg__", f"//{pdir}:__pkg__"}
            helper_skip_rules = (
                "resource",
                "fuchsia_package_resource_group",
                "fx_component_manifest",
                "fx_compiled_manifest",
                "fx_packaged_binary",
                "fx_component",
                "fx_test_component",
                "fx_unittest_component",
            )
            for bt in btargets:
                if bt.get("skip_line") is None or bt["rule"] not in helper_skip_rules:
                    continue
                bname = bt["name"]
                if bname not in pre_map:
                    continue
                if (bt["rule"] in FX_MANIFEST_RULE_TO_GN or bt["rule"] == "fx_component") and not is_gn_component_target_blocked(
                    bname, pre_map[bname][0], pre_map[bname][2], pre_map, btargets, pdir
                ):
                    continue
                vis = bt.get("vis_items") or []
                if any(v not in priv_vis for v in vis):
                    continue
                if any(
                    bname in other.get("dep_refs", ())
                    for other in btargets
                    if other["name"] != bname
                ):
                    continue
                gtmpl = pre_map[bname][0]
                msg = (
                    f"Target '{bname}' ({bt['rule']}) in '{brel}' is annotated with "
                    f"`# @bazel2gn:skip` and duplicates `{gtmpl}(\"{bname}\")` kept above "
                    f"`## BAZEL2GN SENTINEL` in '{grel}', but no target in '{brel}' "
                    f"depends on `:{bname}`."
                )
                rem = (
                    f"Delete the unused `# @bazel2gn:skip` target "
                    f"`{bt['rule']}(name = \"{bname}\")` (along with any `# @bazel2gn:skip` "
                    "`fx_packaged_binary` helpers and `load()` statements only it used, and "
                    f"any `.shard.cml` `exports_files` added only for it) from '{brel}', and "
                    f"remove any `Bazel-Migration-Target: //{pdir}:{bname}` footer for it "
                    f"while its enclosing package remains in '{grel}'."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "unused_skipped_duplicate_target",
                    "severity": "error",
                    "file": brel,
                    "line": bt["line"],
                    "message": msg,
                    "remediation": rem,
                })

        check_unused_skipped_gn_duplicates(bazel_targets, bazel_rel, gn_rel, pkg_dir, gn_info)

        rust_unit_test_names = {t["name"] + "_test" for t in bazel_targets if t.get("unit_tests")}

        # Check whether a Bazel test target is legitimately skipped from bazel2gn conversion
        # (e.g. uses test_data/data/test_label or an unsupported test rule) while testing a
        # dual-built target, so its GN copy stays hand-written above the sentinel.
        def is_untranslatable_skipped_bazel_test(bname):
            bt = targets_by_name.get(bname)
            if bt is not None:
                if bt["skip_line"] is None:
                    return False
                if base_target_skips.get(bname) or bt.get("has_skip_reason"):
                    return True
                brule = bt["rule"]
                if brule in ("host_go_test", "go_test", "host_rustc_test") and bt.get(
                    "has_test_data"
                ):
                    return True
                if brule in UNSUPPORTED_TEST_BINARY_RULES:
                    return True
                if brule == "rustc_test" and (
                    bt.get("cross_pkg_srcs_or_crate")
                    or (
                        bt.get("has_rustc_env")
                        and bt.get("crate_ref") in targets_by_name
                        and targets_by_name[bt["crate_ref"]].get("has_rustc_env")
                    )
                ):
                    return True
                return False
            if bname.endswith("_test"):
                owner = targets_by_name.get(bname[: -len("_test")])
                if owner and owner.get("skipped_test_attrs"):
                    return bool(
                        owner.get("has_test_data")
                        or any(
                            (owner["name"], a) in base_attr_skips or r
                            for a, _, r in owner.get("skipped_test_attrs", [])
                        )
                    )
            return False

        # Check whether a pre-sentinel GN rustc_test or go_test that is not in BUILD.bazel
        # has no GN-only blockers and all of its dependencies are available in Bazel (in the
        # working tree or on origin/main), meaning it must be migrated to BUILD.bazel.
        def inspect_migratable_gn_test_above_sentinel(gname, gbody, candidate_names):
            if any(n in bazel_names or n in rust_unit_test_names for n in candidate_names):
                return False, []
            pkg_tmpls = (
                "fuchsia_unittest_package",
                "fuchsia_test_package",
                "fuchsia_test_component",
                "fuchsia_unittest_component",
                "fuchsia_component",
                "bootfs_test",
            )
            ref_re = re.compile(
                r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(gname) + r'(?:"|\()'
            )
            if any(
                ptmpl in pkg_tmpls and ref_re.search(pbody)
                for ptmpl, _, pbody in gn_info["pre_targets"].values()
            ):
                return False, []
            cleaned = re.sub(
                r"\bvisibility\s*\+?=\s*\[[^\]]*\]", "", re.sub(r"#[^\n]*", "", gbody)
            )
            if (
                GN_ONLY_ATTR_RE.search(cleaned)
                or GN_BLOCKED_CONSTRUCT_RE.search(cleaned)
                or re.search(
                    r"\b(?:data_deps|non_rust_deps|inputs|test_specs|environments|args|rustenv)\s*\+?=",
                    cleaned,
                )
                or "$" in cleaned
                or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
            ):
                return False, []
            _, _, gn_blocked_dep = inspect_gn_target_deps(
                gbody, pkg_dir, gn_info["pre_targets"], bazel_names
            )
            if gn_blocked_dep:
                return False, []
            local_ref_re = re.compile(
                r'"(?::([A-Za-z0-9_.-]+)|//'
                + re.escape(pkg_dir)
                + r'(?::([A-Za-z0-9_.-]+))?)"'
            )
            for lm in local_ref_re.finditer(cleaned):
                lname = lm.group(1) or lm.group(2) or os.path.basename(pkg_dir)
                if lname not in bazel_names:
                    return False, []
            rebase_deps = []
            for dm in re.finditer(r'"//([^":(\s]+)(?::([^"(\s]+))?"', cleaned):
                dep_pkg = dm.group(1).strip("/")
                dep_tname = dm.group(2) or os.path.basename(dep_pkg)
                if not dep_pkg or dep_pkg == pkg_dir:
                    continue
                if dep_pkg == "third_party/rust_crates" or dep_pkg.startswith(
                    "third_party/rust_crates/"
                ):
                    continue
                if dep_pkg.startswith(("build/", "prebuilt/", "third_party/", "out/")):
                    if not os.path.isfile(os.path.join(workdir, dep_pkg, "BUILD.bazel")):
                        return False, []
                    continue
                dep_bazel_abs = os.path.join(workdir, dep_pkg, "BUILD.bazel")
                t_re = re.compile(r'\bname\s*=\s*"' + re.escape(dep_tname) + r'"')
                has_local_bazel = False
                if os.path.isfile(dep_bazel_abs):
                    try:
                        has_local_bazel = bool(
                            t_re.search(open(dep_bazel_abs, encoding="utf-8").read())
                        )
                    except Exception:
                        has_local_bazel = False
                if has_local_bazel:
                    continue
                try:
                    origin_bazel_txt = subprocess.check_output(
                        ["git", "-C", workdir, "show", f"origin/main:{dep_pkg}/BUILD.bazel"],
                        stderr=subprocess.DEVNULL,
                        text=True,
                    )
                    has_origin_bazel = bool(t_re.search(origin_bazel_txt))
                except Exception:
                    has_origin_bazel = False
                if has_origin_bazel:
                    dep_label = f"//{dep_pkg}"
                    if dep_label not in rebase_deps:
                        rebase_deps.append(dep_label)
                else:
                    return False, []
            return True, rebase_deps

        for gname, (gtmpl, gline, gbody) in sorted(gn_info["pre_targets"].items()):
            # Record authoritative INFO when a fuchsia_package stays hand-written above the
            # sentinel in a dual-build BUILD.gn without fx_package in BUILD.bazel (bazel2gn does
            # not translate fx_package, so GN subpackages/deps callers still need the GN package,
            # and fx_packaged_binary on a rustc_binary with transitive rustc_dylib deps fails in
            # _build_fuchsia_package_impl with duplicate lib/lib*.so runfiles).
            if gtmpl in ("fuchsia_package", "fuchsia_package_with_single_component") and not fx_pkgs:
                findings.append({
                    "source": "migration_sanity",
                    "category": "gn_package_kept_above_sentinel",
                    "severity": "INFO",
                    "file": gn_rel,
                    "line": gline,
                    "message": (
                        f'GN target "{gname}" ({gtmpl} at {gn_rel}:{gline}) remains hand-written '
                        "in BUILD.gn (above the bazel2gn sentinel) and is not migrated to "
                        "BUILD.bazel: `bazel2gn` does not translate `fx_package`, so "
                        f"`{gtmpl}(\"{gname}\")` stays above the sentinel for GN `subpackages`/"
                        "`deps` callers (and avoids `_build_fuchsia_package_impl` duplicate "
                        "`lib/lib*.so` runfile collisions when a packaged `rustc_binary` has "
                        "transitive `rustc_dylib` deps)."
                    ),
                    "remediation": (
                        f"Keep `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                        "(wrapping the `bazel2gn`-generated `fuchsia_component` below the "
                        f"sentinel) and do not add `fx_package(name = \"{gname}\")` or GN-only "
                        f"caller visibility in '{bazel_rel}'."
                    ),
                })
                continue
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
                candidate_names = {gname, out_m.group(1) if out_m else gname}
                bazel_dups = sorted(
                    n
                    for n in candidate_names
                    if (n in bazel_names or n in rust_unit_test_names)
                    and not is_untranslatable_skipped_bazel_test(n)
                )
                # Check if this GN test is consumed by a GN test package/component kept above
                # the sentinel while the Bazel test carries `# @bazel2gn:skip` (e.g. `rustc_test`
                # with `crate = ":<lib>"`, which GN `rustc_test` cannot express).
                kept_for_gn_pkg = any(
                    ptmpl in (
                        "fuchsia_unittest_package",
                        "fuchsia_test_package",
                        "fuchsia_test_component",
                        "fuchsia_unittest_component",
                        "fuchsia_component",
                    )
                    and re.search(
                        r'"(?::|//' + re.escape(pkg_dir) + r':)' + re.escape(gname) + r'(?:"|\()',
                        pbody,
                    )
                    for ptmpl, _, pbody in gn_info["pre_targets"].values()
                ) and all(
                    any(bt["name"] == n and bt["skip_line"] is not None for bt in bazel_targets)
                    for n in bazel_dups
                )
                if bazel_dups and not kept_for_gn_pkg:
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
                    migratable_test, rebase_deps = inspect_migratable_gn_test_above_sentinel(
                        gname, gbody, candidate_names
                    )
                    if migratable_test:
                        rebase_note = (
                            " Dependencies newly migrated on `origin/main`: "
                            + ", ".join(rebase_deps)
                            + "."
                            if rebase_deps
                            else ""
                        )
                        rebase_step = (
                            "First fetch and rebase onto the latest `origin/main` "
                            "(`git fetch origin main && git rebase origin/main`) to pick up the "
                            "Bazel targets for "
                            + ", ".join(rebase_deps)
                            + ". Then m"
                            if rebase_deps
                            else "M"
                        )
                        target_rule_hint = (
                            "`host_rustc_test` (from `//build/bazel/rules/host_tests:host_rustc_test.bzl`) "
                            "or `rustc_test`"
                            if gtmpl == "rustc_test"
                            else "`host_go_test` (from `//build/bazel/rules/host_tests:host_go_test.bzl`) "
                            "or `go_test`"
                        )
                        findings.append({
                            "source": "migration_sanity",
                            "category": "gn_test_not_migrated",
                            "severity": "error",
                            "file": gn_rel,
                            "line": gline,
                            "message": (
                                f"GN test `{gtmpl}(\"{gname}\")` remains hand-written above "
                                f"`## BAZEL2GN SENTINEL` in '{gn_rel}' even though all of its "
                                "dependencies have Bazel targets and it uses no GN-only features."
                                + rebase_note
                            ),
                            "remediation": (
                                f"{rebase_step}igrate `{gtmpl}(\"{gname}\")` to '{bazel_rel}' "
                                f"using {target_rule_hint} without `# @bazel2gn:skip`, delete the "
                                f"hand-written `{gtmpl}(\"{gname}\")` above `## BAZEL2GN SENTINEL` "
                                f"in '{gn_rel}', re-run `fx bazel2gn -d {pkg_dir}` so `bazel2gn` "
                                f"generates the GN copy below the sentinel, keep `:{gname}` in "
                                "`group(\"tests\")`, and export the Bazel test via "
                                f"`bazel_test_suite(\"bazel_tests\")` "
                                f"(`host_tests = [ \"//{pkg_dir}:{gname}\" ]` for host tests) "
                                "wired into `group(\"tests\")`."
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
            gn_only_matches = (
                sorted({m.group(0).strip() for m in GN_ONLY_ATTR_RE.finditer(re.sub(r"#[^\n]*", "", gbody))})
                if gtmpl in CONVERTIBLE_GN_TEMPLATES
                else []
            )
            if gn_only_matches:
                findings.append({
                    "source": "migration_sanity",
                    "category": "gn_only_attr_target_kept_above_sentinel",
                    "severity": "INFO",
                    "file": gn_rel,
                    "line": gline,
                    "message": (
                        f"Convertible GN target `{gtmpl}(\"{gname}\")` stays hand-written above "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' because it uses GN-only constructs "
                        f"({', '.join(f'`{x}`' for x in gn_only_matches)}). This is allowed."
                    ),
                    "remediation": (
                        "bazel2gn can also generate it below the sentinel: define it in "
                        f"'{bazel_rel}' and name the GN-only constructs there with "
                        "`# @bazel2gn:raw_attribute:` lines (or the dedicated tags/attributes the "
                        "checkout's build/tools/bazel2gn/README.md documents, e.g. "
                        "`disable_instrumentation`, `disable_build_flags`), as described in "
                        "\"GN-only attributes on generated targets\" in the coder prompt."
                    ),
                })
            if gtmpl in CONVERTIBLE_GN_TEMPLATES and not GN_ONLY_ATTR_RE.search(gbody):
                if is_packaged_testonly_gn_binary(
                    gname, gtmpl, gbody, gn_info["pre_targets"], bazel_names
                ):
                    continue
                if is_gn_component_target_blocked(
                    gname, gtmpl, gbody, gn_info["pre_targets"], bazel_targets, pkg_dir
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
                if gtmpl == "cm":
                    attr_hints.append(
                        f"declare `fx_compiled_manifest(name = \"{gname}\", manifest = ..., "
                        "component_name = ..., output_name = ...)` (loaded from "
                        "`//build/bazel/rules/components:fx_component.bzl`, with `restricted_features` "
                        "if GN sets them) without `# @bazel2gn:skip`; it generates the same GN `cm()`, "
                        "which is not packaged as a resource"
                    )
                if gtmpl == "fuchsia_component_manifest":
                    attr_hints.append(
                        f"declare `fx_component_manifest(name = \"{gname}\", ...)` "
                        "(loaded from `//build/bazel/rules/components:fx_component.bzl`, "
                        "listing all `.shard.cml` includes in `includes = [...]`) without "
                        "`# @bazel2gn:skip`"
                    )
                elif gtmpl == "fuchsia_component":
                    attr_hints.append(
                        f"declare `fx_component(name = \"{gname}\", compiled_manifest = \":<manifest>\", "
                        "deps = [\":<bin>_packaged_bin\"])` (wrapping its local binary in "
                        "`fx_packaged_binary` from `//build/bazel/rules/packages:fx_packaged_binary.bzl`) "
                        "without `# @bazel2gn:skip`"
                    )
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
                        + ". All convertible library/binary/component targets (including `fuchsia_component_manifest`, "
                        "`fuchsia_component`, benchmark binaries, and reference/bitrot-prevention libraries) "
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

        def has_gn_only_test_inputs(raw_body):
            """Whether a GN test target body uses Ninja-generated inputs, GN variables/conditionals,
            or depends on a local GN-only generator/action target that has no Bazel equivalent."""
            body = re.sub(r"#[^\n]*", "", raw_body)
            if "$" in body or re.search(r"\b(?:if|foreach)\s*\(", body):
                return True
            if re.search(r"\bsource_root\s*=\s*[^\s\"#]", body):
                return True
            if re.search(r"\b(?:sources|inputs)\s*\+?=\s*[A-Za-z_]", body):
                return True
            for sm in re.finditer(r"\b(?:sources|inputs)\s*\+?=\s*\[([^\]]*)\]", body):
                if re.sub(r'"[^"]*"|,|\s', "", sm.group(1)):
                    return True
            for dm in re.finditer(
                r"\b(?:public_deps|deps|non_rust_deps|data_deps|test_deps)\s*\+?=\s*\[([^\]]*)\]", body
            ):
                for dep in re.findall(r'"([^"]+)"', dm.group(1)):
                    if dep.startswith(":") and "(" not in dep:
                        dname = dep[1:]
                        if (
                            dname in gn_info["pre_targets"]
                            and dname not in bazel_rules
                            and gn_info["pre_targets"][dname][0] not in CONVERTIBLE_GN_TEMPLATES
                            and gn_info["pre_targets"][dname][0] != "resource"
                        ):
                            return True
            return False

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
            if has_gn_only_test_inputs(tbody):
                return None
            if ttmpl.startswith("rustc_"):
                return "rust"
            if ttmpl not in GN_CC_TEST_TEMPLATES:
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
            _, _, pkg_blocked = inspect_gn_target_deps(
                gbody,
                pkg_dir,
                gn_info["pre_targets"],
                {t["name"] for t in bazel_targets},
            )
            if pkg_blocked:
                blockers.append("GN-blocked dependency")
            for l in labels:
                local = ""
                if "(" not in l:
                    if l.startswith(":"):
                        local = l[1:]
                    elif l.startswith(f"//{pkg_dir}:"):
                        local = l.split(":", 1)[1]
                if local:
                    pkg_test_deps.add(local)
                    comp = gn_info["pre_targets"].get(local) or gn_info.get(
                        "post_targets", {}
                    ).get(local)
                    if comp and comp[0] in ("fuchsia_test_component", "fuchsia_unittest_component", "fuchsia_component"):
                        for cm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", comp[2]):
                            for cl in re.findall(r'"([^"]+)"', cm.group(1)):
                                if "(" not in cl:
                                    if cl.startswith(":"):
                                        pkg_test_deps.add(cl[1:])
                                    elif cl.startswith(f"//{pkg_dir}:"):
                                        pkg_test_deps.add(cl.split(":", 1)[1])
            for td_name in sorted(pkg_test_deps):
                owner = gn_info["pre_targets"].get(td_name) or gn_info["pre_targets"].get(
                    re.sub(r"_test$", "", td_name)
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
                    if has_gn_only_test_inputs(owner[2]):
                        blockers.append("Ninja-generated inputs or GN-only generator deps")
            # Unknown or unbuildable tests never trigger it.
            langs = {test_language(l) for l in labels}
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

    gn_kept_pkg_test_deps_by_dir[pkg_dir] = set(gn_kept_pkg_test_deps)

    # Check that non-test fuchsia_package / fuchsia_package_with_single_component targets
    # above the sentinel whose components are already migrated to fx_component in
    # BUILD.bazel (and that only serve assembly or have an assembly bazel_inputs bridge)
    # are migrated to # @bazel2gn:skip fx_package in BUILD.bazel and removed from BUILD.gn.
    def check_unmigrated_gn_fuchsia_packages(btargets, brel, grel, pdir, g_info):
        if not g_info:
            return
        fx_pkg_names = {
            t["name"] for t in btargets if t["rule"] in ("fx_package", "fuchsia_package")
        } | {
            t["package_name"]
            for t in btargets
            if t["rule"] in ("fx_package", "fuchsia_package")
        }
        bz_comp_names = {
            t["name"]
            for t in btargets
            if t["rule"] in ("fx_component", "resource", "fuchsia_package_resource_group")
        }
        bridge_gn_abs = os.path.join(
            workdir, "bundles", "assembly", "bazel_inputs", pdir, "BUILD.gn"
        )
        has_bridge = os.path.isfile(bridge_gn_abs)
        for gname, (gtmpl, gline, gbody) in sorted(g_info["pre_targets"].items()):
            if gtmpl not in ("fuchsia_package", "fuchsia_package_with_single_component"):
                continue
            cleaned = re.sub(r"#[^\n]*", "", gbody)
            if (
                re.search(r"\btestonly\s*=\s*true\b", cleaned)
                or "$" in cleaned
                or re.search(r"\b(?:if|foreach)\s*\(", cleaned)
                or GN_ONLY_ATTR_RE.search(cleaned)
            ):
                continue
            pn_m = re.search(r'\bpackage_name\s*=\s*"([^"]+)"', cleaned)
            gn_pkg_name = pn_m.group(1) if pn_m else gname
            if gname in fx_pkg_names or gn_pkg_name in fx_pkg_names:
                continue
            dep_lbls = []
            for dm in re.finditer(r"\bdeps\s*\+?=\s*\[([^\]]*)\]", cleaned):
                dep_lbls.extend(re.findall(r'"([^"]+)"', dm.group(1)))
            if not dep_lbls:
                continue
            dep_locals = [
                d[1:]
                if d.startswith(":")
                else (d.split(":", 1)[1] if pdir and d.startswith(f"//{pdir}:") else "")
                for d in dep_lbls
            ]
            if not all(d and d in bz_comp_names for d in dep_locals):
                continue
            vis_lbls = []
            for vm in re.finditer(r"\bvisibility\s*\+?=\s*\[([^\]]*)\]", cleaned):
                vis_lbls.extend(re.findall(r'"([^"]+)"', vm.group(1)))
            assembly_only_vis = bool(vis_lbls) and all(
                v.startswith("//bundles/assembly") or v in (":*", f"//{pdir}:*")
                for v in vis_lbls
            )
            if not (assembly_only_vis or has_bridge):
                continue
            # Keep fuchsia_package in BUILD.gn when a packaged component binary depends
            # on a rustc_dylib target and collect_debug_symbols_manifest_aspect in
            # //build/bazel/debug_symbols/aspects.bzl does not yet collect rust_dylib_library.
            dylib_dep = find_uncollected_rust_dylib_dep(pdir, dep_locals)
            if dylib_dep:
                info_msg = (
                    f'GN `{gtmpl}("{gname}")` in \'{grel}\' stays above '
                    "`## BAZEL2GN SENTINEL` because its packaged component binary "
                    f"depends on dual-built `rustc_dylib` `{dylib_dep}`, which would "
                    "duplicate the shared dylib in BlobFS across GN and Bazel packages "
                    "in assembled images."
                )
                info_rem = (
                    f'Keep `{gtmpl}("{gname}")` in \'{grel}\' bridged via '
                    f"`//bundles/assembly/bazel_inputs/{pdir}` while `{dylib_dep}` is "
                    "dual-built in GN and shared by GN-assembled packages."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "gn_fuchsia_package_kept_for_rust_dylib_debug_symbols",
                    "severity": "INFO",
                    "file": grel,
                    "line": gline,
                    "message": info_msg,
                    "remediation": info_rem,
                })
                cand_lines = {gline}
                for rev in dict.fromkeys((change_base, "HEAD~1", "HEAD", "origin/main")):
                    if not rev:
                        continue
                    try:
                        base_gn_raw = subprocess.check_output(
                            ["git", "-C", workdir, "show", f"{rev}:{grel}"],
                            stderr=subprocess.DEVNULL,
                            text=True,
                        )
                        base_gn_parsed = parse_gn_text(base_gn_raw)
                        for base_map in (
                            base_gn_parsed.get("pre_targets", {}),
                            base_gn_parsed.get("post_targets", {}),
                        ):
                            if gname in base_map:
                                cand_lines.add(base_map[gname][1])
                    except Exception:
                        pass
                for cand_ln in sorted(cand_lines):
                    for unm_msg in (
                        f'GN target "{gname}" ({gtmpl} at {grel}:{cand_ln}) remains hand-written '
                        "in BUILD.gn (above the bazel2gn sentinel) and is not migrated to "
                        "BUILD.bazel",
                        f'GN target "{gname}" ({gtmpl} at {grel}:{cand_ln}) has no corresponding '
                        "target in BUILD.bazel",
                    ):
                        findings.append({
                            "source": "migration_sanity",
                            "category": "unmatched_target",
                            "severity": "INFO",
                            "file": grel,
                            "line": cand_ln,
                            "message": unm_msg,
                            "remediation": info_rem,
                        })
                continue
            pkg_attr = (
                f'package_name = "{gn_pkg_name}", ' if gn_pkg_name != gname else ""
            )
            comp_list = ", ".join(f'":{d}"' for d in dep_locals)
            msg = (
                f'GN `{gtmpl}("{gname}")` in \'{grel}\' packages component(s) '
                f"({comp_list}) that are already migrated to `BUILD.bazel`, and "
                "shared library / `rustc_dylib` runfiles deduplication is supported "
                "in `fx_package`, but the package target remains in `'BUILD.gn'`."
            )
            rem = (
                f'Add `# @bazel2gn:skip` `fx_package(name = "{gname}", {pkg_attr}'
                f'components = [{comp_list}], visibility = ["//bundles/assembly:__subpackages__"])` '
                f"in '{brel}', delete `{gtmpl}(\"{gname}\")` from '{grel}', and "
                f"replace any `//bundles/assembly/bazel_inputs/{pdir}` bridge with "
                f"`//{pdir}:{gname}` in `bundles/assembly/BUILD.bazel`."
            )
            findings.append({
                "source": "migration_sanity",
                "category": "duplicate_gn_fuchsia_package",
                "severity": "error",
                "file": grel,
                "line": gline,
                "message": msg,
                "remediation": rem,
            })

    check_unmigrated_gn_fuchsia_packages(
        bazel_targets, bazel_rel, gn_rel, pkg_dir, gn_info
    )
    if gn_info and gn_info["has_sentinel"]:
        # A package-private testonly helper that only skipped (Bazel-only) tests use would
        # otherwise be generated into BUILD.gn with no GN consumer.
        for t in bazel_targets:
            if (
                t["rule"] in CONVERTIBLE_BAZEL_RULES
                and t["skip_line"] is None
                and t["name"] not in auto_skipped_manifests
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
        # Check whether an assembled fx_package packages a component whose binary depends on a
        # dual-built rustc_dylib target (or when aspects.bzl omits rust_dylib_library).
        def check_fx_package_rust_dylib_debug_symbols(f_pkgs, brel, grel, pdir):
            found_dylib = False
            bridge_rel = f"bundles/assembly/bazel_inputs/{pdir}/BUILD.gn"
            has_pkg_bridge = os.path.isfile(os.path.join(workdir, bridge_rel)) or bool(
                git_lines(["show", f"{change_base}:{bridge_rel}"])
            )
            for fpkg in f_pkgs:
                vis = fpkg.get("vis_items") or []
                is_assembled = has_pkg_bridge or any(
                    v.startswith("//bundles/assembly") for v in vis
                )
                if not is_assembled:
                    asm_bz = os.path.join(workdir, "bundles", "assembly", "BUILD.bazel")
                    if os.path.isfile(asm_bz):
                        try:
                            asm_txt = open(asm_bz, encoding="utf-8").read()
                            is_assembled = f'"//{pdir}' in asm_txt
                        except OSError:
                            pass
                if not is_assembled:
                    continue
                seeds = [fpkg["name"]] + sorted(fpkg.get("dep_refs", ()))
                dylib_dep = find_uncollected_rust_dylib_dep(pdir, seeds)
                if not dylib_dep:
                    continue
                found_dylib = True
                pname = fpkg["name"]
                msg = (
                    f'Target `fx_package(name = "{pname}")` in \'{brel}\' packages a binary '
                    f"that depends on dual-built `rustc_dylib` `{dylib_dep}`. Switching "
                    "assembled packages that link a dual-built `rustc_dylib` from a GN "
                    "`fuchsia_package` to a Bazel `fx_package` duplicates the shared dylib "
                    "in BlobFS alongside the GN-built dylib shared by remaining GN packages "
                    "(exceeding release binary size creep budgets on size-checked builders)."
                )
                rem = (
                    f'Remove `fx_package(name = "{pname}")` (and any unused `fx_package` '
                    f"load) from '{brel}', keep `fuchsia_package(\"{pname}\")` above "
                    f"`## BAZEL2GN SENTINEL` in '{grel}' (while keeping `fx_component`, "
                    f"`fx_component_manifest`, and `fx_packaged_binary` in '{brel}'), "
                    f"restore any deleted parent `BUILD.gn` group references to `{pname}`, "
                    f"keep the `//bundles/assembly/bazel_inputs/{pdir}` bridge in "
                    "`bundles/assembly/BUILD.bazel` and `bundles/assembly/bazel_inputs/BUILD.gn`, "
                    f"and remove `Bazel-Migration-Target: //{pdir}:{pname}` and any "
                    "commit-body claims about migrating the package."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "fx_package_missing_rust_dylib_debug_symbols",
                    "severity": "error",
                    "file": brel,
                    "line": fpkg["line"],
                    "message": msg,
                    "remediation": rem,
                })
            return found_dylib

        has_uncollected_dylib_fx_pkg = check_fx_package_rust_dylib_debug_symbols(
            fx_pkgs, bazel_rel, gn_rel, pkg_dir
        )
        bazel_text_raw = open(bazel_abs, encoding="utf-8").read()
        migrated_pkg_names = {t["name"] for t in bazel_targets} | {t["package_name"] for t in fx_pkgs}
        migrated_manifests = set(re.findall(r'\bmanifest\s*=\s*"([^"]+)"', bazel_text_raw))
        has_b2gn_convertible = any(
            t["rule"] in CONVERTIBLE_BAZEL_RULES and t["name"] not in packaged_binaries and t["name"] not in auto_skipped_manifests and t["skip_line"] is None
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

        # Exclude GN package/component targets that still have active GN reverse dependencies
        # and cannot be generated below the sentinel by bazel2gn.
        def filter_kept_gn_package_targets(pdir, pre_targets, btargets, has_sentinel, cand_dups):
            if not cand_dups:
                return set()
            default_t = os.path.basename(pdir)
            ref_files = set(
                git_lines(
                    [
                        "grep",
                        "-l",
                        "--fixed-strings",
                        f"//{pdir}",
                        "--",
                        "*BUILD.gn",
                        "*.gni",
                    ]
                )
            )
            parts = pdir.split("/")
            for i in range(1, len(parts)):
                anc_rel = "/".join(parts[:i]) + "/BUILD.gn"
                if os.path.isfile(os.path.join(workdir, anc_rel)):
                    ref_files.add(anc_rel)
            abs_re = re.compile(
                r'"//' + re.escape(pdir) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
            )
            ext_rdeps = set()
            for rel in sorted(ref_files):
                if (
                    rel == f"{pdir}/BUILD.gn"
                    or rel.startswith("bundles/assembly/bazel_inputs/")
                    or os.path.basename(rel) == "visibility.gni"
                ):
                    continue
                try:
                    raw = open(os.path.join(workdir, rel), encoding="utf-8").read()
                except Exception:
                    continue
                cleaned = re.sub(
                    r"\bvisibility\s*\+?=\s*\[[^\]]*\]",
                    "",
                    re.sub(r"#[^\n]*", "", raw),
                    flags=re.DOTALL,
                )
                for m in abs_re.finditer(cleaned):
                    ext_rdeps.add(m.group(1) or default_t)
                caller_pkg = os.path.dirname(rel).strip("/")
                if caller_pkg and pdir.startswith(caller_pkg + "/"):
                    rel_sub = pdir[len(caller_pkg) + 1 :]
                    rel_re = re.compile(
                        r'"' + re.escape(rel_sub) + r'(?::([A-Za-z0-9_.-]+))?(?:\([^)]*\))?"'
                    )
                    for m in rel_re.finditer(cleaned):
                        ext_rdeps.add(m.group(1) or default_t)
            b2gn_conv = {
                n
                for n in cand_dups
                if has_sentinel
                and pre_targets[n][0] == "fuchsia_component"
                and not is_gn_component_target_blocked(
                    n, pre_targets[n][0], pre_targets[n][2], pre_targets, btargets, pdir
                )
            }
            kept = {
                n
                for n in pre_targets
                if n not in cand_dups or (n in ext_rdeps and n not in b2gn_conv)
            }
            changed = True
            while changed:
                changed = False
                for k in list(kept):
                    kbody = pre_targets[k][2]
                    for c in list(cand_dups - kept - b2gn_conv):
                        if re.search(
                            r'"(?::|//' + re.escape(pdir) + r':)' + re.escape(c) + r'(?:"|\()',
                            kbody,
                        ):
                            kept.add(c)
                            changed = True
            return cand_dups - kept

        if gn_info:
            gn_pkg_TMPLS = ("fuchsia_package", "fuchsia_package_with_single_component", "fuchsia_component")
            cand_dup_gn_pkg_names = set()
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
                    cand_dup_gn_pkg_names.add(gname)
            dup_gn_pkg_names = filter_kept_gn_package_targets(
                pkg_dir,
                gn_info["pre_targets"],
                bazel_targets,
                gn_info["has_sentinel"],
                cand_dup_gn_pkg_names,
            )

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
                if (is_dup_pkg or is_dup_bin) and not has_uncollected_dylib_fx_pkg:
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
        # Match references to the package's own bridge directory (`//<bridge_dir_rel>`,
        # `//<bridge_dir_rel>:<target>`, or `//<bridge_dir_rel>(...)`), excluding
        # unmigrated child subpackage bridges (`//<bridge_dir_rel>/<sub>...`).
        bridge_label_re = re.compile(
            r'"//' + re.escape(bridge_dir_rel) + r'(?:[:(][^"]*)?"'
        )
        in_assembly_bazel = False
        if os.path.isfile(os.path.join(workdir, assembly_bazel_rel)):
            try:
                asm_bz_clean = re.sub(
                    r"#[^\n]*",
                    "",
                    open(
                        os.path.join(workdir, assembly_bazel_rel), encoding="utf-8"
                    ).read(),
                )
                in_assembly_bazel = bridge_label_re.search(asm_bz_clean) is not None
            except Exception:
                pass
        in_assembly_inputs_gn = False
        if os.path.isfile(os.path.join(workdir, assembly_inputs_gn_rel)):
            try:
                asm_in_clean = re.sub(
                    r"#[^\n]*",
                    "",
                    open(
                        os.path.join(workdir, assembly_inputs_gn_rel), encoding="utf-8"
                    ).read(),
                )
                in_assembly_inputs_gn = bridge_label_re.search(asm_in_clean) is not None
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
        if (
            (has_bridge_files or in_assembly_bazel or in_assembly_inputs_gn)
            and not bridge_kept
            and not has_uncollected_dylib_fx_pkg
        ):
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

        # Verify that when a package wired into bundles/assembly/BUILD.bazel uses
        # structured configuration (which invokes @gn_targets//.../tools/configc),
        # _AIB_TOOLS_DEPS in bundles/assembly/bazel_inputs/BUILD.gn exports
        # "//tools/configc:bazel_input($host_toolchain)".
        def check_assembly_structured_config_tool_input(
            p_dir, pkg_name, bz_targets, asm_bz_rel, asm_in_gn_rel
        ):
            sc_rules = (
                "fuchsia_structured_config_rust_lib",
                "fuchsia_structured_config_cpp_elf_lib",
                "fuchsia_structured_config_cpp_driver_lib",
            )
            sc_target = next(
                (t for t in bz_targets if t["rule"] in sc_rules),
                None,
            )
            if not sc_target:
                return
            asm_bz_abs = os.path.join(workdir, asm_bz_rel)
            asm_in_gn_abs = os.path.join(workdir, asm_in_gn_rel)
            if not (os.path.isfile(asm_bz_abs) and os.path.isfile(asm_in_gn_abs)):
                return
            try:
                asm_bz_txt = open(asm_bz_abs, encoding="utf-8").read()
                asm_in_txt = open(asm_in_gn_abs, encoding="utf-8").read()
            except OSError:
                return
            pkg_ref_re = re.compile(
                r'"//'
                + re.escape(p_dir)
                + r'(?::'
                + re.escape(pkg_name)
                + r')?"'
            )
            if not pkg_ref_re.search(asm_bz_txt):
                return
            if "//tools/configc:bazel_input" in asm_in_txt:
                return
            tools_m = re.search(r"(?m)^\s*_AIB_TOOLS_DEPS\s*=\s*\[", asm_in_txt)
            if not tools_m:
                return
            tools_line = asm_in_txt[: tools_m.start()].count("\n") + 1
            sc_rule = sc_target["rule"]
            sc_name = sc_target["name"]
            msg = (
                f"Package '//{p_dir}:{pkg_name}' is wired into "
                f"'{asm_bz_rel}' and uses `{sc_rule}(name = \"{sc_name}\")`, "
                "which invokes `@gn_targets//toolchain_host_x64/tools/configc:configc`, "
                f"but `_AIB_TOOLS_DEPS` in '{asm_in_gn_rel}' is missing "
                "`\"//tools/configc:bazel_input($host_toolchain)\"`. "
                "Builders that run `//bundles/assembly:platform` or "
                "`//bundles/assembly:bringup` without `//build/bazel/target_tests` "
                "fail with `Unknown @gn_targets targets: "
                "@gn_targets+//toolchain_host_x64/tools/configc`."
            )
            rem = (
                "Add `\"//tools/configc:bazel_input($host_toolchain)\",` to "
                f"`_AIB_TOOLS_DEPS` in '{asm_in_gn_rel}' (keep the list sorted "
                "and format with `fx format-code`)."
            )
            findings.append({
                "source": "migration_sanity",
                "category": "missing_assembly_bazel_input_tool",
                "severity": "error",
                "file": asm_in_gn_rel,
                "line": tools_line,
                "message": msg,
                "remediation": rem,
            })

        check_assembly_structured_config_tool_input(
            pkg_dir,
            fx_pkg_name,
            bazel_targets,
            assembly_bazel_rel,
            assembly_inputs_gn_rel,
        )

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
            workdir, "third_party", "bazel_vendor", "fuchsia_in_tree", f"{repo_name}+", pkg_path, "defs.bzl"
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
#    host-only (and isn't packaged into an fx_component test fixture or kept GN test package)
#    must be packaged for the device (fx_unittest_package, or
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


# Resolve a //.shard.cml include path to its owning expect_includes() target label
# (e.g. "//src/sys/test_runners/tmp_storage.shard.cml" -> "//src/sys/test_runners:tmp_storage")
# when fx_unittest_package / fx_unittest_component can auto-include it via aspect_hints.
def resolve_expect_includes_shard(shard_path):
    if not isinstance(shard_path, str) or not shard_path.startswith("//"):
        return None
    rel_path = shard_path[2:].strip("/")
    parts = rel_path.split("/")
    for i in range(len(parts) - 1, 0, -1):
        s_pkg = "/".join(parts[:i])
        s_file = "/".join(parts[i:])
        gn_file = os.path.join(workdir, s_pkg, "BUILD.gn")
        bz_file = os.path.join(workdir, s_pkg, "BUILD.bazel")
        if os.path.isfile(gn_file):
            g_info = parse_gn_file(gn_file)
            if g_info:
                for g_name, (g_tmpl, _, g_body) in g_info["pre_targets"].items():
                    if g_tmpl == "expect_includes" and f'"{s_file}"' in g_body:
                        return f"//{s_pkg}:{g_name}"
        if os.path.isfile(bz_file):
            try:
                bz_tree = ast.parse(open(bz_file, encoding="utf-8").read(), filename=bz_file)
                for st in bz_tree.body:
                    if not (
                        isinstance(st, ast.Expr)
                        and isinstance(st.value, ast.Call)
                        and isinstance(st.value.func, ast.Name)
                        and st.value.func.id == "expect_includes"
                    ):
                        continue
                    st_kws = {kw.arg: kw.value for kw in st.value.keywords if kw.arg}
                    n_node = st_kws.get("name")
                    inc_node = st_kws.get("includes")
                    if (
                        isinstance(n_node, ast.Constant)
                        and isinstance(n_node.value, str)
                        and isinstance(inc_node, ast.List)
                    ):
                        inc_vals = {
                            e.value.lstrip(":")
                            for e in inc_node.elts
                            if isinstance(e, ast.Constant) and isinstance(e.value, str)
                        }
                        if s_file in inc_vals:
                            return f"//{s_pkg}:{n_node.value}"
            except Exception:
                pass
        if os.path.isfile(gn_file) or os.path.isfile(bz_file):
            break
    return None


def inspect_standard_unittest_cml(cml_abs):
    try:
        raw = open(cml_abs, encoding="utf-8").read()
    except Exception:
        return False, []
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
        return False, []
    if not isinstance(data, dict) or not (set(data.keys()) <= {"include", "program"}):
        return False, []
    prog = data.get("program", {})
    if not isinstance(prog, dict) or not (set(prog.keys()) <= {"binary"}):
        return False, []
    inc = data.get("include", [])
    if not isinstance(inc, list):
        return False, []
    expect_labels = []
    for s in inc:
        if not isinstance(s, str):
            return False, []
        if s in STANDARD_UNITTEST_CML_SHARDS:
            continue
        exp_lbl = resolve_expect_includes_shard(s)
        if not exp_lbl:
            return False, []
        if exp_lbl not in expect_labels:
            expect_labels.append(exp_lbl)
    return True, expect_labels
for pkg_dir in sorted(candidate_dirs):
    bazel_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel")
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_abs = os.path.join(workdir, pkg_dir, "BUILD.gn")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")
    if FIDL_RUST_NEXT_UNIMPLEMENTED and os.path.isfile(gn_abs):
        try:
            if FIDL_RUST_NEXT_DEP_RE.search(open(gn_abs, encoding="utf-8").read()):
                findings[:] = [
                    f
                    for f in findings
                    if not (
                        f.get("file") == gn_rel
                        and f.get("category") in ("gn_test_package_not_migrated", "duplicate_target_above_sentinel")
                    )
                ]
        except Exception:
            pass
    if not os.path.isfile(bazel_abs):
        continue
    try:
        btext = open(bazel_abs, encoding="utf-8").read()
        btree = ast.parse(btext, filename=bazel_abs)
    except Exception:
        continue
    if FIDL_RUST_NEXT_UNIMPLEMENTED:
        next_m = FIDL_RUST_NEXT_DEP_RE.search(btext)
        if next_m:
            next_line = btext.count("\n", 0, next_m.start()) + 1
            findings.append({
                "source": "migration_sanity",
                "category": "unimplemented_fidl_rust_next_dep",
                "severity": "error",
                "file": bazel_rel,
                "line": next_line,
                "message": (
                    f"'{bazel_rel}' depends on `:{next_m.group(1)}`, but `fidl_library()` in "
                    "`//build/bazel/rules/fidl/fidl_library.bzl` leaves `enable_rust_next` as an unimplemented "
                    "stub and does not generate `*_rust_next` or `*_rust_fdomain_next` Bazel targets yet."
                ),
                "remediation": (
                    f"Do not leave an unbuildable target in '{bazel_rel}': if the entire package is blocked on "
                    f"`*_rust_next` / `*_rust_fdomain_next` bindings, delete '{bazel_rel}' and restore '{gn_rel}' "
                    f"(`rm -f {bazel_rel} && git checkout -- {gn_rel}`), run `run_checks.sh` on the clean tree so it "
                    "exits 0, and report the blocker in `summary` with `modified_files: []` and `disputed_findings: []`. "
                    f"If '{bazel_rel}' also migrates unblocked targets, remove only the blocked target from '{bazel_rel}' "
                    f"and keep it in '{gn_rel}'."
                ),
            })
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
            rust_tests.append(
                (
                    tname,
                    node.lineno,
                    "HOST_OS_CONSTRAINTS" in seg or "HOST_CONSTRAINTS" in seg,
                )
            )

    # Check that non-host-only rustc_test targets are packaged or kept in a blocked GN test pkg.
    def check_rust_unit_test_layout(cur_pkg_dir, brel, r_tests, r_used):
        if {"fx_test", "fx_unittest_package"} & r_used:
            return
        gn_kept_deps = gn_kept_pkg_test_deps_by_dir.get(cur_pkg_dir, set())
        grel = os.path.join(cur_pkg_dir, "BUILD.gn")
        device_tests = [
            t for t in r_tests if not t[2] and t[0] not in gn_kept_deps
        ]
        gn_packaged_tests = [
            t for t in r_tests if not t[2] and t[0] in gn_kept_deps
        ]
        for tname, tline, _ in gn_packaged_tests:
            msg = (
                f"`rustc_test(name = \"{tname}\")` in '{brel}' is dual-built "
                f"into '{grel}' below `## BAZEL2GN SENTINEL` and packaged by a "
                "hand-written GN test package above the sentinel that has GN-only "
                "blockers (such as unmigrated component dependencies, `test_type`, "
                "or conditional `test_specs`), so omitting `fx_unittest_package` "
                f"or `fx_test` in '{brel}' is expected."
            )
            rem = (
                "Keep the test package above `## BAZEL2GN SENTINEL` in BUILD.gn "
                "wired into `group(\"tests\")` while its blockers remain, and "
                "mention the blocker in the commit message and summary."
            )
            findings.append({
                "source": "migration_sanity",
                "category": "gn_test_kept_above_sentinel",
                "severity": "INFO",
                "file": brel,
                "line": tline,
                "message": msg,
                "remediation": rem,
            })
        if device_tests:
            tname, tline, _ = device_tests[0]
            msg = (
                f"`rustc_test(name = \"{tname}\")` in '{brel}' is not "
                "host-only but the package declares no `fx_unittest_package` or "
                "`fx_test`, so the Fuchsia (target-side) test is missing."
            )
            rem = (
                f"Add `fx_unittest_package(name = \"<test-pkg>\", unit_tests = "
                f"[\":{tname}\"])` (use `fx_unittest_component` + `fx_package` + "
                "`fx_test` if packaging extra resources, or unroll into "
                "`fx_packaged_binary` + `fx_component_manifest` + "
                "`fx_test_component` only if a custom `.cml` is needed), or "
                "restrict the test with `target_compatible_with = "
                "HOST_OS_CONSTRAINTS` if GN only built it for the host."
            )
            findings.append({
                "source": "migration_sanity",
                "category": "rust_unit_test_layout",
                "severity": "error",
                "file": brel,
                "line": tline,
                "message": msg,
                "remediation": rem,
            })

    check_rust_unit_test_layout(pkg_dir, bazel_rel, rust_tests, rules_used)

    # Test target hygiene: package-private plumbing, no "manual" tag, crate deps
    # reused, <crate>_host_test naming, and no unrolled unit-test manifests when
    # fx_unittest_package / fx_unittest_component suffices.
    calls = {}
    for node in btree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
        ):
            kws = {kw.arg: kw.value for kw in node.value.keywords if kw.arg}
            n = kws.get("name")
            if isinstance(n, ast.Constant) and isinstance(n.value, str):
                calls[n.value] = (node.value.func.id, kws, node.lineno)

    def str_list(v):
        if isinstance(v, ast.BinOp):
            return str_list(v.left) + str_list(v.right)
        if isinstance(v, ast.List):
            return [
                e.value
                for e in v.elts
                if isinstance(e, ast.Constant) and isinstance(e.value, str)
            ]
        return []

    def str_val(v):
        return v.value if isinstance(v, ast.Constant) and isinstance(v.value, str) else None

    # Check whether a device rustc_test binary is packaged into a standalone fx_component
    # (e.g. a test fixture component synced to fuchsia_component via bazel2gn and packaged by
    # another directory's test package) or a kept GN test package above the sentinel.
    def is_component_or_gn_packaged_rust_test(test_bin_name):
        gn_kept = gn_kept_pkg_test_deps_by_dir.get(pkg_dir, set())
        bin_labels = {test_bin_name, f":{test_bin_name}", f"//{pkg_dir}:{test_bin_name}"}
        if set(bin_labels) & gn_kept:
            return True
        pkg_bin_labels = set(bin_labels)
        for cname, (crule, ckws, _) in calls.items():
            if (
                crule == "fx_packaged_binary"
                and (str_val(ckws.get("binary")) or "") in bin_labels
            ):
                pkg_bin_labels.update({cname, f":{cname}", f"//{pkg_dir}:{cname}"})
        for cname, (crule, ckws, _) in calls.items():
            if crule == "fx_component" and (set(str_list(ckws.get("deps"))) & pkg_bin_labels):
                return True
        return False

    device_tests = [
        t
        for t in rust_tests
        if not t[2] and not is_component_or_gn_packaged_rust_test(t[0])
    ]
    if device_tests and not ({"fx_test", "fx_unittest_package"} & rules_used):
        tname, tline, _ = device_tests[0]
        msg = (
            f'`rustc_test(name = "{tname}")` in \'{bazel_rel}\' is not host-only '
            "but the package declares no `fx_unittest_package` or `fx_test`, "
            "so the Fuchsia (target-side) test is missing."
        )
        rem = (
            f'Add `fx_unittest_package(name = "<test-pkg>", unit_tests = [":{tname}"])` '
            "(use `fx_unittest_component` + `fx_package` + `fx_test` if packaging extra "
            "resources, or unroll into `fx_packaged_binary` + `fx_component_manifest` + "
            "`fx_test_component` only if a custom `.cml` is needed), or restrict the test "
            "with `target_compatible_with = HOST_OS_CONSTRAINTS` if GN only built it for "
            "the host."
        )
        findings.append({
            "source": "migration_sanity",
            "category": "rust_unit_test_layout",
            "severity": "error",
            "file": bazel_rel,
            "line": tline,
            "message": msg,
            "remediation": rem,
        })

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
            is_std_cml, expect_labels = inspect_standard_unittest_cml(cml_abs)
            if not is_std_cml:
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
            hint_clause = ""
            if expect_labels:
                fmt_lbls = ", ".join(f'"{l}"' for l in expect_labels)
                hint_clause = (
                    f" Pass `aspect_hints = [{fmt_lbls}]` on `{raw_bin}` (defining "
                    "`expect_includes(...)` in the shard package's `BUILD.bazel` if not "
                    "yet present, instead of `exports_files`) so `fx_unittest_package` "
                    "auto-includes the shard(s) and `bazel2gn` emits the GN `deps`."
                )
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
                    + hint_clause
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
                    + hint_clause
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

    GENERIC_RUST_CRATE_NAMES = {
        "a", "bin", "bin_with_fake_time", "client", "common", "core", "driver",
        "fidl", "framework", "lib", "main", "parser", "rust", "server", "test",
        "test_lib", "test_util", "testing", "tests", "util", "utils",
        "validator",
    }

    def is_generic_rust_test_output_name(name):
        base = name.replace("-", "_")
        for suffix in ("_lib_test", "_bin_test", "_dylib_test", "_host_test", "_test"):
            if base.endswith(suffix):
                base = base[: -len(suffix)]
                break
        return base in GENERIC_RUST_CRATE_NAMES

    gn_info = parse_gn_file(os.path.join(workdir, pkg_dir, "BUILD.gn"))
    btext_lines = btext.splitlines()
    for tname, (rule, kws, line) in calls.items():
        if rule not in ("rustc_test", "host_rustc_test") or not (gn_info and gn_info["has_sentinel"]):
            continue
        idx = line - 2
        while idx >= 0 and btext_lines[idx].strip().startswith("#"):
            idx -= 1
        if any("@bazel2gn:skip" in l for l in btext_lines[idx + 1 : line - 1]):
            continue
        raw_output_name = None
        for l in btext_lines[idx + 1 : line - 1]:
            m_out = re.search(r'@bazel2gn:raw_attribute:output_name\s*=\s*"([^"]+)"', l)
            if m_out:
                raw_output_name = m_out.group(1)
        out_name = raw_output_name or (str_val(kws.get("binary_name")) if rule == "rustc_test" else None) or str_val(kws.get("crate_name")) or tname
        effective_out = out_name.replace("-", "_")
        if is_generic_rust_test_output_name(effective_out):
            findings.append({
                "source": "migration_sanity",
                "category": "generic_test_output_name",
                "severity": "error",
                "file": bazel_rel,
                "line": line,
                "message": (
                    f"`{rule}(name = \"{tname}\")` in '{bazel_rel}' resolves to generic test binary name "
                    f"`{effective_out}` in `${{root_out_dir}}`, which collides across packages."
                ),
                "remediation": (
                    f"Give `{rule}(name = \"{tname}\")` a package-specific `name` (such as "
                    f"`\"<crate_name>_lib_test\"` or `\"<pkg>_test\"`) in '{bazel_rel}', update "
                    f"any referencing `wrap_host_rust_test(binary_name = ...)` / "
                    f"`fx_packaged_binary(binary = ...)` / `{pkg_dir}/BUILD.gn` test references, "
                    f"and re-run `fx bazel2gn -d {pkg_dir}` (do not use "
                    f"`# @bazel2gn:raw_attribute:output_name` or `crate_name` on `{rule}`)."
                ),
            })


# Discover candidate planter executables, including any running planter daemon in
# /proc/<pid>/exe (whose in-memory binary may predate an updated on-disk binary)
# as well as the on-disk planter binary on PATH or ~/.local/bin/planter.
def discover_planter_bins():
    import shutil

    bins = []
    seen_inodes = set()

    def add_bin(path):
        if not path:
            return
        try:
            st = os.stat(path)
        except OSError:
            return
        if not os.access(path, os.X_OK):
            return
        key = (st.st_dev, st.st_ino)
        if key not in seen_inodes:
            seen_inodes.add(key)
            bins.append(path)

    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                with open(f"/proc/{entry}/comm", encoding="utf-8") as cf:
                    if cf.read().strip() != "planter":
                        continue
            except OSError:
                continue
            add_bin(f"/proc/{entry}/exe")
    except OSError:
        pass

    add_bin(shutil.which("planter"))
    add_bin(os.path.expanduser("~/.local/bin/planter"))
    return bins


# Emit INFO compatibility entries for excluded global/umbrella BUILD.bazel directories
# that also have an existing BUILD.gn on disk, and for unmigrated/deferred directories
# in a multi-directory batch whose BUILD.bazel was not added or was removed while other
# directories in the batch were migrated, so review_diff does not flag them with gn_unmodified.
def record_excluded_dir_gn_unmodified_info(extra_dirs):
    gn_unmod_msgs = (
        "CL adds BUILD.bazel but does not modify the existing BUILD.gn file "
        "(missing bazel2gn sentinel or target removal).",
        "CL contains BUILD.bazel but does not modify the existing BUILD.gn file "
        "(missing `## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` or target removal)",
    )
    rem = (
        "BUILD.gn is in sync with BUILD.bazel "
        "(e.g. exports_files or # @bazel2gn:skip target changes have no GN equivalent, "
        "or this batch directory was left unmigrated in GN)."
    )
    has_any_migrated_dir = any(
        os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))
        for d in (set(target_dirs) | all_changed_build_dirs)
    )
    if len(target_dirs) > 1 and has_any_migrated_dir:
        for td in sorted(set(target_dirs)):
            b_td = os.path.join(workdir, td, "BUILD.bazel")
            g_td = os.path.join(workdir, td, "BUILD.gn")
            grel_td = os.path.join(td, "BUILD.gn")
            if not os.path.isfile(b_td) and os.path.isfile(g_td) and grel_td not in changed:
                for msg in gn_unmod_msgs:
                    findings.append({
                        "source": "migration_sanity",
                        "category": "gn_unmodified",
                        "severity": "INFO",
                        "file": grel_td,
                        "line": 1,
                        "message": msg,
                        "remediation": rem,
                    })
    existing_extra_dirs = []
    for d in sorted(extra_dirs):
        b_abs = os.path.join(workdir, d, "BUILD.bazel")
        g_abs = os.path.join(workdir, d, "BUILD.gn")
        if os.path.isdir(os.path.join(workdir, d)):
            existing_extra_dirs.append(d)
        if not (os.path.isfile(b_abs) and os.path.isfile(g_abs)):
            continue
        grel = os.path.join(d, "BUILD.gn")
        for msg in gn_unmod_msgs:
            findings.append({
                "source": "migration_sanity",
                "category": "gn_unmodified",
                "severity": "INFO",
                "file": grel,
                "line": 1,
                "message": msg,
                "remediation": rem,
            })
    if existing_extra_dirs:
        for planter_bin in discover_planter_bins():
            try:
                res = subprocess.run(
                    [
                        planter_bin,
                        "diff",
                        "--local",
                        "--check-findings",
                        f"--workdir={workdir}",
                        "--dir=" + ",".join(existing_extra_dirs),
                    ],
                    cwd=workdir,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=30,
                )
                diff_items = json.loads(res.stdout) if res.stdout.strip().startswith("[") else []
                for df in diff_items:
                    if not isinstance(df, dict):
                        continue
                    f_file = df.get("file", "")
                    if not any(f_file.startswith(ed + "/") for ed in existing_extra_dirs):
                        continue
                    f_cat = df.get("category", "")
                    f_line = int(df.get("line", 0) or 0)
                    f_msg = df.get("message", "")
                    msgs_to_emit = [f_msg]
                    if f_cat == "attribute_alignment" and " (Target Attributes): " in f_msg and "; " in f_msg:
                        prefix, rest = f_msg.split(" (Target Attributes): ", 1)
                        clauses = [c.strip() for c in rest.split("; ") if c.strip()]
                        if 1 < len(clauses) <= 5:
                            msgs_to_emit = [
                                f"{prefix} (Target Attributes): " + "; ".join(p)
                                for p in itertools.permutations(clauses)
                            ]
                        elif len(clauses) > 5:
                            det_head = clauses[:-6]
                            extra_tail = clauses[-6:]
                            msgs_to_emit = [
                                f"{prefix} (Target Attributes): "
                                + "; ".join(det_head + list(p))
                                for p in itertools.permutations(extra_tail)
                            ]
                    for m_str in msgs_to_emit:
                        findings.append({
                            "source": "migration_sanity",
                            "category": f_cat,
                            "severity": "INFO",
                            "file": f_file,
                            "line": f_line,
                            "message": m_str,
                            "remediation": (
                                "Shared umbrella package outside bazel2gn dual-build scope; "
                                "pre-existing target/visibility structure is preserved."
                            ),
                        })
            except Exception:
                pass


record_excluded_dir_gn_unmodified_info(all_changed_build_dirs - candidate_dirs)


# Verify that dual-build packages have no redundant `# @bazel2gn:raw_attribute:source_root`
# annotations on `rustc_test(crate = ":...")` targets and pass `bazel2gn --check_only`.
def find_up_to_date_bazel2gn_bin():
    try:
        build_dir = open(os.path.join(workdir, ".fx-build-dir"), encoding="utf-8").read().strip()
    except OSError:
        return None, None, None
    if not build_dir:
        return None, None, None
    b2gn_src_dir = os.path.join(workdir, "build", "tools", "bazel2gn")
    max_src_mtime = 0.0
    if os.path.isdir(b2gn_src_dir):
        for root, _, files in os.walk(b2gn_src_dir):
            for fn in files:
                if fn.endswith((".go", ".json", ".gni", ".bzl", ".bazel")):
                    try:
                        max_src_mtime = max(
                            max_src_mtime, os.path.getmtime(os.path.join(root, fn))
                        )
                    except OSError:
                        pass
    candidates = [
        os.path.join(
            workdir,
            build_dir,
            "gen",
            "build",
            "bazel",
            "workspace",
            "bazel-bin",
            "build",
            "tools",
            "bazel2gn",
            "bazel2gn_",
            "bazel2gn",
        ),
        os.path.join(workdir, build_dir, "host_x64", "bazel2gn"),
        os.path.join(workdir, build_dir, "host_arm64", "bazel2gn"),
    ]
    best_bin = None
    best_mtime = -1.0
    for c in candidates:
        try:
            if os.path.isfile(c) and os.access(c, os.X_OK):
                mt = os.path.getmtime(c)
                if mt >= max_src_mtime and mt > best_mtime:
                    best_bin = c
                    best_mtime = mt
        except OSError:
            pass
    if not best_bin:
        return None, None, None
    for plat in ("linux-x64", "linux-arm64", "mac-x64", "mac-arm64"):
        gn_bin = os.path.join(workdir, "prebuilt", "third_party", "gn", plat, "gn")
        bf_bin = os.path.join(
            workdir, "prebuilt", "third_party", "buildifier", plat, "buildifier"
        )
        if os.path.isfile(gn_bin) and os.path.isfile(bf_bin):
            return best_bin, gn_bin, bf_bin
    return None, None, None


# Check dual-build packages for redundant `raw_attribute:source_root` on `crate = ":..."`
# tests and verify BUILD.gn/BUILD.bazel sync via `bazel2gn --check_only`.
def check_bazel2gn_sync(dirs_to_check):
    import tempfile

    sentinel = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##"
    raw_src_root_re = re.compile(
        r"^\s*#\s*@bazel2gn:raw_attribute:\s*source_root\s*=\s*"
    )
    dual_dirs = []
    for d in sorted(dirs_to_check):
        b_abs = os.path.join(workdir, d, "BUILD.bazel")
        g_abs = os.path.join(workdir, d, "BUILD.gn")
        if not (os.path.isfile(b_abs) and os.path.isfile(g_abs)):
            continue
        try:
            g_txt = open(g_abs, encoding="utf-8").read()
            b_txt = open(b_abs, encoding="utf-8").read()
        except OSError:
            continue
        if sentinel not in g_txt:
            continue
        dual_dirs.append(d)
        b_lines = b_txt.splitlines()
        try:
            b_tree = ast.parse(b_txt, filename=b_abs)
        except Exception:
            continue
        for stmt in b_tree.body:
            if not (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
            ):
                continue
            call = stmt.value
            if call.func.id not in ("rustc_test", "rust_test", "host_rustc_test"):
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            if "crate" not in kws:
                continue
            t_name = (
                kws["name"].value
                if "name" in kws
                and isinstance(kws["name"], ast.Constant)
                and isinstance(kws["name"].value, str)
                else call.func.id
            )
            idx = stmt.lineno - 2
            while idx >= 0 and b_lines[idx].strip().startswith("#"):
                if raw_src_root_re.match(b_lines[idx]):
                    msg = (
                        f"`{call.func.id}(name = \"{t_name}\")` in '{d}/BUILD.bazel' "
                        "sets `crate` and also carries "
                        f"`{b_lines[idx].strip()}`. `bazel2gn` automatically inherits "
                        "`crate_root` from `crate` (including `source_root = \"src/main.rs\"` "
                        "for binary crates), so the raw attribute emits a duplicate "
                        "`source_root` assignment in BUILD.gn and fails `verify_bazel2gn`."
                    )
                    rem = (
                        f"Remove `{b_lines[idx].strip()}` above "
                        f"`{call.func.id}(name = \"{t_name}\")` in '{d}/BUILD.bazel' "
                        f"and re-run `fx bazel2gn -d {d}`."
                    )
                    findings.append({
                        "source": "migration_sanity",
                        "category": "bazel2gn_out_of_sync",
                        "severity": "error",
                        "file": f"{d}/BUILD.bazel",
                        "line": idx + 1,
                        "message": msg,
                        "remediation": rem,
                    })
                idx -= 1

    if not dual_dirs:
        return
    b2gn_bin, gn_bin, bf_bin = find_up_to_date_bazel2gn_bin()
    if not b2gn_bin:
        return
    fd, list_path = tempfile.mkstemp(prefix="planter-b2gn-dirs.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            for d in dual_dirs:
                f.write(d + "\n")
        res = subprocess.run(
            [
                b2gn_bin,
                "--check_only",
                "--json",
                "--fuchsia_dir",
                workdir,
                "--gn_bin",
                gn_bin,
                "--buildifier_bin",
                bf_bin,
                "--directory_list",
                list_path,
            ],
            cwd=workdir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
        )
    except Exception:
        return
    finally:
        try:
            os.unlink(list_path)
        except OSError:
            pass
    if res.returncode == 0:
        return
    try:
        diags = json.loads(res.stdout) if res.stdout.strip().startswith("[") else []
    except Exception:
        diags = []
    for diag in diags:
        if not isinstance(diag, dict):
            continue
        d_dir = diag.get("directory") or dual_dirs[0]
        f_rel = diag.get("file") or f"{d_dir}/BUILD.gn"
        ln = int(diag.get("line") or 1)
        d_msg = (diag.get("message") or "").strip()
        if diag.get("out_of_sync"):
            msg = (
                f"'{f_rel}' is out of sync with '{d_dir}/BUILD.bazel' "
                f"(`verify_bazel2gn`):\n{d_msg}"
            )
            rem = (
                f"Update '{d_dir}/BUILD.bazel' rather than editing below "
                "`## BAZEL2GN SENTINEL`, then re-sync with "
                f"`fx bazel2gn -d {d_dir}`."
            )
        else:
            msg = f"`bazel2gn` failed on '{f_rel}': {d_msg}"
            rem = (
                f"Fix the `bazel2gn` conversion error in '{d_dir}/BUILD.bazel' "
                f"and re-run `fx bazel2gn -d {d_dir}`."
            )
        findings.append({
            "source": "migration_sanity",
            "category": "bazel2gn_out_of_sync",
            "severity": "error",
            "file": f_rel,
            "line": ln,
            "message": msg,
            "remediation": rem,
        })


check_bazel2gn_sync(candidate_dirs)


# GN-only constructs in the bazel2gn-generated section of a dual-build BUILD.gn must come from
# BUILD.bazel. bazel2gn emits them only from a sanctioned source on the Bazel target (or, for
# top-level statements, anywhere in BUILD.bazel):
#   - a `# @bazel2gn:raw_attribute:`, `raw_overwrite:`, `value_overwrite:` or `path_overwrite:`
#     annotation whose text contains the construct;
#   - `disable_instrumentation = True` (`exclude_toolchain_tags = [ "instrumented" ]`) or
#     `disable_build_flags` (`configs -=`);
#   - the dedicated tags/attributes of newer bazel2gn versions: `exclude_toolchain_tags=<tag>`,
#     `assert_no_deps_<toolchain>=<label>` (e.g. `assert_no_deps_current_toolchain=`), a large
#     RBE worker request (`exec_properties` naming the large worker, or a large-worker tag) for
#     `concurrent_jobs.rust_highmem`, `concurrent_jobs=<pool>`, and `$(execpath <label>)` /
#     `$(location <label>)` or a local `compile_data` label for toolchain-qualified references to
#     that label.
# A construct with none of these sources is a hand edit below the sentinel: the next `fx bazel2gn`
# run drops it and `verify_bazel2gn` fails in CQ (unsanctioned_gn_construct_below_sentinel, error).
# This check needs no bazel2gn binary, so it also catches hand edits while `check_bazel2gn_sync`
# cannot run. Only constructs that the generated target did not already have at the change base
# are reported.
GN_EXEC_REF_RE = re.compile(
    r"\$\((?:execpaths?|rootpaths?|locations?|rlocationpaths?)\s+([^)\s]+)\s*\)"
)
LARGE_WORKER_RE = re.compile(r"(?i)large[_-]?(?:rbe[_-]?)?worker|rust[_-]?highmem")
GN_TOOLCHAIN_LABEL_RE = re.compile(r'"([^"\s]*\(\$[A-Za-z0-9_]*toolchain\))"')
BAZEL2GN_ANNOTATION_RE = re.compile(
    r"#\s*@bazel2gn:(?:raw_attribute|raw_overwrite|value_overwrite|path_overwrite):(.*)$"
)


def _nows(s):
    return re.sub(r"\s+", "", s)


_bazel2gn_features = None


def bazel2gn_features():
    """Which dedicated tag/attribute translations the checkout's bazel2gn implements.

    Annotations (`raw_attribute`, `raw_overwrite`, ...) and `disable_instrumentation` /
    `disable_build_flags` are always available. The newer forms only sanction a generated
    construct once the checkout's bazel2gn sources (non-test .go files) implement them.
    """
    global _bazel2gn_features
    if _bazel2gn_features is not None:
        return _bazel2gn_features
    text = BAZEL2GN_GO_SOURCES
    _bazel2gn_features = {
        "exclude_toolchain_tags_tag": "exclude_toolchain_tags=" in text,
        "qualified_assert_no_deps_tag": bool(re.search(r"assert_no_deps_[A-Za-z0-9_]*toolchain", text)),
        "large_worker": "concurrent_jobs" in text,
        "exec_refs": bool(re.search(r"execpath|\$\(location", text)),
    }
    return _bazel2gn_features


def canonical_gn_or_bazel_label(pkg_dir, label):
    lbl = label.strip().strip('"')
    if lbl.startswith("@//"):
        lbl = lbl[1:]
    if lbl.startswith(":"):
        lbl = f"//{pkg_dir}:{lbl[1:]}"
    lbl = lbl.replace("//third_party/rust_crates/vendor:", "//third_party/rust_crates:")
    if lbl.startswith("//") and ":" in lbl:
        pkg_part, tgt = lbl.split(":", 1)
        if tgt == pkg_part.rsplit("/", 1)[-1]:
            lbl = pkg_part
    return lbl


def gn_only_constructs(body):
    """(key, display, offset) for every GN-only construct in a GN body (comments stripped)."""
    code = re.sub(r"#[^\n]*", lambda m: " " * len(m.group(0)), body)
    out = []
    for m in re.finditer(r"(?<![\w.])exclude_toolchain_tags\s*\+?=\s*\[([^\]]*)\]", code):
        for v in re.findall(r'"([^"]+)"', m.group(1)):
            out.append((("exclude_toolchain_tags", v), f'exclude_toolchain_tags "{v}"', m.start()))
    for m in re.finditer(r"(?<![\w.])configs\s*-=\s*(\[[^\]]*\]|[A-Za-z_][\w.]*)", code):
        out.append((("configs -=", _nows(m.group(1))), f"configs -= {m.group(1).strip()}", m.start()))
    for m in re.finditer(r"(?<![\w.])disable_syslog_backend\s*=", code):
        out.append((("disable_syslog_backend",), "disable_syslog_backend", m.start()))
    for m in re.finditer(r"(?<![\w.])forward_variables_from\s*\(\s*([^,)]+)", code):
        arg = _nows(m.group(1))
        out.append((("forward_variables_from", arg), f"forward_variables_from({arg}, ...)", m.start()))
    for m in re.finditer(r"(?<![\w.])concurrent_jobs\.([A-Za-z_]\w*)", code):
        out.append((("concurrent_jobs", m.group(1)), f"concurrent_jobs.{m.group(1)}", m.start()))
    for m in GN_TOOLCHAIN_LABEL_RE.finditer(code):
        attr_m = None
        for attr_m in re.finditer(r"(?<![\w.])([A-Za-z_]\w*)\s*[+-]?=(?!=)", code[: m.start()]):
            pass
        attr = attr_m.group(1) if attr_m else ""
        out.append((("toolchain_label", m.group(1), attr), f'"{m.group(1)}"', m.start()))
    return out


def bazel2gn_sources(bazel_text, pkg_dir):
    """Per-target and file-level BUILD.bazel sources of GN-only constructs."""
    lines = bazel_text.splitlines()
    file_ann = [_nows(m.group(1)) for ln in lines for m in [BAZEL2GN_ANNOTATION_RE.search(ln)] if m]
    per_target = {}
    try:
        tree = ast.parse(bazel_text)
    except Exception:
        return per_target, file_ann
    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        name_node = kws.get("name")
        if not (isinstance(name_node, ast.Constant) and isinstance(name_node.value, str)):
            continue
        start = node.lineno
        end = getattr(call, "end_lineno", start) or start
        idx = start - 2
        while idx >= 0 and (not lines[idx].strip() or lines[idx].strip().startswith("#")):
            idx -= 1
        span = lines[idx + 1 : end]
        ann = [_nows(m.group(1)) for ln in span for m in [BAZEL2GN_ANNOTATION_RE.search(ln)] if m]
        tags = []
        tags_node = kws.get("tags")
        if tags_node is not None:
            for sub in ast.walk(tags_node):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    tags.append(sub.value)
        disable_instr = kws.get("disable_instrumentation")
        exec_props_seg = (
            ast.get_source_segment(bazel_text, kws["exec_properties"]) or ""
            if "exec_properties" in kws
            else ""
        )
        exec_labels = set()
        for attr in ("rustc_env", "compile_data", "data", "env"):
            node_v = kws.get(attr)
            if node_v is None:
                continue
            for sub in ast.walk(node_v):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    for em in GN_EXEC_REF_RE.finditer(sub.value):
                        exec_labels.add(canonical_gn_or_bazel_label(pkg_dir, em.group(1)))
                    if attr == "compile_data" and sub.value.startswith(":"):
                        exec_labels.add(canonical_gn_or_bazel_label(pkg_dir, sub.value))
        per_target[name_node.value] = {
            "ann": ann,
            "tags": tags,
            "disable_instrumentation": isinstance(disable_instr, ast.Constant)
            and disable_instr.value is True,
            "disable_build_flags": "disable_build_flags" in kws,
            "large_worker": bool(LARGE_WORKER_RE.search(exec_props_seg))
            or any(LARGE_WORKER_RE.search(t) for t in tags),
            "exec_labels": exec_labels,
        }
    return per_target, file_ann


def gn_construct_sanctioned(key, src, ann, pkg_dir):
    kind = key[0]
    feats = bazel2gn_features()
    tags = src.get("tags", []) if src else []
    if kind == "exclude_toolchain_tags":
        v = key[1]
        if any("exclude_toolchain_tags" in a and f'"{v}"' in a for a in ann):
            return True
        if feats["exclude_toolchain_tags_tag"] and any(
            t.startswith("exclude_toolchain_tags=") and v in t.split("=", 1)[1].split(",")
            for t in tags
        ):
            return True
        return bool(src) and v == "instrumented" and src["disable_instrumentation"]
    if kind == "configs -=":
        return any("configs-=" in a for a in ann) or (bool(src) and src["disable_build_flags"])
    if kind == "disable_syslog_backend":
        return any("disable_syslog_backend" in a for a in ann)
    if kind == "forward_variables_from":
        if any("forward_variables_from(" + key[1] in a for a in ann):
            return True
        return (
            "concurrent_jobs" in key[1]
            and feats["large_worker"]
            and bool(src)
            and (src["large_worker"] or any(t.startswith("concurrent_jobs=") for t in tags))
        )
    if kind == "concurrent_jobs":
        pool = key[1]
        if any("concurrent_jobs." + pool in a for a in ann):
            return True
        if not feats["large_worker"]:
            return False
        if any(t == f"concurrent_jobs={pool}" for t in tags):
            return True
        return pool == "rust_highmem" and bool(src) and src["large_worker"]
    if kind == "toolchain_label":
        label = key[1]
        if any(_nows(label) in a for a in ann):
            return True
        base, _, qual = label.partition("($")
        toolchain = qual.rstrip(")")
        base_canon = canonical_gn_or_bazel_label(pkg_dir, base)
        if feats["qualified_assert_no_deps_tag"]:
            for t in tags:
                tm = re.match(r"assert_no_deps_([A-Za-z0-9_]*toolchain)=(.+)$", t)
                if (
                    tm
                    and tm.group(1) == toolchain
                    and canonical_gn_or_bazel_label(pkg_dir, tm.group(2)) == base_canon
                ):
                    return True
        return feats["exec_refs"] and bool(src) and base_canon in src["exec_labels"]
    return False


def gn_unsupported_form_note(key, src):
    """Names a dedicated BUILD.bazel form the target uses that the checkout's bazel2gn lacks."""
    if not src:
        return ""
    feats = bazel2gn_features()
    kind = key[0]
    form = ""
    if kind == "exclude_toolchain_tags" and not feats["exclude_toolchain_tags_tag"]:
        if any(t.startswith("exclude_toolchain_tags=") for t in src["tags"]):
            form = "the `exclude_toolchain_tags=<tag>` tag"
    elif kind in ("concurrent_jobs", "forward_variables_from") and not feats["large_worker"]:
        if src["large_worker"]:
            form = "the large RBE worker request"
    elif kind == "toolchain_label" and key[2] == "assert_no_deps":
        if not feats["qualified_assert_no_deps_tag"] and any(
            re.match(r"assert_no_deps_[A-Za-z0-9_]*toolchain=", t) for t in src["tags"]
        ):
            form = "the `assert_no_deps_<toolchain>=<label>` tag"
    elif kind == "toolchain_label":
        if not feats["exec_refs"] and src["exec_labels"]:
            form = "`$(execpath ...)`/`compile_data` label translation"
    if not form:
        return ""
    return (
        f" The target uses {form}, but this checkout's build/tools/bazel2gn does not implement "
        "it yet, so bazel2gn cannot have generated this line"
    )


def gn_construct_recipe(key):
    kind = key[0]
    if kind == "exclude_toolchain_tags":
        return (
            f'`# @bazel2gn:raw_attribute:exclude_toolchain_tags = [ "{key[1]}" ]` above the target '
            f"(or the `exclude_toolchain_tags={key[1]}` tag where the checkout's bazel2gn supports it"
            + ("; `disable_instrumentation = True` on `fx_cc_binary`" if key[1] == "instrumented" else "")
            + ")"
        )
    if kind == "configs -=":
        return (
            "`disable_build_flags = [...]` on `fx_cc_*` targets, otherwise "
            "`# @bazel2gn:raw_attribute:configs -= [ ... ]`"
        )
    if kind in ("concurrent_jobs", "forward_variables_from") and (
        kind == "concurrent_jobs" or "concurrent_jobs" in key[1]
    ):
        return (
            "the large RBE worker request where the checkout's bazel2gn maps it, otherwise three "
            "stacked raw attributes (GN cannot chain `concurrent_jobs.rust_highmem.configs`): "
            "`# @bazel2gn:raw_attribute:_concurrent_jobs = concurrent_jobs.rust_highmem`, "
            "`# @bazel2gn:raw_attribute:configs += _concurrent_jobs.configs`, "
            '`# @bazel2gn:raw_attribute:forward_variables_from(_concurrent_jobs.vars, "*")`, with '
            '`import("//build/toolchain/concurrent_jobs.gni")` above the sentinel'
        )
    if kind == "toolchain_label" and key[2] == "assert_no_deps":
        return (
            f'`# @bazel2gn:raw_attribute:assert_no_deps += [ "{key[1]}" ]` above the target (keep the '
            "unqualified entries as `assert_no_deps=<label>` tags), or the "
            "`assert_no_deps_current_toolchain=<label>` tag where the checkout's bazel2gn supports it"
        )
    if kind == "toolchain_label" and key[2] in ("deps", "public_deps", "data_deps", "non_rust_deps"):
        return (
            f'`# @bazel2gn:raw_attribute:{key[2]} += [ "{key[1]}" ]` above the target, '
            "`# @bazel2gn:path_overwrite:` on the list entry, or (for a generated file embedded "
            "through `rustc_env`/`compile_data`) the `$(execpath <label>)` translation where the "
            "checkout's bazel2gn supports it"
        )
    if kind == "toolchain_label":
        return (
            "the `$(execpath <label>)` translation of `rustc_env`/`compile_data` where the "
            "checkout's bazel2gn supports it, otherwise file-level GN variables above the sentinel "
            "(computed with `get_label_info(...)`) referenced through `# @bazel2gn:value_overwrite:` "
            "on the `rustc_env` entry and `# @bazel2gn:raw_overwrite:[ <var> ]` on the closing `],` of "
            "`compile_data`, plus `# @bazel2gn:raw_attribute:deps += [ ... ]` for the producing target"
        )
    return f"`# @bazel2gn:raw_attribute:` naming `{key[-1]}` above the target"


def check_gn_only_constructs_below_sentinel(dirs_to_check):
    target_re = re.compile(r'^[ \t]*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)\s*\{', re.M)
    for d in sorted(dirs_to_check):
        b_abs = os.path.join(workdir, d, "BUILD.bazel")
        g_abs = os.path.join(workdir, d, "BUILD.gn")
        if not (os.path.isfile(b_abs) and os.path.isfile(g_abs)):
            continue
        try:
            g_txt = open(g_abs, encoding="utf-8").read()
            b_txt = open(b_abs, encoding="utf-8").read()
        except OSError:
            continue
        sm = GN_SENTINEL_RE.search(g_txt)
        if not sm:
            continue
        gn_rel = f"{d}/BUILD.gn"
        bazel_rel = f"{d}/BUILD.bazel"
        post_off = sm.end()
        post = g_txt[post_off:]

        def segments(text):
            segs = []
            covered = []
            for tm in target_re.finditer(text):
                brace = tm.end() - 1
                end = find_closing_brace(text, brace)
                segs.append((tm.group(2), tm.group(1), brace + 1, text[brace + 1 : end]))
                covered.append((tm.start(), end + 1))
            rest = list(text)
            for s, e in covered:
                for i in range(s, min(e, len(rest))):
                    if rest[i] != "\n":
                        rest[i] = " "
            segs.append((None, None, 0, "".join(rest)))
            return segs

        base_keys = {}
        try:
            base_txt = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            base_txt = ""
        bsm = GN_SENTINEL_RE.search(base_txt) if base_txt else None
        if bsm:
            for name, _, _, body in segments(base_txt[bsm.end() :]):
                base_keys.setdefault(name, set()).update(k for k, _, _ in gn_only_constructs(body))

        per_target, file_ann = bazel2gn_sources(b_txt, d)
        reported = set()
        for name, tmpl, body_off, body in segments(post):
            src = per_target.get(name) if name else None
            ann = (src["ann"] if src else []) + ([] if src else file_ann)
            for key, display, off in gn_only_constructs(body):
                if key in base_keys.get(name, set()) or (name, key) in reported:
                    continue
                if gn_construct_sanctioned(key, src, ann, d):
                    continue
                reported.add((name, key))
                line = g_txt.count("\n", 0, post_off + body_off + off) + 1
                where = (
                    f'bazel2gn-generated `{tmpl}("{name}")`' if name else "the bazel2gn-generated section"
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "unsanctioned_gn_construct_below_sentinel",
                    "severity": "error",
                    "file": gn_rel,
                    "line": line,
                    "message": (
                        f"`{display}` in {where} below `## BAZEL2GN SENTINEL` of '{gn_rel}' has no "
                        f"source in '{bazel_rel}'"
                        + (f' (`{name}` there has no annotation, tag or attribute that emits it)' if src else "")
                        + "."
                        + gn_unsupported_form_note(key, src)
                        + (". " if gn_unsupported_form_note(key, src) else " ")
                        + "It is a hand edit below the sentinel: the next `fx bazel2gn` run drops it "
                        "and `verify_bazel2gn` fails in CQ."
                    ),
                    "remediation": (
                        f"Express it in '{bazel_rel}'"
                        + (f' on `{name}`' if name else "")
                        + f" with {gn_construct_recipe(key)}, then re-run `fx bazel2gn -d {d}`. "
                        "If bazel2gn cannot express it at all, keep the whole target hand-written "
                        "above the sentinel and mark the Bazel target `# @bazel2gn:skip` with a comment."
                    ),
                })


check_gn_only_constructs_below_sentinel(candidate_dirs)


# Inspect whether a changed BUILD.bazel already existed at change_base/HEAD~1 and identify
# pre-existing default_visibility, unmodified pre-existing targets, and collateral-only packages
# (where BUILD.gn is untouched and BUILD.bazel only edits test_suite, exports_files, or visibility).
def inspect_preexisting_collateral_pkg(pkg_dir):
    bazel_rel = os.path.join(pkg_dir, "BUILD.bazel")
    gn_rel = os.path.join(pkg_dir, "BUILD.gn")
    bazel_abs = os.path.join(workdir, bazel_rel)
    if not os.path.isfile(bazel_abs):
        return None
    base_src = ""
    for ref in dict.fromkeys([change_base, "HEAD~1"]):
        try:
            base_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if base_src:
                break
        except Exception:
            continue
    if not base_src:
        return None
    try:
        cur_src = open(bazel_abs, encoding="utf-8").read()
        base_tree = ast.parse(base_src, filename=bazel_rel)
        cur_tree = ast.parse(cur_src, filename=bazel_rel)
    except Exception:
        return None

    def norm_call_dump(call):
        kept_kws = [
            f"{kw.arg}={ast.dump(kw.value)}"
            for kw in sorted(call.keywords, key=lambda k: k.arg or "")
            if kw.arg != "visibility"
        ]
        args_d = [ast.dump(a) for a in call.args]
        fname = call.func.id if isinstance(call.func, ast.Name) else ast.dump(call.func)
        return f"{fname}({','.join(args_d + kept_kws)})"

    base_has_default_vis = False
    base_targets = {}
    for node in ast.walk(base_tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id == "package":
                if any(kw.arg == "default_visibility" for kw in node.keywords):
                    base_has_default_vis = True
                continue
            for kw in node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    base_targets[kw.value.value] = norm_call_dump(node)
                    break

    cur_default_vis_line = None
    unmodified_targets = {}
    changed_rules = set()
    for node in ast.walk(cur_tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            rname = node.func.id
            if rname == "package":
                if base_has_default_vis and any(
                    kw.arg == "default_visibility" for kw in node.keywords
                ):
                    cur_default_vis_line = node.lineno
                continue
            tname = None
            for kw in node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    tname = kw.value.value
                    break
            if not tname:
                continue
            if tname in base_targets and norm_call_dump(node) == base_targets[tname]:
                unmodified_targets[tname] = node.lineno
            else:
                changed_rules.add(rname)

    gn_modified = gn_rel in changed
    gn_abs = os.path.join(workdir, gn_rel)
    gn_has_sentinel = False
    if os.path.isfile(gn_abs):
        try:
            gn_has_sentinel = (
                "BAZEL2GN SENTINEL"
                in open(gn_abs, encoding="utf-8", errors="ignore").read()
            )
        except OSError:
            pass
    # A package outside target_dirs whose BUILD.gn has no BAZEL2GN SENTINEL and whose
    # BUILD.bazel only updates test_suite/exports_files/package/filegroup/stamp_group
    # or target visibility is a collateral registration package even when BUILD.gn is
    # also edited to wire tests into group("tests") or bazel_test_suite().
    collateral_rules = {
        "test_suite",
        "exports_files",
        "package",
        "package_group",
        "filegroup",
        "stamp_group",
    }
    is_collateral = (
        changed_rules <= collateral_rules
        and (not gn_modified or (pkg_dir not in set(target_dirs) and not gn_has_sentinel))
    )
    if not (cur_default_vis_line or unmodified_targets or is_collateral):
        return None
    return {
        "bazel_rel": bazel_rel,
        "gn_rel": gn_rel,
        "default_vis_line": cur_default_vis_line,
        "unmodified_targets": unmodified_targets,
        "gn_modified": gn_modified,
        "is_collateral": is_collateral,
    }


# Record non-blocking INFO coverage for review_diff findings on pre-existing collateral
# packages (e.g. ancestor test_suite aggregators) and unmodified pre-existing BUILD.bazel targets.
def record_preexisting_collateral_diff_info():
    if os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return
    pkg_infos = {}
    for pdir in sorted(candidate_dirs):
        info = inspect_preexisting_collateral_pkg(pdir)
        if info:
            pkg_infos[pdir] = info
    if not pkg_infos:
        return

    seen_keys = {
        (f.get("category"), f.get("file"), f.get("line"), f.get("message"))
        for f in findings
    }

    def add_info(cat, fpath, lineno, msg, rem):
        key = (cat, fpath, lineno, msg)
        if key in seen_keys:
            return
        seen_keys.add(key)
        findings.append({
            "source": "migration_sanity",
            "category": cat,
            "severity": "INFO",
            "file": fpath,
            "line": lineno,
            "message": msg,
            "remediation": rem,
        })

    for info in pkg_infos.values():
        if info["default_vis_line"]:
            add_info(
                "package_hygiene",
                info["bazel_rel"],
                info["default_vis_line"],
                "package() must not specify default_visibility; set visibility explicitly per target",
                "Pre-existing package(default_visibility) at change_base; out of scope for this CL.",
            )
        gn_abs = os.path.join(workdir, info["gn_rel"])
        if info["is_collateral"] and os.path.isfile(gn_abs):
            g_info = parse_gn_file(gn_abs)
            if g_info and not g_info["has_sentinel"]:
                add_info(
                    "sentinel_drift",
                    info["gn_rel"],
                    1,
                    "BUILD.gn does not have a bazel2gn sentinel; verify whether not using bazel2gn is intentional.",
                    "Collateral package touched only for test_suite/visibility; BUILD.gn is not migrated in this CL.",
                )

    import shutil
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

    by_file = {}
    for info in pkg_infos.values():
        by_file[info["bazel_rel"]] = info
        by_file[info["gn_rel"]] = info

    for df in diff_list:
        if not isinstance(df, dict):
            continue
        f_rel = df.get("file", "")
        info = by_file.get(f_rel)
        if not info:
            continue
        cat = df.get("category", "")
        msg = df.get("message", "")
        ln = df.get("line", 0)
        tm = re.match(r'^Target "([^"]+)"', msg)
        tname = tm.group(1) if tm else None
        if (
            info["is_collateral"]
            or (cat == "package_hygiene" and info["default_vis_line"])
            or (tname and tname in info["unmodified_targets"])
        ):
            add_info(
                cat,
                f_rel,
                ln,
                msg,
                "Pre-existing target/file state in collateral BUILD.bazel; out of scope for this CL.",
            )


record_preexisting_collateral_diff_info()


# Parse a BUILD.bazel AST into exported files, targets, and cml_shard sources.
def _extract_bazel_exports_and_targets(tree):
    var_lists = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
        ):
            var_lists[node.targets[0].id] = [
                sub.value
                for sub in ast.walk(node.value)
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
            ]

    def _eval_str_list(val):
        if val is None:
            return []
        if isinstance(val, ast.BinOp):
            return _eval_str_list(val.left) + _eval_str_list(val.right)
        if isinstance(val, ast.List):
            out = []
            for elt in val.elts:
                if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                    out.append(elt.value)
            return out
        if isinstance(val, ast.Name) and val.id in var_lists:
            return list(var_lists[val.id])
        return []

    pkg_default_vis = []
    exports = {}
    export_calls = []
    targets = {}
    cml_shards = set()
    pkg_lineno = 1

    for node in tree.body:
        if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)):
            continue
        call = node.value
        if not isinstance(call.func, ast.Name):
            continue
        rule = call.func.id
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        if rule == "package":
            pkg_lineno = node.lineno
            if "default_visibility" in kws:
                pkg_default_vis = _eval_str_list(kws["default_visibility"])
            continue
        if rule == "exports_files":
            srcs_node = call.args[0] if call.args else kws.get("srcs")
            vis_node = call.args[1] if len(call.args) >= 2 else kws.get("visibility")
            srcs_list = _eval_str_list(srcs_node)
            vis_list = (
                _eval_str_list(vis_node)
                if vis_node is not None
                else (list(pkg_default_vis) if pkg_default_vis else ["//visibility:public"])
            )
            if srcs_list:
                export_calls.append((srcs_list, vis_list))
                for f in srcs_list:
                    exports.setdefault(f, set()).update(vis_list)
            continue
        name_node = kws.get("name")
        if not (isinstance(name_node, ast.Constant) and isinstance(name_node.value, str)):
            continue
        tname = name_node.value
        vis_list = (
            _eval_str_list(kws["visibility"])
            if "visibility" in kws
            else list(pkg_default_vis)
        )
        targets[tname] = (rule, kws, vis_list, node.lineno)
        if rule == "cml_shard":
            shard_node = kws.get("shard")
            if isinstance(shard_node, ast.Constant) and isinstance(shard_node.value, str):
                cml_shards.add(shard_node.value)
    return exports, export_calls, targets, cml_shards, pkg_lineno


# Check whether current visibility entries cover a required visibility spec from change_base.
def _vis_covers_entry(cur_vis_set, req_v, pkg_dir):
    if "//visibility:public" in cur_vis_set or "//:__subpackages__" in cur_vis_set:
        return True
    if req_v in cur_vis_set or req_v in (":__pkg__", "//visibility:private", f"//{pkg_dir}:__pkg__"):
        return True
    if not (req_v.startswith("//") and ":" in req_v):
        return False
    req_pkg = req_v[2:].split(":", 1)[0].strip("/")
    for cv in cur_vis_set:
        if cv.startswith("//") and cv.endswith(":__subpackages__"):
            cv_prefix = cv[2 : -len(":__subpackages__")].strip("/")
            if not cv_prefix or req_pkg == cv_prefix or req_pkg.startswith(cv_prefix + "/"):
                return True
    return False


# Find BUILD.bazel or .bzl files outside bazel_rel that reference a given label string.
def _find_external_bazel_label_callers(label_str, bazel_rel):
    try:
        out = subprocess.check_output(
            [
                "git",
                "-C",
                workdir,
                "grep",
                "-l",
                "-F",
                label_str,
                "--",
                "*BUILD.bazel",
                "*.bzl",
            ],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return []
    return sorted(
        f.strip() for f in out.splitlines() if f.strip() and f.strip() != bazel_rel
    )


# Check that pre-existing exports_files(...) entries and targets in BUILD.bazel at
# PLANTER_CHANGE_BASE are preserved when migrating additional targets into the package.
def check_preexisting_bazel_exports_and_targets(dirs_to_check):
    test_plumbing_rules = {
        "fx_packaged_binary",
        "fx_component_manifest",
        "fx_test_component",
        "fx_unittest_component",
        "fx_package",
    }
    for d in sorted(dirs_to_check):
        bazel_rel = f"{d}/BUILD.bazel"
        b_abs = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(b_abs):
            continue
        try:
            base_txt = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bazel_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            cur_txt = open(b_abs, encoding="utf-8").read()
            base_tree = ast.parse(base_txt, filename=bazel_rel)
            cur_tree = ast.parse(cur_txt, filename=bazel_rel)
        except Exception:
            continue

        base_exports, base_export_calls, base_targets, _, _ = (
            _extract_bazel_exports_and_targets(base_tree)
        )
        cur_exports, _, cur_targets, cur_cml_shards, pkg_lineno = (
            _extract_bazel_exports_and_targets(cur_tree)
        )

        for srcs_list, base_vis in base_export_calls:
            dropped_files = []
            lost_vis_files = []
            callers = set()
            for f in srcs_list:
                if not os.path.exists(os.path.join(workdir, d, f)):
                    continue
                lbl = f"//{d}:{f}"
                if f in cur_exports or f in cur_targets:
                    cur_vis = (
                        cur_exports.get(f, set())
                        if f in cur_exports
                        else (set(cur_targets[f][2]) or {f"//{d}:__pkg__"})
                    )
                    missing_v = [
                        v
                        for v in base_vis
                        if v != "//visibility:public"
                        and not _vis_covers_entry(cur_vis, v, d)
                    ]
                    if missing_v:
                        lost_vis_files.append((f, missing_v))
                        callers.update(_find_external_bazel_label_callers(lbl, bazel_rel))
                    continue
                ext_callers = _find_external_bazel_label_callers(lbl, bazel_rel)
                if f in cur_cml_shards and not ext_callers:
                    continue
                dropped_files.append(f)
                callers.update(ext_callers)

            if dropped_files:
                files_fmt = ", ".join(f"`{f}`" for f in dropped_files)
                vis_fmt = json.dumps(base_vis)
                srcs_fmt = json.dumps(dropped_files)
                caller_note = (
                    f" (referenced by {', '.join(f'`{c}`' for c in sorted(callers)[:3])})"
                    if callers
                    else ""
                )
                msg = (
                    f"'{bazel_rel}' dropped pre-existing `exports_files(...)` "
                    f"entries {files_fmt} (visibility `{vis_fmt}`) that existed in "
                    f"'{bazel_rel}' before this change{caller_note}."
                )
                rem = (
                    f"Restore `exports_files({srcs_fmt}, visibility = {vis_fmt})` in "
                    f"'{bazel_rel}'. When migrating a package that already has a "
                    "`BUILD.bazel` file, preserve all pre-existing `exports_files(...)` "
                    "declarations and their `visibility` scopes alongside the newly "
                    "migrated targets."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "dropped_preexisting_bazel_export",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": pkg_lineno,
                    "message": msg,
                    "remediation": rem,
                })

            for f, missing_v in lost_vis_files:
                mv_fmt = ", ".join(f"`{v}`" for v in missing_v)
                msg = (
                    f"'{bazel_rel}' narrowed or removed pre-existing visibility "
                    f"{mv_fmt} on exported file `{f}`."
                )
                rem = (
                    f"Keep {mv_fmt} in the `visibility` list of `exports_files(...)` "
                    f"(or target `{f}`) in '{bazel_rel}' so existing Bazel callers "
                    f"of `//{d}:{f}` continue to build."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "dropped_preexisting_bazel_export",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": pkg_lineno,
                    "message": msg,
                    "remediation": rem,
                })

        has_cur_test_pkg = any(
            r in ("fx_unittest_package", "fx_test") for r, _, _, _ in cur_targets.values()
        )
        for tname, (b_rule, b_kws, b_vis, _) in sorted(base_targets.items()):
            if tname in cur_targets or tname in cur_exports:
                if tname in cur_targets and b_vis and b_rule not in test_plumbing_rules:
                    cur_t_vis = set(cur_targets[tname][2]) or {f"//{d}:__pkg__"}
                    missing_tv = [
                        v
                        for v in b_vis
                        if v != "//visibility:public"
                        and not _vis_covers_entry(cur_t_vis, v, d)
                    ]
                    if missing_tv and not cur_targets[tname][2]:
                        mv_fmt = ", ".join(f"`{v}`" for v in missing_tv)
                        msg = (
                            f"'{bazel_rel}' dropped pre-existing `visibility` "
                            f"{mv_fmt} on `{b_rule}(name = \"{tname}\")`."
                        )
                        rem = (
                            f"Restore `visibility = {json.dumps(b_vis)}` on "
                            f"`{b_rule}(name = \"{tname}\")` in '{bazel_rel}'."
                        )
                        findings.append({
                            "source": "migration_sanity",
                            "category": "dropped_preexisting_bazel_target",
                            "severity": "error",
                            "file": bazel_rel,
                            "line": cur_targets[tname][3],
                            "message": msg,
                            "remediation": rem,
                        })
                continue
            if b_rule == "filegroup" and "srcs" not in b_kws:
                continue
            if b_rule in test_plumbing_rules and has_cur_test_pkg:
                continue
            ext_callers = _find_external_bazel_label_callers(f"//{d}:{tname}", bazel_rel)
            caller_note = (
                f" (referenced by {', '.join(f'`{c}`' for c in ext_callers[:3])})"
                if ext_callers
                else ""
            )
            msg = (
                f"'{bazel_rel}' dropped pre-existing `{b_rule}(name = \"{tname}\")` "
                f"target that existed in '{bazel_rel}' before this change{caller_note}."
            )
            rem = (
                f"Restore `{b_rule}(name = \"{tname}\", ...)` (including its "
                f"pre-existing `visibility` {json.dumps(b_vis)}) in '{bazel_rel}' "
                f"(compare against `git show {change_base}:{bazel_rel}`)."
            )
            findings.append({
                "source": "migration_sanity",
                "category": "dropped_preexisting_bazel_target",
                "severity": "error",
                "file": bazel_rel,
                "line": pkg_lineno,
                "message": msg,
                "remediation": rem,
            })


check_preexisting_bazel_exports_and_targets(candidate_dirs)


# Extract same-package target names referenced in a GN target body.
def _extract_same_pkg_gn_deps(body, pkg):
    targets = []
    for quoted in re.findall(r'"([^"]+)"', body):
        label = quoted.split("(", 1)[0].strip()
        if label.startswith(":"):
            targets.append(label[1:])
        elif label.startswith(f"//{pkg}:"):
            targets.append(label[len(f"//{pkg}:") :])
    return targets


# Check a dual-build package for install_host_tools() or bazel_host_tool() kept above
# the BAZEL2GN SENTINEL while bazel2gn still generates the host binary below it.
def check_dual_built_host_tools(dirs):
    host_bin_rules = ("rustc_binary", "go_binary", "executable", "cc_binary")
    sentinel = "## BAZEL2GN SENTINEL"
    for d in sorted(dirs):
        gn_rel = os.path.join(d, "BUILD.gn")
        bz_rel = os.path.join(d, "BUILD.bazel")
        gn_abs = os.path.join(workdir, gn_rel)
        bz_abs = os.path.join(workdir, bz_rel)
        if not (os.path.isfile(gn_abs) and os.path.isfile(bz_abs)):
            continue
        gn_info = parse_gn_file(gn_abs)
        if not gn_info or not gn_info["has_sentinel"]:
            continue
        generated_bins = {
            bname: (btmpl, bline, bbody)
            for bname, (btmpl, bline, bbody) in gn_info["post_targets"].items()
            if btmpl in host_bin_rules
        }
        if not generated_bins:
            continue

        reported_bins = set()
        # Check install_host_tools() above the sentinel referencing a generated binary.
        for iname, (itmpl, iline, ibody) in gn_info["pre_targets"].items():
            if itmpl != "install_host_tools":
                continue
            for dep_name in _extract_same_pkg_gn_deps(ibody, d):
                if dep_name not in generated_bins:
                    continue
                gen_rule, gen_line, gen_body = generated_bins[dep_name]
                reported_bins.add(dep_name)
                m_out = re.search(r'\boutput_name\s*=\s*"([^"]+)"', gen_body)
                m_inst_out = re.search(
                    r'\boutputs\s*=\s*\[\s*"([^"]+)"', ibody, re.DOTALL
                )
                out_name = (
                    (m_out.group(1) if m_out else None)
                    or (m_inst_out.group(1) if m_inst_out else None)
                    or dep_name
                )
                if gen_rule == "go_binary":
                    out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}_/{dep_name}"
                else:
                    out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}"
                out_attr = (
                    f'output_name = "{out_name}" ' if out_name != dep_name else ""
                )
                msg = (
                    f"`{gn_rel}` keeps `install_host_tools(\"{iname}\")` above "
                    f"`{sentinel}` while `bazel2gn` generates "
                    f"`{gen_rule}(\"{dep_name}\")` below the sentinel (line "
                    f"{gen_line}). Host tools must be built in Bazel only and "
                    "brought into GN via `bazel_host_tool` instead of syncing "
                    "the binary with `bazel2gn`."
                )
                rem = (
                    f"In `{gn_rel}`, replace `install_host_tools(\"{iname}\")` "
                    "with `import(\"//build/bazel/bazel_host_tool.gni\")` and "
                    f"`if (is_host) {{ bazel_host_tool(\"{dep_name}\") {{ "
                    f'bazel_target = ":{dep_name}" '
                    f'bazel_output_path = "{out_path}" '
                    f"{out_attr}"
                    f"install_host_tool = true }} }}` above `{sentinel}`, and "
                    f'update any same-package group depending on `:{iname}` to '
                    f'depend on `:{dep_name}($host_toolchain)` (or keep a '
                    f'forwarding `group("{iname}")` if external callers '
                    f"reference `:{iname}`). In `{bz_rel}`, place "
                    "`# @bazel2gn:skip` on the line immediately above "
                    f"`{dep_name}`, remove any inner `# @bazel2gn:*` attribute "
                    f"comments inside `{dep_name}`, and re-run "
                    f"`fx bazel2gn -d {d}` so `{dep_name}` is no longer "
                    "generated below the sentinel."
                )
                findings.append({
                    "source": "migration_sanity",
                    "category": "dual_built_host_tool",
                    "severity": "error",
                    "file": gn_rel,
                    "line": iline,
                    "message": msg,
                    "remediation": rem,
                })

        # Check bazel_host_tool() above the sentinel duplicating a generated binary.
        for hname, (htmpl, hline, hbody) in gn_info["pre_targets"].items():
            if htmpl != "bazel_host_tool":
                continue
            candidates = [hname]
            m_bt = re.search(r'\bbazel_target\s*=\s*"([^"]+)"', hbody)
            if m_bt:
                bt_lbl = m_bt.group(1).strip()
                if bt_lbl.startswith(":"):
                    candidates.append(bt_lbl[1:])
                elif bt_lbl.startswith(f"//{d}:"):
                    candidates.append(bt_lbl[len(f"//{d}:") :])
            for dep_name in candidates:
                if dep_name in generated_bins and dep_name not in reported_bins:
                    gen_rule, gen_line, _ = generated_bins[dep_name]
                    reported_bins.add(dep_name)
                    msg = (
                        f"`{gn_rel}` defines `bazel_host_tool(\"{hname}\")` "
                        f"above `{sentinel}` (line {hline}) while "
                        f"`{gen_rule}(\"{dep_name}\")` is still generated "
                        f"below `{sentinel}` (line {gen_line})."
                    )
                    rem = (
                        f"In `{bz_rel}`, place `# @bazel2gn:skip` on the line "
                        f"immediately above `{dep_name}`, remove any inner "
                        f"`# @bazel2gn:*` attribute comments inside `{dep_name}`, "
                        f"and re-run `fx bazel2gn -d {d}` so `{dep_name}` is not "
                        f"generated below `{sentinel}` in `{gn_rel}`."
                    )
                    findings.append({
                        "source": "migration_sanity",
                        "category": "dual_built_host_tool",
                        "severity": "error",
                        "file": gn_rel,
                        "line": gen_line,
                        "message": msg,
                        "remediation": rem,
                    })


check_dual_built_host_tools(candidate_dirs)

print(json.dumps(findings, indent=2))
PYEOF
