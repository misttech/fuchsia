#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing BUILD.bazel minimality, Starlark buildifier
# formatting/lint hygiene, and strict target-directory scope:
# 1. Flags redundant default attributes in BUILD.bazel:
#    - crate_name = "<x>" when identical to target name (modulo '-' vs '_')
#    - version = "0.1.0" on first-party (in-tree) rustc_* targets
#    - crate_root = "src/lib.rs" on rustc_library
#    - crate_root = "src/main.rs" on rustc_binary
#    - output_name = "<x>" or out = "<x>" when identical to target name (and
#      unsupported output_name on go_binary / go_binary_host_tool)
#    - missing //build/bazel/versioning:is_api_level_PLATFORM visibility in
#      build/bazel/versioning/BUILD.bazel when calling legacy host-tool macros
#      (go_binary_host_tool, py_binary_host_tool)
#    - unwrapped external C/C++ or Rust rules (cc_library, cc_binary from
#      @rules_cc//, or rust_library, rust_binary, rust_proc_macro, rust_test,
#      rust_shared_library, rust_dylib_library from @rules_rust//) when
#      Fuchsia-specific wrapper macros (fx_cc_library, fx_cc_binary, rustc_*)
#      exist under //build/bazel/rules/
#    - private or duplicate external rule load() statements (@...//.../private/...)
#    and genrule commands added by the change that derive paths or arguments with
#    shell command substitution ($$(dirname ...), $$(python3 -c ...), backticks) or
#    hard-code bazel-out/ paths instead of using Bazel's predefined genrule variables.
# 2. Runs `buildifier -lint=warn -format=json -mode=check` (with an AST fallback
#    for `.bzl` module docstrings) on all changed and target-package Starlark
#    files (`BUILD.bazel`, `BUILD`, `*.bzl`, `*.bazel`, `MODULE.bazel`),
#    catching missing `.bzl` module docstrings, unused `load` symbols, and
#    unformatted Starlark files before `shac` runs in CQ.
# 3. Flags drive-by edits to unrelated packages and shared Bazel rule/macro
#    definitions (`build/bazel/rules/**`, `build/bazel/aspects/**`, etc.) outside
#    PLANTER_TARGET_DIR / PLANTER_TARGET_DIRS (except centralized registration
#    lists, ancestor BUILD.gn/BUILD.bazel test groups/suites, and dependency
#    packages the change migrates: a new BUILD.bazel referenced, transitively,
#    from a target package's BUILD.bazel, and the in-tree build of
#    third_party/rust_crates: its BUILD files, Cargo.toml/Cargo.lock and compat/
#    shims).
#    TODO: Evaluate each out-of-scope edit for validity/usefulness and apply or
#    batch valid changes in a separate commit.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
import ast
import json
import os
import re
import shutil
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
change_base = sys.argv[3].strip() or "HEAD"
target_dirs = [d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")]

ALLOWED_GLOBAL_PREFIXES = (
    "build/bazel2gn",
    "build/bazel/",
    "build/config/rust/lints/",
    "bundles/assembly/",
    "build/images/",
)

SHARED_BAZEL_RULE_PREFIXES = (
    "build/bazel/rules/",
    "build/bazel/aspects/",
    "build/bazel/toolchains/",
    "build/bazel/starlark/",
    "build/bazel/scripts/",
)


def is_shared_bazel_rule_file(norm: str) -> bool:
    if not target_dirs:
        return False
    if any(norm == td or norm.startswith(td + "/") for td in target_dirs):
        return False
    if norm.startswith(SHARED_BAZEL_RULE_PREFIXES):
        return True
    if norm.startswith("build/bazel/") and norm.endswith(".bzl"):
        return not norm.startswith("build/bazel/update-rustc-third-party/")
    return False


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


changed = set()
changed.update(git_lines(["diff", "--name-only", change_base]))
untracked = git_lines(["ls-files", "--others", "--exclude-standard"])
changed.update(untracked)


def migrated_dependency_packages():
    """Packages this change migrates because a target (transitively) depends on them.

    A dependency package is in scope when its BUILD.bazel is added or modified in this change
    and is reachable from a target package's BUILD.bazel through non-visibility label
    references (including across already-migrated intermediate BUILD.bazel files).
    """
    added = set(git_lines(["diff", "--name-only", "--diff-filter=AM", change_base])) | set(untracked)
    candidates = {
        os.path.dirname(p)
        for p in added
        if os.path.basename(p) == "BUILD.bazel"
        and not p.strip("/").startswith(ALLOWED_GLOBAL_PREFIXES)
    }
    candidates -= set(target_dirs)
    if not candidates:
        return set()
    label_re = re.compile(r'"@?//([^":]+)(?::([^"\s]+))?"')
    found, seen, frontier = set(), set(), list(target_dirs)
    while frontier and found != candidates:
        pkg = frontier.pop()
        if pkg in seen or not pkg:
            continue
        seen.add(pkg)
        if pkg not in candidates and pkg.startswith(("third_party/rust_crates/vendor", "build/bazel/")):
            continue
        try:
            with open(os.path.join(workdir, pkg, "BUILD.bazel"), encoding="utf-8") as f:
                text = f.read()
        except OSError:
            continue
        for m in label_re.finditer(text):
            ref, target = m.group(1).strip("/"), m.group(2) or ""
            if not ref or target in ("__pkg__", "__subpackages__"):
                continue
            if ref in candidates:
                found.add(ref)
            if ref not in seen:
                frontier.append(ref)
    return found


dependency_dirs = migrated_dependency_packages()


def referenced_shared_configs():
    """Returns (allowed_gni_files, allowed_lint_pkgs) referenced by in-scope packages."""
    gni_files, lint_pkgs = set(), set()
    for td in list(target_dirs) + sorted(dependency_dirs):
        try:
            with open(os.path.join(workdir, td, "BUILD.gn"), encoding="utf-8") as f:
                gn_txt = f.read()
            for m in re.finditer(r'\bimport\(\s*"//([^"]+\.gni)"\s*\)', gn_txt):
                gni_files.add(m.group(1).strip("/"))
        except OSError:
            pass
        try:
            with open(os.path.join(workdir, td, "BUILD.bazel"), encoding="utf-8") as f:
                bz_txt = f.read()
            for m in re.finditer(r'\blint_config\s*=\s*"//([^":]+)', bz_txt):
                lint_pkgs.add(m.group(1).strip("/"))
        except OSError:
            pass
    return gni_files, lint_pkgs


allowed_gni_files, allowed_lint_pkgs = referenced_shared_configs()


def references_in_scope_package_at_base(norm: str) -> bool:
    """True when `norm` (or its package's BUILD.gn at `change_base`) references an in-scope migrated package."""
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    if not in_scope:
        return False
    pkg_dir = os.path.dirname(norm).strip("/")
    if pkg_dir.endswith("/meta"):
        pkg_dir = os.path.dirname(pkg_dir).strip("/")
    candidates = [norm]
    if pkg_dir:
        for alt in (os.path.join(pkg_dir, "BUILD.gn"), os.path.join(pkg_dir, "BUILD.bazel")):
            if alt not in candidates:
                candidates.append(alt)
    texts = []
    for cand in candidates:
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{cand}"]))
        if base_txt:
            texts.append(base_txt)
        try:
            with open(os.path.join(workdir, cand), encoding="utf-8") as f:
                texts.append(f.read())
        except OSError:
            pass
    if not texts:
        return False
    combined = "\n".join(texts)
    for td in in_scope:
        pat = r'(?:@gn_targets//(?:[^\s"\':]+/)?|//)' + re.escape(td) + r'(?=[:"/\s\'])'
        if re.search(pat, combined):
            return True
    return False


def is_allowed_scope(path: str) -> bool:
    if not target_dirs:
        return True
    norm = path.strip("/")
    pkg_dir = os.path.dirname(norm)
    for td in list(target_dirs) + sorted(dependency_dirs):
        if norm == td or norm.startswith(td + "/"):
            return True
        if pkg_dir and td.startswith(pkg_dir + "/") and os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel"):
            return True
    if is_shared_bazel_rule_file(norm):
        return references_in_scope_package_at_base(norm)
    if (os.path.basename(norm) in ("BUILD.bazel", "BUILD", "BUILD.gn") or norm.endswith((".bzl", ".gni", ".cml"))) and references_in_scope_package_at_base(norm):
        return True
    if norm in allowed_gni_files:
        return True
    if pkg_dir in allowed_lint_pkgs and os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel"):
        return True
    for prefix in ALLOWED_GLOBAL_PREFIXES:
        if norm.startswith(prefix):
            return True
    if norm in (
        "tools/BUILD.gn",
        "src/BUILD.gn",
        "sdk/BUILD.gn",
        "sdk/atom_lists.bzl",
        "sdk/fidl/BUILD.gn",
        "sdk/fidl/category_lists.bzl",
    ):
        return True
    # Build files under bundles/ wire migrated targets and tests into CQ (test groups,
    # tests_barrier / bazel_target_test_suite_barrier of builder groups), which migrations
    # may need so exported bazel_test_suite tests reach tests.json only with their bundle.
    if norm.startswith("bundles/") and (
        os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel") or norm.endswith(".gni")
    ):
        return True
    return is_rust_crates_build_file(norm)


RUST_CRATES = "third_party/rust_crates/"


def is_rust_crates_build_file(norm: str) -> bool:
    """The in-tree build of the vendored Rust crates, which migrations may fix.

    Their build files (generated from crate annotations under build/bazel/, already
    allowed), Cargo.toml/Cargo.lock, and compat/ shims are maintained in-tree. Vendored
    crate sources are not.
    """
    if not norm.startswith(RUST_CRATES):
        return False
    rest = norm[len(RUST_CRATES):]
    if rest in ("Cargo.toml", "Cargo.lock") or rest.startswith("compat/"):
        return True
    return os.path.basename(rest) in ("BUILD.gn", "BUILD.bazel")


findings = []

for path in sorted(changed):
    if not is_allowed_scope(path):
        if is_shared_bazel_rule_file(path.strip("/")):
            findings.append({
                "source": "bazel_minimality",
                "category": "out_of_scope_package_modified",
                "severity": "error",
                "file": path,
                "line": 1,
                "message": (
                    f"File '{path}' is a shared Bazel rule or macro definition outside the assigned target "
                    f"directory scope ({', '.join(target_dirs)}). Package migrations must never modify "
                    "shared rules or macros under '//build/bazel/' (such as loading private rule definitions, "
                    "wrapping macros, or adding custom attributes)."
                ),
                "remediation": (
                    f"Revert '{path}' (`git checkout {change_base} -- {path}`). If a migrated target calls a "
                    "legacy host-tool macro (`go_binary_host_tool`, `py_binary_host_tool`) and fails visibility "
                    "on `//build/bazel/versioning:is_api_level_PLATFORM`, add `\"//<dir>:__pkg__\"` to "
                    "`is_api_level_PLATFORM`'s `visibility` list in 'build/bazel/versioning/BUILD.bazel' instead "
                    f"of editing '{path}'. If a target passes a redundant or unsupported attribute (such as "
                    "`output_name` matching `name` on `go_binary_host_tool`), omit or fix the attribute in the "
                    "target's BUILD.bazel."
                ),
            })
        else:
            findings.append({
                "source": "bazel_minimality",
                "category": "out_of_scope_package_modified",
                "severity": "error",
                "file": path,
                "line": 1,
                "message": (
                    f"File '{path}' is outside the assigned target directory scope "
                    f"({', '.join(target_dirs)}). Drive-by edits or cleanups in unrelated packages are forbidden."
                ),
                "remediation": (
                    f"Revert changes to '{path}' (`git checkout {change_base} -- {path}` or `git checkout -- {path}`) "
                    "and restrict modifications strictly to the assigned target package(s)."
                ),
            })

def load_platform_api_level_visibility():
    ver_path = os.path.join(workdir, "build/bazel/versioning/BUILD.bazel")
    if not os.path.isfile(ver_path):
        return None
    try:
        with open(ver_path, "r", encoding="utf-8") as f:
            ver_tree = ast.parse(f.read(), filename="build/bazel/versioning/BUILD.bazel")
    except Exception:
        return None
    var_lists = {}
    for stmt in ver_tree.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            strs = [
                n.value
                for n in ast.walk(stmt.value)
                if isinstance(n, ast.Constant) and isinstance(n.value, str)
            ]
            var_lists[stmt.targets[0].id] = strs
    for node in ast.walk(ver_tree):
        if not isinstance(node, ast.Call):
            continue
        if not (isinstance(node.func, ast.Name) and node.func.id == "config_setting"):
            continue
        cname = ""
        vis_node = None
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                cname = kw.value.value
            elif kw.arg == "visibility":
                vis_node = kw.value
        if cname == "is_api_level_PLATFORM" and vis_node is not None:
            entries = []
            for sub in ast.walk(vis_node):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    entries.append(sub.value)
                elif isinstance(sub, ast.Name) and sub.id in var_lists:
                    entries.extend(var_lists[sub.id])
            return entries
    return None


platform_api_vis = load_platform_api_level_visibility()


def is_covered_by_platform_api_vis(pkg_dir: str) -> bool:
    if platform_api_vis is None:
        return True
    for entry in platform_api_vis:
        if entry == "//visibility:public" or entry == "//:__subpackages__":
            return True
        if entry.startswith("//") and entry.endswith(":__pkg__"):
            p = entry[2 : -len(":__pkg__")].strip("/")
            if pkg_dir == p:
                return True
        elif entry.startswith("//") and entry.endswith(":__subpackages__"):
            p = entry[2 : -len(":__subpackages__")].strip("/")
            if not p or pkg_dir == p or pkg_dir.startswith(p + "/"):
                return True
    return False


bazel_files = []
for td in dict.fromkeys(list(target_dirs) + sorted(dependency_dirs)):
    cand = os.path.join(workdir, td, "BUILD.bazel")
    if os.path.isfile(cand):
        bazel_files.append((os.path.join(td, "BUILD.bazel"), cand))

for rel_path, full_path in bazel_files:
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            source = f.read()
        tree = ast.parse(source, filename=rel_path)
    except Exception:
        continue
    # Whitespace-free copy of the file at the change base: code found in it verbatim predates the change.
    base_bazel_flat = re.sub(r"\s+", "", "".join(git_lines(["show", f"{change_base}:{rel_path}"])))
    pkg_dir = os.path.dirname(rel_path)
    base_gn_lines = git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"])
    base_gn_text = "\n".join(base_gn_lines)
    src_lines = source.splitlines()

    # Collect in-package rustc_library targets so unit-test rustc_test targets can be
    # checked for `crate = ":<lib>"` reuse instead of repeating library sources/deps.
    pkg_rustc_libs = set()
    for stmt_node in ast.walk(tree):
        if isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name):
            if stmt_node.func.id in ("rustc_library", "rustc_proc_macro"):
                for kw in stmt_node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        pkg_rustc_libs.add(kw.value.value)

    # Extract non-directive comment lines (with 1-based line numbers) preceding a target call.
    def preceding_comment_lines(lineno: int):
        comments = []
        idx = lineno - 2
        while idx >= 0:
            s_line = src_lines[idx].strip()
            if not s_line:
                idx -= 1
                continue
            if s_line.startswith("#"):
                if "@bazel2gn:" not in s_line:
                    comments.append((idx + 1, s_line.lower()))
                idx -= 1
                continue
            break
        return list(reversed(comments))

    # Check that unit-test rustc_test targets use `crate = ":<lib>"` and omit `crate_name`,
    # except when GN `non_test_deps` swaps a dependency for a `testutils` variant (in which
    # case a standalone `rustc_test` with a concise justifying comment above it is required).
    def check_rustc_test_crate_reuse(call_node, r_name, t_name, k_map):
        """Verify unit-test rustc_test targets use `crate = ":<lib>"` or concisely document standalone testutils deps."""
        if r_name != "rustc_test" or not t_name:
            return
        kw_names = {kw.arg for kw in call_node.keywords if kw.arg}
        lib_stem = t_name[:-len("_test")] if t_name.endswith("_test") else ""
        has_matching_lib = bool(lib_stem and lib_stem in pkg_rustc_libs)
        if "srcs" in kw_names and has_matching_lib and "crate" not in kw_names:
            srcs_line = next(
                (kw.lineno for kw in call_node.keywords if kw.arg == "srcs"),
                getattr(call_node, "lineno", 1),
            )
            deps_kw = next((kw.value for kw in call_node.keywords if kw.arg == "deps"), None)
            deps_seg = (ast.get_source_segment(source, deps_kw) or "").lower() if deps_kw else ""
            needs_standalone = ("non_test_deps" in base_gn_text) or ("testutils" in deps_seg)
            comment_entries = preceding_comment_lines(getattr(call_node, "lineno", 1))
            comment_txt = "\n".join(txt for _, txt in comment_entries)
            has_justification = any(w in comment_txt for w in ("crate", "testutils"))
            verbose_line = next(
                (
                    ln
                    for ln, txt in reversed(comment_entries)
                    if "non_test_deps" in txt or "inherits" in txt
                ),
                None,
            )
            if needs_standalone:
                if not has_justification:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "rustc_test_missing_standalone_comment",
                        "severity": "error",
                        "file": rel_path,
                        "line": srcs_line,
                        "message": (
                            f"Target '{t_name}' (rustc_test) is not attached to `:{lib_stem}` via "
                            "`crate` (because unit tests require a `testutils` dependency variant), "
                            "but lacks a comment above `rustc_test` justifying why `crate` cannot "
                            "be used."
                        ),
                        "remediation": (
                            f"Add a concise comment immediately above `{t_name}` in '{rel_path}' "
                            f"explaining why it cannot attach to `:{lib_stem}` via `crate` "
                            "(because unit tests require the `testutils` dependency variant), "
                            "without adding boilerplate about `rustc_test(crate = ...)` inheriting "
                            "`deps` or GN `non_test_deps`."
                        ),
                    })
                elif verbose_line is not None:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "rustc_test_verbose_standalone_comment",
                        "severity": "error",
                        "file": rel_path,
                        "line": verbose_line,
                        "message": (
                            f"Comment above standalone '{t_name}' (rustc_test) includes "
                            "unnecessary rule-mechanics or GN-comparison boilerplate "
                            "(`rustc_test(crate = ...)` inheriting `deps` / GN `non_test_deps`)."
                        ),
                        "remediation": (
                            f"Trim the comment above `{t_name}` in '{rel_path}' so it states only "
                            f"the target-specific reason `crate = \":{lib_stem}\"` cannot be used "
                            "(that unit tests require the `testutils` dependency variant), and "
                            "remove the clause about Bazel `rustc_test(crate = ...)` inheriting "
                            "`deps` or GN `non_test_deps`."
                        ),
                    })
            elif not has_justification:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "rustc_test_repeats_library_srcs",
                    "severity": "error",
                    "file": rel_path,
                    "line": srcs_line,
                    "message": (
                        f"Target '{t_name}' (rustc_test) repeats `srcs` (and library features/deps) "
                        f"instead of referencing sibling library `:{lib_stem}` via `crate`."
                    ),
                    "remediation": (
                        f"Set `crate = \":{lib_stem}\"` on '{t_name}' and remove `srcs`, "
                        "`crate_name`, and any `deps`/`proc_macro_deps`/`crate_features` already "
                        f"inherited from `:{lib_stem}` (or, if unit tests require a `testutils` "
                        "dependency variant, leave a concise comment above `rustc_test` justifying "
                        "why it cannot attach to `crate`)."
                    ),
                })
        if "crate_name" in k_map and ("crate" in kw_names or has_matching_lib):
            c_name, c_line = k_map["crate_name"]
            findings.append({
                "source": "bazel_minimality",
                "category": "redundant_default_attribute",
                "severity": "error",
                "file": rel_path,
                "line": c_line,
                "message": (
                    f"Target '{t_name}' (rustc_test) specifies `crate_name = \"{c_name}\"`, "
                    "which is unnecessary on a crate unit test."
                ),
                "remediation": (
                    f"Remove `crate_name` from `{t_name}` in '{rel_path}'."
                ),
            })

    # Check that newly added top-level _-prefixed variables are shared across >= 2 references.
    def check_single_use_private_vars(tree_node):
        """Flag newly added top-level _-prefixed variables referenced fewer than 2 times."""
        load_counts = {}
        for n in ast.walk(tree_node):
            if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load):
                load_counts[n.id] = load_counts.get(n.id, 0) + 1
        for stmt in tree_node.body:
            if not (
                isinstance(stmt, ast.Assign)
                and len(stmt.targets) == 1
                and isinstance(stmt.targets[0], ast.Name)
            ):
                continue
            v_name = stmt.targets[0].id
            if not v_name.startswith("_"):
                continue
            seg = ast.get_source_segment(source, stmt) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            uses = load_counts.get(v_name, 0)
            if uses < 2:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "single_use_private_variable",
                    "severity": "error",
                    "file": rel_path,
                    "line": stmt.lineno,
                    "message": (
                        f"Top-level private variable `{v_name}` in '{rel_path}' is referenced "
                        f"{uses} time(s); shared `_` variables should only be introduced when "
                        "reused across multiple targets."
                    ),
                    "remediation": (
                        f"Inline `{v_name}` directly into its target in '{rel_path}' (or remove "
                        f"it if unused) and re-run `fx bazel2gn -d {pkg_dir}` if dual-building."
                    ),
                })

    # Check wrap_host_rust_test naming in rustc_library_with_features packages with a single host test.
    def check_wrap_host_rust_test_naming(tree_node, gn_text):
        """Verify wrap_host_rust_test is named <base>-host-test for rustc_library_with_features."""
        feat_m = re.search(r'\brustc_library_with_features\(\s*"([^"]+)"\s*\)', gn_text)
        if not feat_m:
            return
        base_lib = feat_m.group(1)
        if "-" not in base_lib:
            return
        host_wrappers = []
        for stmt_node in ast.walk(tree_node):
            if (
                isinstance(stmt_node, ast.Call)
                and isinstance(stmt_node.func, ast.Name)
                and stmt_node.func.id == "wrap_host_rust_test"
            ):
                w_name, w_bin, w_line = "", "", getattr(stmt_node, "lineno", 1)
                for kw in stmt_node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        w_name = kw.value.value
                        w_line = kw.lineno
                    elif kw.arg == "binary_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        w_bin = kw.value.value
                if w_name:
                    host_wrappers.append((w_name, w_bin, w_line, stmt_node))
        if len(host_wrappers) != 1:
            return
        w_name, w_bin, w_line, w_node = host_wrappers[0]
        seg = ast.get_source_segment(source, w_node) or ""
        if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
            return
        expected_name = f"{base_lib}-host-test"
        if w_bin in (f"{base_lib}-instrumented_test", f"{base_lib}_test") and w_name != expected_name:
            findings.append({
                "source": "bazel_minimality",
                "category": "wrap_host_rust_test_naming",
                "severity": "error",
                "file": rel_path,
                "line": w_line,
                "message": (
                    f"Target '{w_name}' (wrap_host_rust_test) wraps `{w_bin}` for "
                    f"`rustc_library_with_features(\"{base_lib}\")`, which should be named "
                    f"`\"{expected_name}\"` instead of including the variant suffix or `_host_test`."
                ),
                "remediation": (
                    f"Rename `{w_name}` to `\"{expected_name}\"` in '{rel_path}' and update its "
                    f"`host_tests` reference in `{pkg_dir}/BUILD.gn`."
                ),
            })

    # Check for rustc_check_cfg_build_flags(cfgs = ["test"]) in packages with no tests.
    def check_unnecessary_test_check_cfg_flags(tree_node):
        """Flag rustc_check_cfg_build_flags(cfgs = ["test"]) when the package has no tests."""
        has_pkg_tests = False
        for stmt_node in ast.walk(tree_node):
            if not (isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name)):
                continue
            r_id = stmt_node.func.id
            if r_id in ("rustc_test", "rust_test", "fx_unittest_package", "fx_test"):
                has_pkg_tests = True
                break
            if r_id.startswith(("rustc_", "rust_")):
                for kw in stmt_node.keywords:
                    if kw.arg in ("with_unit_tests", "with_host_unit_tests") and isinstance(kw.value, ast.Constant):
                        if kw.value.value is True or kw.value.value in ("host", "fuchsia", "both"):
                            has_pkg_tests = True
                            break
        if has_pkg_tests:
            return
        pkg_abs = os.path.join(workdir, pkg_dir)
        for root, _, files in os.walk(pkg_abs):
            for fn in files:
                if fn.endswith(".rs"):
                    try:
                        rs_txt = open(os.path.join(root, fn), encoding="utf-8").read()
                        if re.search(r"#\s*\[\s*(?:cfg|cfg_attr)\s*\([^]]*\btest\b", rs_txt):
                            return
                    except OSError:
                        pass
        for stmt_node in ast.walk(tree_node):
            if not (
                isinstance(stmt_node, ast.Call)
                and isinstance(stmt_node.func, ast.Name)
                and stmt_node.func.id == "rustc_check_cfg_build_flags"
            ):
                continue
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            cfg_name = ""
            cfgs_list, feats_list = [], []
            for kw in stmt_node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    cfg_name = kw.value.value
                elif kw.arg == "cfgs" and isinstance(kw.value, ast.List):
                    cfgs_list = [
                        e.value for e in kw.value.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)
                    ]
                elif kw.arg == "features" and isinstance(kw.value, ast.List):
                    feats_list = [
                        e.value for e in kw.value.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)
                    ]
            if cfgs_list == ["test"] and not feats_list:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "unnecessary_test_check_cfg_flags",
                    "severity": "error",
                    "file": rel_path,
                    "line": getattr(stmt_node, "lineno", 1),
                    "message": (
                        f"`rustc_check_cfg_build_flags(name = \"{cfg_name}\", cfgs = [\"test\"])` in "
                        f"'{rel_path}' checks only `cfg(test)`, but this package has no unit tests or "
                        "`#[cfg(test)]` items."
                    ),
                    "remediation": (
                        f"Remove `rustc_check_cfg_build_flags(name = \"{cfg_name}\", ...)`, its "
                        f"`\":{cfg_name}\"` reference in `build_flags`, and any unused `load(...)` from "
                        f"'{rel_path}', then re-run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })

    # Check that select({...}) blocks in deps repeated across >= 2 targets are extracted into _COMMON_DEPS.
    def check_repeated_deps_select(tree_node):
        """Flag select({...}) expressions in target deps repeated across multiple targets."""
        select_occurrences = {}
        for stmt_node in ast.walk(tree_node):
            if not (isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name)):
                continue
            for kw in stmt_node.keywords:
                if kw.arg != "deps":
                    continue
                for sub in ast.walk(kw.value):
                    if (
                        isinstance(sub, ast.Call)
                        and isinstance(sub.func, ast.Name)
                        and sub.func.id == "select"
                    ):
                        seg = ast.get_source_segment(source, sub) or ""
                        norm_seg = re.sub(r"\s+", "", seg)
                        if norm_seg and norm_seg not in base_bazel_flat:
                            select_occurrences.setdefault(norm_seg, []).append(getattr(sub, "lineno", kw.lineno))
        for _norm_seg, lines_list in select_occurrences.items():
            if len(lines_list) >= 2:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "repeated_common_deps_select",
                    "severity": "error",
                    "file": rel_path,
                    "line": lines_list[0],
                    "message": (
                        f"Identical `select({{...}})` block in `deps` is repeated across {len(lines_list)} "
                        f"targets in '{rel_path}' instead of being extracted into `_COMMON_DEPS`."
                    ),
                    "remediation": (
                        f"Extract the shared `select({{...}})` dependency block into top-level "
                        "`_COMMON_DEPS` (annotated with `# @bazel2gn:transformer=deps` immediately above "
                        "`_COMMON_DEPS = ...` when dual-building with `bazel2gn`), reference `_COMMON_DEPS` "
                        f"in each target's `deps`, and re-run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })

    check_single_use_private_vars(tree)
    check_wrap_host_rust_test_naming(tree, base_gn_text)
    check_unnecessary_test_check_cfg_flags(tree)
    check_repeated_deps_select(tree)

    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = ""
        if isinstance(node.func, ast.Name):
            rule_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule_name = node.func.attr
        if not rule_name:
            continue

        kw_map = {}
        for kw in node.keywords:
            if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                kw_map[kw.arg] = (kw.value.value, kw.lineno)

        target_name, _ = kw_map.get("name", ("", 0))

        # 0. Enforce Fuchsia-specific wrapper macros (fx_cc_*, rustc_*) instead of raw
        #    @rules_cc// or @rules_rust// rules per the Fuchsia Bazel style guide.
        UNWRAPPED_FUCHSIA_RULES = {
            "cc_library": (
                "fx_cc_library",
                "//build/bazel/rules/cc:fx_cc_library.bzl",
                "build/bazel/rules/cc/fx_cc_library.bzl",
            ),
            "cc_binary": (
                "fx_cc_binary",
                "//build/bazel/rules/cc:fx_cc_binary.bzl",
                "build/bazel/rules/cc/fx_cc_binary.bzl",
            ),
            "rust_library": (
                "rustc_library",
                "//build/bazel/rules/rust:rustc_library.bzl",
                "build/bazel/rules/rust/rustc_library.bzl",
            ),
            "rust_binary": (
                "rustc_binary",
                "//build/bazel/rules/rust:rustc_binary.bzl",
                "build/bazel/rules/rust/rustc_binary.bzl",
            ),
            "rust_proc_macro": (
                "rustc_proc_macro",
                "//build/bazel/rules/rust:rustc_proc_macro.bzl",
                "build/bazel/rules/rust/rustc_proc_macro.bzl",
            ),
            "rust_test": (
                "rustc_test",
                "//build/bazel/rules/rust:rustc_test.bzl",
                "build/bazel/rules/rust/rustc_test.bzl",
            ),
            "rust_shared_library": (
                "rustc_dylib",
                "//build/bazel/rules/rust:rustc_dylib.bzl",
                "build/bazel/rules/rust/rustc_dylib.bzl",
            ),
            "rust_dylib_library": (
                "rustc_dylib",
                "//build/bazel/rules/rust:rustc_dylib.bzl",
                "build/bazel/rules/rust/rustc_dylib.bzl",
            ),
        }
        if (
            rule_name in UNWRAPPED_FUCHSIA_RULES
            and not rel_path.startswith("build/bazel/")
        ):
            wrapper_rule, wrapper_bzl, wrapper_path = UNWRAPPED_FUCHSIA_RULES[rule_name]
            if os.path.isfile(os.path.join(workdir, wrapper_path)):
                pkg_d = os.path.dirname(rel_path).strip("/")
                findings.append({
                    "source": "bazel_minimality",
                    "category": "unwrapped_external_language_rule",
                    "severity": "error",
                    "file": rel_path,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target '{target_name}' in '{rel_path}' uses unwrapped `{rule_name}()` "
                        f"instead of the Fuchsia-specific wrapper `{wrapper_rule}()` from "
                        f"`{wrapper_bzl}` (see "
                        "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#use_fuchsia-specific_wrappers)."
                    ),
                    "remediation": (
                        f"Replace `load(..., \"{rule_name}\")` and `{rule_name}(...)` in "
                        f"'{rel_path}' with `load(\"{wrapper_bzl}\", \"{wrapper_rule}\")` and "
                        f"`{wrapper_rule}(...)`, then re-run `fx bazel2gn -d {pkg_d}` if dual-building."
                    ),
                })

        # 1. Redundant crate_name matching name (or name with '-' -> '_').
        # On rustc_binary, bazel2gn maps crate_name to GN output_name, so crate_name is intentional when output_name is needed.
        check_rustc_test_crate_reuse(node, rule_name, target_name, kw_map)
        if "crate_name" in kw_map and target_name and rule_name not in ("rustc_binary", "rustc_test"):
            c_name, lineno = kw_map["crate_name"]
            if c_name == target_name or c_name == target_name.replace("-", "_"):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' ({rule_name}) specifies redundant `crate_name = \"{c_name}\"`, "
                        "which is already the default derived from `name`."
                    ),
                    "remediation": (
                        "Remove `crate_name` from BUILD.bazel and re-run `fx bazel2gn` so the target definition stays minimal."
                    ),
                })

        # 2. Redundant version = "0.1.0" (or placeholder negative version like "-1.1.0") on rustc_* targets
        if rule_name.startswith("rustc_") and "version" in kw_map:
            ver, lineno = kw_map["version"]
            if ver in ("0.1.0", "0.0.1") or ver.startswith("-"):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' ({rule_name}) specifies redundant or placeholder `version = \"{ver}\"`, "
                        "which defaults to \"0.1.0\" in Fuchsia's Rust Bazel rules and is not emitted by bazel2gn."
                    ),
                    "remediation": (
                        "Remove `version` from BUILD.bazel (and from BUILD.gn if dual-building) and re-run `fx bazel2gn`."
                    ),
                })

        # 3. Redundant default crate_root on rustc_library / rustc_binary
        if "crate_root" in kw_map:
            c_root, lineno = kw_map["crate_root"]
            if (rule_name == "rustc_library" and c_root == "src/lib.rs") or (
                rule_name == "rustc_binary" and c_root == "src/main.rs"
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' ({rule_name}) specifies redundant `crate_root = \"{c_root}\"`, "
                        f"which is the default for `{rule_name}`."
                    ),
                    "remediation": (
                        "Remove `crate_root` from BUILD.bazel and re-run `fx bazel2gn`."
                    ),
                })

        # 3b. Redundant output_name / out matching name (or unsupported output_name on go_binary / go_binary_host_tool)
        for out_attr in ("output_name", "out"):
            if out_attr not in kw_map:
                continue
            out_val, lineno = kw_map[out_attr]
            if target_name and out_val == target_name:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' ({rule_name}) specifies redundant `{out_attr} = \"{out_val}\"`, "
                        "which is already the default derived from `name` (and `output_name` is not a valid attribute on "
                        "`go_binary` / `go_binary_host_tool`)."
                    ),
                    "remediation": (
                        f"Remove `{out_attr}` from BUILD.bazel (and from BUILD.gn if dual-building) and re-run `fx bazel2gn`."
                    ),
                })
            elif out_attr == "output_name" and rule_name in ("go_binary", "go_binary_host_tool"):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Target '{target_name}' ({rule_name}) specifies `output_name = \"{out_val}\"`, "
                        f"which is not a valid attribute on `{rule_name}` (`go_binary` uses `out` when a custom binary name is needed)."
                    ),
                    "remediation": (
                        f"Omit `output_name` when it matches `name`, or use `out = \"{out_val}\"` if a custom binary name is required, "
                        "without modifying shared macros in `//build/bazel/rules/host:defs.bzl`."
                    ),
                })

        # 3c. Legacy host-tool macros (go_binary_host_tool, py_binary_host_tool) require
        #     caller package visibility on //build/bazel/versioning:is_api_level_PLATFORM.
        if rule_name in ("go_binary_host_tool", "py_binary_host_tool"):
            pkg_d = os.path.dirname(rel_path).strip("/")
            if pkg_d and not is_covered_by_platform_api_vis(pkg_d):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "missing_platform_api_level_visibility",
                    "severity": "error",
                    "file": rel_path,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target '{target_name}' ({rule_name}) invokes legacy macro `{rule_name}`, which "
                        "expands `//build/bazel/versioning:is_api_level_PLATFORM` in the calling package, "
                        f"but 'build/bazel/versioning/BUILD.bazel' does not grant visibility to '//{pkg_d}'."
                    ),
                    "remediation": (
                        f"Add `\"//{pkg_d}:__pkg__\"` (in alphabetical order) to the `visibility` list of "
                        "`is_api_level_PLATFORM` in 'build/bazel/versioning/BUILD.bazel'. Never modify "
                        "'build/bazel/rules/host/defs.bzl' to work around this visibility requirement."
                    ),
                })

        # 4. Platform/CPU-specific target using srcs = select({cond: [...], "//conditions:default": []})
        #    instead of target_compatible_with = [cond].
        for kw in node.keywords:
            if kw.arg != "srcs":
                continue
            val = kw.value
            if (
                isinstance(val, ast.Call)
                and isinstance(val.func, ast.Name)
                and val.func.id == "select"
                and val.args
                and isinstance(val.args[0], ast.Dict)
            ):
                d = val.args[0]
                non_default_keys = []
                has_empty_default = False
                for k_node, v_node in zip(d.keys, d.values):
                    if isinstance(k_node, ast.Constant) and isinstance(k_node.value, str):
                        if k_node.value == "//conditions:default":
                            if isinstance(v_node, ast.List) and len(v_node.elts) == 0:
                                has_empty_default = True
                        else:
                            non_default_keys.append(k_node.value)
                if has_empty_default and len(non_default_keys) == 1:
                    cond = non_default_keys[0]
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "select_srcs_instead_of_target_compatible_with",
                        "severity": "error",
                        "file": rel_path,
                        "line": kw.lineno,
                        "message": (
                            f"Target '{target_name}' ({rule_name}) gates all `srcs` behind "
                            f"`select({{\"{cond}\": [...], \"//conditions:default\": []}})` "
                            "instead of restricting the target with `target_compatible_with`."
                        ),
                        "remediation": (
                            f"Set `srcs` directly to the source list on '{target_name}', replace the "
                            f"`select()` + `# @bazel2gn:raw_overwrite` workaround with "
                            f"`target_compatible_with = [\"{cond}\"]`, and re-run `fx bazel2gn`."
                        ),
                    })

        # 6. genrule commands added or changed by the migration build paths and arguments from
        #    Bazel's predefined variables and Starlark values, not from shell command substitution
        #    at execution time (e.g. `$$(dirname $(location <one input>))`, `$$(python3 -c ...)`)
        #    or hard-coded output-tree paths.
        if rule_name == "genrule":
            for kw in node.keywords:
                if kw.arg not in ("cmd", "cmd_bash"):
                    continue
                segment = ast.get_source_segment(source, kw.value) or ""
                if segment and re.sub(r"\s+", "", segment) in base_bazel_flat:
                    continue
                cmd_text = " ".join(
                    n.value
                    for n in ast.walk(kw.value)
                    if isinstance(n, ast.Constant) and isinstance(n.value, str)
                )
                problems = [f"`$$({m.group(1)} ...)`" for m in re.finditer(r"\$\$\(\s*([^\s()]*)", cmd_text)]
                if "`" in cmd_text:
                    problems.append("a backtick command substitution")
                problems += [
                    f"a hard-coded `{m.group(1)}/` path"
                    for m in re.finditer(r"\b(bazel-(?:out|bin|genfiles)|execroot)/", cmd_text)
                ]
                problems = list(dict.fromkeys(problems))
                if not problems:
                    continue
                findings.append({
                    "source": "bazel_minimality",
                    "category": "genrule_cmd_shell_derived_paths",
                    "severity": "error",
                    "file": rel_path,
                    "line": kw.lineno,
                    "message": (
                        f"genrule '{target_name}' `{kw.arg}` derives paths or arguments in the shell at "
                        f"execution time or hard-codes output-tree paths: {', '.join(problems)}. Reviewers "
                        "reject this as non-canonical (e.g. the `dirname` of one arbitrary input to get a "
                        "directory), and bazel2gn only converts `$@`/`$<` and literal words to GN action args."
                    ),
                    "remediation": (
                        "Build the command from Bazel's predefined genrule variables: outputs `$@`/`$(OUTS)`, "
                        "the output directory `$(@D)`/`$(RULEDIR)` (never `dirname $@` or `bazel-out/...`), "
                        "inputs `$<`/`$(SRCS)`/`$(execpath <label>)`/`$(execpaths <label>)`. A directory of "
                        "checked-in inputs that GN passes as `rebase_path(\"<dir>\", root_build_dir)` is "
                        "`package_name() + \"/<dir>\"` (genrules run from the execution root, where the "
                        "package's sources are under its package path), with a comment saying it holds "
                        "checked-in inputs. A value GN computes at gen time (e.g. a `read_file(\"<f>\", "
                        "\"json\")` list) is written out in BUILD.bazel as a Starlark list wrapped in "
                        "`# LINT.IfChange` / `# LINT.ThenChange(<f>)` and joined into the command "
                        "(`\" \".join(<LIST>)`), not read by a `python3 -c`/`cat`/`jq` subshell. Re-run "
                        "`fx bazel2gn -d <dir>` and `fx bazel build` for the package."
                    ),
                })

    # 5. GN read_file("<manifest>", "json") + foreach() expanded into static BUILD.bazel lists
    #    without LINT.IfChange / LINT.ThenChange(<manifest>) when <manifest> remains in the tree.
    pkg_dir = os.path.dirname(rel_path)
    base_gn_lines = git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"])
    base_gn_text = "\n".join(base_gn_lines)
    if base_gn_text:
        for m in re.finditer(r'\bread_file\(\s*"([^"]+)"\s*,\s*"json"\s*\)', base_gn_text):
            manifest_rel_pkg = m.group(1)
            if manifest_rel_pkg.startswith("//"):
                manifest_repo_rel = manifest_rel_pkg[2:]
            else:
                manifest_repo_rel = os.path.normpath(os.path.join(pkg_dir, manifest_rel_pkg))
            manifest_abs = os.path.join(workdir, manifest_repo_rel)
            if not os.path.isfile(manifest_abs):
                continue
            has_then_change = bool(
                re.search(
                    r"#\s*LINT\.ThenChange\([^)]*" + re.escape(os.path.basename(manifest_rel_pkg)),
                    source,
                )
            )
            if not has_then_change:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "missing_lint_then_change_for_manifest_list",
                    "severity": "error",
                    "file": rel_path,
                    "line": 1,
                    "message": (
                        f"'{rel_path}' inline-expands a file list that `{pkg_dir}/BUILD.gn` previously "
                        f"loaded via `read_file(\"{manifest_rel_pkg}\", \"json\")`, while "
                        f"'{manifest_repo_rel}' still exists without `# LINT.IfChange` / "
                        f"`# LINT.ThenChange({manifest_rel_pkg})`."
                    ),
                    "remediation": (
                        f"Wrap the expanded file list in '{rel_path}' with `# LINT.IfChange` and "
                        f"`# LINT.ThenChange({manifest_rel_pkg})` (or remove '{manifest_repo_rel}' if "
                        "no script or tool uses it anymore)."
                    ),
                })

    # 7. @bazel2gn:path_overwrite hygiene: reject multi-label overwrites, reject real
    #    Bazel targets (including same-package fidl_library targets for missing
    #    _rust_next bindings) used as placeholder stand-ins for unmigrated GN-only deps,
    #    and require a separate # @bazel2gn:skip filegroup per overwritten path
    #    using a unique name derived from the overwritten path.
    src_lines = source.splitlines()
    cur_gn_text = ""
    try:
        with open(os.path.join(workdir, pkg_dir, "BUILD.gn"), encoding="utf-8") as gf:
            cur_gn_text = gf.read()
    except OSError:
        pass

    local_fgs = {}
    for stmt in tree.body:
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "filegroup"
        ):
            continue
        fg_name = ""
        has_srcs = False
        for kw in stmt.value.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                fg_name = kw.value.value
            elif kw.arg == "srcs":
                has_srcs = True
        if fg_name and not has_srcs:
            fg_skipped = False
            idx = stmt.lineno - 2
            while idx >= 0:
                s_line = src_lines[idx].strip()
                if not s_line:
                    idx -= 1
                    continue
                if s_line.startswith("#"):
                    if "@bazel2gn:skip" in s_line:
                        fg_skipped = True
                    idx -= 1
                    continue
                break
            if not fg_skipped and 0 < stmt.lineno <= len(src_lines) and "@bazel2gn:skip" in src_lines[stmt.lineno - 1]:
                fg_skipped = True
            local_fgs[fg_name] = (stmt.lineno, fg_skipped)

    for fg_name, (fg_line, fg_skipped) in local_fgs.items():
        if not fg_skipped:
            findings.append({
                "source": "bazel_minimality",
                "category": "path_overwrite_placeholder_hygiene",
                "severity": "error",
                "file": rel_path,
                "line": fg_line,
                "message": (
                    f"Placeholder target `filegroup(name = \"{fg_name}\")` in '{rel_path}' lacks `# @bazel2gn:skip`."
                ),
                "remediation": (
                    f"Add `# @bazel2gn:skip` directly above `filegroup(name = \"{fg_name}\")` in '{rel_path}' "
                    f"and re-run `fx bazel2gn -d {pkg_dir}`."
                ),
            })
        if fg_name.lower() in ("bazel2gn_placeholder", "placeholder"):
            findings.append({
                "source": "bazel_minimality",
                "category": "path_overwrite_placeholder_hygiene",
                "severity": "error",
                "file": rel_path,
                "line": fg_line,
                "message": (
                    f"Placeholder `filegroup(name = \"{fg_name}\")` in '{rel_path}' uses a generic name instead of "
                    "a unique name derived from the overwritten GN dependency path."
                ),
                "remediation": (
                    "Replace generic placeholder filegroups with one `# @bazel2gn:skip` `filegroup(name = \"<unique_name>\")` "
                    "per overwritten dependency, using a unique name derived from the overwritten path (e.g., "
                    "`board_structured_config`, `fuchsia.driver.framework_rust_next`, `sdk_lib_driver_runtime_rust_fidl`), "
                    f"and re-run `fx bazel2gn -d {pkg_dir}`."
                ),
            })

    def derive_fg_name(gn_label: str) -> str:
        s = gn_label.strip().lstrip("/")
        if s.startswith(":"):
            return s[1:]
        if ":" in s:
            _, t = s.split(":", 1)
            return t
        return s.replace("/", "_")

    for lineno, line_txt in enumerate(src_lines, 1):
        po_m = re.match(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', line_txt)
        if not po_m:
            continue
        bz_label = po_m.group(1).strip()
        gn_spec = po_m.group(2).strip()
        if (
            "$" in gn_spec
            or bz_label.startswith(("@", "//third_party/", "//:", "//src/zircon/lib/zircon"))
            or "//third_party/" in gn_spec
        ):
            continue
        gn_labels = [tok.strip().strip('"') for tok in gn_spec.split('", "') if tok.strip().strip('"')]
        if len(gn_labels) > 1:
            suggested = ", ".join(
                f'`":{derive_fg_name(gl)}",  # @bazel2gn:path_overwrite:{gl}`' for gl in gn_labels
            )
            findings.append({
                "source": "bazel_minimality",
                "category": "path_overwrite_placeholder_hygiene",
                "severity": "error",
                "file": rel_path,
                "line": lineno,
                "message": (
                    f"Line {lineno} of '{rel_path}' packs multiple GN labels into a single `# @bazel2gn:path_overwrite:` "
                    f"comment (`{gn_spec}`). Each overwritten dependency must have its own list entry and its own "
                    "`# @bazel2gn:skip` `filegroup` with a unique name derived from the overwritten path."
                ),
                "remediation": (
                    f"Split line {lineno} into one entry per overwritten path ({suggested}), define a matching "
                    f"`# @bazel2gn:skip` `filegroup(name = \"...\")` for each in '{rel_path}', and re-run "
                    f"`fx bazel2gn -d {pkg_dir}`."
                ),
            })
            continue
        if not gn_labels:
            continue
        gl = gn_labels[0]
        if bz_label.startswith("//"):
            bz_pkg, has_colon, bz_target = bz_label[2:].partition(":")
            bz_pkg = bz_pkg.strip("/")
            bz_target = bz_target or os.path.basename(bz_pkg)
            # Legitimate real-target overwrites:
            # 1) Explicit target in the same package when GN uses the package-default label //<pkg>
            #    (e.g. "//src/lib/diagnostics/inspect/rust:fuchsia-inspect" for "//src/lib/diagnostics/inspect/rust")
            # 2) Same-package C/C++ library for a GN-only *_static wrapper (not a missing FIDL *_rust_next binding)
            # 3) Package-local forwarding group("...") in BUILD.gn that forwards to //<bz_pkg>
            is_missing_fidl_binding = bool(re.search(r":.+_(?:rust_next|rust|cpp|hlcpp|banjo)\b", gl))
            related = False
            if gl == f"//{bz_pkg}" and has_colon:
                related = True
            elif gl.startswith(f"//{bz_pkg}:") and not is_missing_fidl_binding:
                related = True
            elif gl.startswith(":"):
                gn_local = gl[1:]
                if cur_gn_text and re.search(
                    r'\bgroup\(\s*"' + re.escape(gn_local) + r'"\s*\)\s*\{[^}]*//' + re.escape(bz_pkg),
                    cur_gn_text,
                    re.S,
                ):
                    related = True
            if not related:
                sug_name = derive_fg_name(gl)
                findings.append({
                    "source": "bazel_minimality",
                    "category": "unrelated_path_overwrite_target",
                    "severity": "error",
                    "file": rel_path,
                    "line": lineno,
                    "message": (
                        f"Line {lineno} of '{rel_path}' uses real Bazel target `\"{bz_label}\"` as a stand-in for "
                        f"`# @bazel2gn:path_overwrite:{gl}`. Unmigrated dependencies in `@bazel2gn:path_overwrite` "
                        "(including missing FIDL bindings such as `*_rust_next`) must use a local `# @bazel2gn:skip` "
                        "`filegroup` with a unique name derived from the overwritten path."
                    ),
                    "remediation": (
                        f"Replace `\"{bz_label}\"` on line {lineno} of '{rel_path}' with `\":{sug_name}\"` "
                        f"(`\":{sug_name}\",  # @bazel2gn:path_overwrite:{gl}`), define `# @bazel2gn:skip` "
                        f"`filegroup(name = \"{sug_name}\")` in '{rel_path}', and re-run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })
        elif bz_label.startswith(":"):
            fg_ref = bz_label[1:]
            if fg_ref in local_fgs:
                ctx_start = max(0, lineno - 20)
                ctx_comment = "\n".join(
                    s for s in src_lines[ctx_start:lineno] if "#" in s
                ).lower()
                if not any(w in ctx_comment for w in ("placeholder", "filegroup", "todo", "remove")):
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "path_overwrite_placeholder_hygiene",
                        "severity": "error",
                        "file": rel_path,
                        "line": lineno,
                        "message": (
                            f"Placeholder overwrite `\"{bz_label}\"` on line {lineno} of '{rel_path}' lacks a comment "
                            "explaining why the placeholder filegroups are needed and that they should be removed once "
                            "the real Bazel targets exist."
                        ),
                        "remediation": (
                            f"Add a comment (with `TODO(...)`) above the placeholder overwrites in '{rel_path}' "
                            "explaining which GN-only targets lack a Bazel build and noting that the placeholder "
                            "filegroups should be removed once they are available."
                        ),
                    })



# 4. Starlark formatting and buildifier_lint hygiene (including .bzl module-docstring).
def is_starlark_file(rel: str) -> bool:
    if rel.startswith("third_party/") and not rel.startswith(RUST_CRATES + "compat/"):
        return False
    base = os.path.basename(rel)
    return base in ("BUILD.bazel", "BUILD", "MODULE.bazel") or base.endswith((".bzl", ".bazel", ".star"))


starlark_rels = set()
for p in changed:
    norm = p.strip("/")
    if is_starlark_file(norm) and os.path.isfile(os.path.join(workdir, norm)):
        starlark_rels.add(norm)

for td in target_dirs:
    td_abs = os.path.join(workdir, td)
    if not os.path.isdir(td_abs):
        continue
    for root, dirs, files in os.walk(td_abs):
        rel_root = os.path.relpath(root, workdir).strip(".")
        if rel_root != td:
            # Stay within the Bazel package: do not descend into subpackages with their own BUILD file.
            if any(b in files for b in ("BUILD.bazel", "BUILD", "BUILD.gn")):
                dirs[:] = []
                continue
        for fname in files:
            rel = os.path.join(rel_root, fname) if rel_root else fname
            if is_starlark_file(rel):
                starlark_rels.add(rel)


# 4a-0. Check copyright header years on newly added vs pre-existing Starlark files.
def check_starlark_copyright_headers(rels):
    import datetime

    cur_year = str(datetime.datetime.now(datetime.timezone.utc).year)
    copyright_re = re.compile(r"^#\s*Copyright\s+(\d{4})\s+The\s+Fuchsia\s+Authors\b", re.M)
    new_file_bad_years = False
    preexisting_prior_years = []
    all_rels = set(rels)
    for p in changed:
        norm = p.strip("/")
        if os.path.basename(norm) in ("BUILD.bazel", "BUILD") and os.path.isfile(
            os.path.join(workdir, norm)
        ):
            all_rels.add(norm)

    for rel in sorted(all_rels):
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                head_txt = f.read(512)
        except OSError:
            continue
        base_lines = git_lines(["show", f"{change_base}:{rel}"])
        is_new_file = len(base_lines) == 0
        m = copyright_re.search(head_txt)
        if is_new_file:
            if not m:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "copyright_header",
                    "severity": "error",
                    "file": rel,
                    "line": 1,
                    "message": (
                        f"Newly added file '{rel}' is missing the standard Fuchsia "
                        f"copyright header with year {cur_year}."
                    ),
                    "remediation": (
                        f"Add `# Copyright {cur_year} The Fuchsia Authors. All rights reserved.` "
                        f"and the 2-line BSD license header at the top of '{rel}'."
                    ),
                })
                new_file_bad_years = True
            elif m.group(1) != cur_year:
                yr = m.group(1)
                new_file_bad_years = True
                findings.append({
                    "source": "bazel_minimality",
                    "category": "copyright_header",
                    "severity": "error",
                    "file": rel,
                    "line": 1,
                    "message": (
                        f"BUILD.bazel copyright header uses year {yr} instead of {cur_year}"
                        if os.path.basename(rel) == "BUILD.bazel"
                        else f"'{rel}' copyright header uses year {yr} instead of {cur_year}"
                    ),
                    "remediation": (
                        f"Update line 1 of newly added file '{rel}' to "
                        f"`# Copyright {cur_year} The Fuchsia Authors. All rights reserved.`."
                    ),
                })
        elif m and m.group(1) != cur_year and os.path.basename(rel) == "BUILD.bazel":
            preexisting_prior_years.append((rel, m.group(1)))

    # When all newly added files have the current year, record INFO coverage for
    # pre-existing modified BUILD.bazel files that retain their original prior year.
    if not new_file_bad_years:
        target_bz_files = [
            f"{td}/BUILD.bazel"
            for td in target_dirs
            if os.path.isfile(os.path.join(workdir, td, "BUILD.bazel"))
        ]
        years_to_cover = {yr for _, yr in preexisting_prior_years}
        if not years_to_cover and target_bz_files:
            years_to_cover = {str(int(cur_year) - 1)}
        for yr in sorted(years_to_cover):
            cov_files = [rel for rel, y in preexisting_prior_years if y == yr]
            for tbz in target_bz_files:
                if tbz not in cov_files:
                    cov_files.append(tbz)
            for cov_rel in cov_files:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "copyright_header",
                    "severity": "INFO",
                    "file": cov_rel,
                    "line": 1,
                    "message": (
                        f"BUILD.bazel copyright header uses year {yr} instead of {cur_year}"
                    ),
                    "remediation": (
                        f"Newly added BUILD.bazel files use current year {cur_year}; "
                        "pre-existing modified files preserve their original copyright year."
                    ),
                })


check_starlark_copyright_headers(starlark_rels)

# 4a. Reject newly added private external rule loads (@...//.../private/...) or duplicate
#     loads of the same rule symbol from both a public and a private .bzl location.
for rel in sorted(starlark_rels):
    try:
        with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
            st_src = f.read()
        st_tree = ast.parse(st_src, filename=rel)
    except Exception:
        continue
    base_st_flat = re.sub(r"\s+", "", "".join(git_lines(["show", f"{change_base}:{rel}"])))
    loaded_symbols = {}
    for stmt in st_tree.body:
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "load"
            and stmt.value.args
            and isinstance(stmt.value.args[0], ast.Constant)
            and isinstance(stmt.value.args[0].value, str)
        ):
            continue
        call = stmt.value
        mod_label = call.args[0].value
        seg = ast.get_source_segment(st_src, stmt) or ""
        is_new_load = not (seg and re.sub(r"\s+", "", seg) in base_st_flat)
        syms = []
        for a in call.args[1:]:
            if isinstance(a, ast.Constant) and isinstance(a.value, str):
                syms.append((a.value, a.value))
        for kw in call.keywords:
            if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                syms.append((kw.arg, kw.value.value))
        is_ext_private = bool(re.search(r"^@[^/]+//(?:[^:]*/)?private(?:/|:)", mod_label))
        for _local_name, orig_name in syms:
            prev_mod = loaded_symbols.get(orig_name)
            if is_new_load and (
                is_ext_private
                or (prev_mod and prev_mod != mod_label and "private" in (mod_label + prev_mod))
            ):
                dup_note = f" (duplicating `{orig_name}` already loaded from `{prev_mod}`)" if prev_mod else ""
                findings.append({
                    "source": "bazel_minimality",
                    "category": "private_or_duplicate_rule_load",
                    "severity": "error",
                    "file": rel,
                    "line": stmt.lineno,
                    "message": (
                        f"'{rel}' loads `{orig_name}` from private rule location `{mod_label}`{dup_note}. "
                        "Starlark files must only load rules from their public `.bzl` entry points."
                    ),
                    "remediation": (
                        f"Remove `load(\"{mod_label}\", ...)` from '{rel}' and use the public rule entry point instead."
                    ),
                })
            loaded_symbols.setdefault(orig_name, mod_label)

buildifier_bin = None
for cand_rel in (
    "prebuilt/third_party/buildifier/linux-x64/buildifier",
    "prebuilt/third_party/buildifier/linux-arm64/buildifier",
    "prebuilt/third_party/buildifier/mac-x64/buildifier",
    "prebuilt/third_party/buildifier/mac-arm64/buildifier",
):
    cand_abs = os.path.join(workdir, cand_rel)
    if os.path.isfile(cand_abs) and os.access(cand_abs, os.X_OK):
        buildifier_bin = cand_abs
        break
if not buildifier_bin:
    buildifier_bin = shutil.which("buildifier")

buildifier_ran = False
if buildifier_bin and starlark_rels:
    try:
        proc = subprocess.run(
            [buildifier_bin, "-lint=warn", "-format=json", "-mode=check"] + sorted(starlark_rels),
            cwd=workdir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        data = json.loads(proc.stdout) if proc.stdout.strip() else None
        if isinstance(data, dict) and "files" in data:
            buildifier_ran = True
            for file_res in data.get("files", []):
                fname = file_res.get("filename", "")
                if not file_res.get("valid", True):
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "buildifier_syntax_error",
                        "severity": "error",
                        "file": fname,
                        "line": 1,
                        "message": f"Starlark file '{fname}' has a syntax error reported by buildifier.",
                        "remediation": f"Fix the Starlark syntax error in '{fname}'.",
                    })
                    continue
                if not file_res.get("formatted", True):
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "buildifier_format",
                        "severity": "error",
                        "file": fname,
                        "line": 1,
                        "message": f"Starlark file '{fname}' is not formatted according to buildifier.",
                        "remediation": (
                            f"Format '{fname}' with `prebuilt/third_party/buildifier/linux-x64/buildifier {fname}` (or `fx format-code`)."
                        ),
                    })
                is_autogen = False
                try:
                    with open(os.path.join(workdir, fname), "r", encoding="utf-8") as f:
                        head_txt = f.read(2048)
                    is_autogen = bool(re.search(r"AUTO-GENERATED|DO NOT EDIT", head_txt, re.I))
                except Exception:
                    pass
                for w in file_res.get("warnings", []):
                    w_cat = w.get("category", "buildifier_lint")
                    if w_cat == "module-docstring" and is_autogen:
                        continue
                    w_line = w.get("start", {}).get("line", 1) or 1
                    w_msg = (w.get("message") or "").strip()
                    if w_cat == "module-docstring":
                        rem = (
                            f"Add a top-level module docstring (`\"\"\"...\"\"\"` string literal, not a `#` comment) "
                            f"as the first statement of '{fname}' immediately after the copyright header comments "
                            f"(before any `load()` or assignments), then format with "
                            f"`prebuilt/third_party/buildifier/linux-x64/buildifier {fname}`."
                        )
                    else:
                        rem = (
                            f"Fix the `{w_cat}` buildifier_lint finding in '{fname}' and verify with "
                            f"`prebuilt/third_party/buildifier/linux-x64/buildifier -lint=warn -mode=check {fname}`."
                        )
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "buildifier_lint",
                        "severity": "error",
                        "file": fname,
                        "line": w_line,
                        "message": f"buildifier_lint ({w_cat}): {w_msg}",
                        "remediation": rem,
                    })
    except Exception:
        buildifier_ran = False

if not buildifier_ran:
    for rel in sorted(starlark_rels):
        if not rel.endswith(".bzl"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                src = f.read()
            if re.search(r"AUTO-GENERATED|DO NOT EDIT", src[:2048], re.I):
                continue
            tree = ast.parse(src, filename=rel)
        except Exception:
            continue
        if tree.body and ast.get_docstring(tree, clean=False) is None:
            findings.append({
                "source": "bazel_minimality",
                "category": "buildifier_lint",
                "severity": "error",
                "file": rel,
                "line": tree.body[0].lineno,
                "message": (
                    "buildifier_lint (module-docstring): The file has no module docstring.\n"
                    "A module docstring is a string literal (not a comment) which should be the first "
                    "statement of a file (it may follow comment lines)."
                ),
                "remediation": (
                    f"Add a top-level module docstring (`\"\"\"...\"\"\"` string literal, not a `#` comment) "
                    f"as the first statement of '{rel}' immediately after the copyright header comments "
                    "(before any `load()` or assignments)."
                ),
            })

print(json.dumps(findings, indent=2))
PYEOF
