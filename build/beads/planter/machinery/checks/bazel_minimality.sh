#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing BUILD.bazel minimality, Starlark buildifier
# formatting/lint hygiene, and strict target-directory scope:
#    - redundant default attributes in BUILD.bazel:
#      * crate_name = "<x>" when identical to target name (modulo '-' vs '_'), or
#        on a crate unit-test rustc_test unless disambiguating the bazel2gn GN
#        output_name (e.g. <lib_crate_name>_test)
#      * edition on a rustc_test that sets `crate` (inherited from the crate); a
#        kept `crate_name` next to `crate` needs a comment saying what needs the
#        GN output_name (rustc_test_crate_name_missing_comment, warning)
#      * version = "0.1.0" on first-party (in-tree) rustc_* targets
#      * crate_root = "src/lib.rs" on rustc_library
#      * crate_root = "src/main.rs" on rustc_binary
#      * output_name = "<x>" or out = "<x>" when identical to target name (and
#        unsupported output_name on go_binary / go_binary_host_tool)
#      * commented-out dependency or source labels (`# "//..."`, `# ":..."`) in
#        BUILD.bazel list attributes (`deps`, `srcs`, etc.) carried over from BUILD.gn
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


def git_out(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        ).strip()
    except Exception:
        return ""


def is_ancestor(anc: str, desc: str) -> bool:
    if not anc or not desc:
        return False
    return (
        subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def normalize_git_rev(rev: str) -> str:
    """Map jj relative revsets (@, @-, @--) to git HEAD equivalents for git commands."""
    rev = (rev or "").strip() or "HEAD"
    m = re.fullmatch(r"@(-*)", rev)
    if m:
        return f"HEAD~{len(m.group(1))}" if m.group(1) else "HEAD"
    return rev


# Advance change_base to merge-base(origin/main, HEAD) or HEAD~1 when HEAD was rebased
# onto a newer origin/main after PLANTER_CHANGE_BASE was pinned.
def resolve_effective_change_base(raw_base: str) -> str:
    base = normalize_git_rev(raw_base)
    head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
    base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    origin_main = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    merge_base = (
        git_out(["merge-base", origin_main, head_sha]) if (origin_main and head_sha) else ""
    )
    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and is_ancestor(base_sha, merge_base)
    ):
        base = merge_base
        base_sha = merge_base
    elif head_sha and base_sha and not is_ancestor(base_sha, head_sha):
        base = merge_base or "HEAD~1"
        base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    head_subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
    if parent_sha and head_subj.startswith(("[bazel_migration]", "[bazel2gn]", "[bazel]")):
        head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
        if not target_dirs or any(
            any(p == td or p.startswith(td + "/") for td in target_dirs) for p in head_files
        ):
            if base == "HEAD" or base_sha == head_sha or (
                base_sha and base_sha != parent_sha and is_ancestor(base_sha, parent_sha)
            ):
                return "HEAD~1"
    return base


change_base = resolve_effective_change_base(change_base)
changed = set()
changed.update(git_lines(["diff", "--name-only", change_base]))
# Untracked agent report files (CoderReport*.json) at the checkout root are not part of the
# change (build_only_scope notes them as INFO), so they are not out-of-scope edits either.
untracked = [
    f
    for f in git_lines(["ls-files", "--others", "--exclude-standard"])
    if "/" in f or not re.fullmatch(r"CoderReport[\w.-]*\.json", f)
]
changed.update(untracked)


def get_pkg_area(pkg: str) -> str:
    parts = [p for p in pkg.strip("/").split("/") if p]
    if not parts:
        return ""
    if parts[0] in ("src", "sdk", "tools", "zircon") and len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0]


def is_same_third_party_scope(pkg_a: str, pkg_b: str) -> bool:
    tp_a = "third_party" in pkg_a.strip("/").split("/")
    tp_b = "third_party" in pkg_b.strip("/").split("/")
    return tp_a == tp_b


def package_eliminated_gn_or_sentinel(pkg: str) -> bool:
    gn_path = os.path.join(workdir, pkg, "BUILD.gn")
    if not os.path.isfile(gn_path):
        return True
    try:
        with open(gn_path, encoding="utf-8") as f:
            return "BAZEL2GN SENTINEL" not in f.read()
    except OSError:
        return False


def migrated_dependency_packages():
    """Packages outside target_dirs that are legitimately co-migrated per scoping_changelists.md.

    Per //build/beads/references/migration/scoping_changelists.md Section 2 & Section 3:
    - Never co-migrate cross-area packages, third_party/ packages (with first-party), general-purpose
      libraries, or multi-hop transitive dependency closures.
    - Allowed co-migrations:
      1. Subdirectories of an assigned target directory (`cand_pkg.startswith(td + "/")`, e.g.
         internal helpers `//<dir>/<helper>` or direct test dirs `//<dir>/tests`, Patterns 2.B & 2.D).
      2. A sole same-area GN consumer of `td` ONLY when co-migrating it eliminates `td/BUILD.gn` or
         avoids `bazel2gn` in `td/BUILD.gn` (Pattern 2.A).
      3. A same-area private helper directly depended on by `td` whose `BUILD.gn` is deleted or has
         no `BAZEL2GN SENTINEL` because `td` is its sole consumer (Pattern 2.B).
    """
    added = set(git_lines(["diff", "--name-only", "--diff-filter=AM", change_base])) | set(untracked)
    candidates = {
        os.path.dirname(p)
        for p in added
        if os.path.basename(p) == "BUILD.bazel"
        and not p.strip("/").startswith(ALLOWED_GLOBAL_PREFIXES)
        and not p.strip("/").startswith("bundles/")
    }
    candidates -= set(target_dirs)
    if not candidates or not target_dirs:
        return set()
    label_re = re.compile(r'"@?//([^":]+)(?::([^"\s]+))?"')
    td_direct_deps = set()
    for td in target_dirs:
        try:
            with open(os.path.join(workdir, td, "BUILD.bazel"), encoding="utf-8") as f:
                text = f.read()
        except OSError:
            continue
        for m in label_re.finditer(text):
            ref, target = m.group(1).strip("/"), m.group(2) or ""
            if ref and target not in ("__pkg__", "__subpackages__"):
                td_direct_deps.add(ref)
    found = set()
    for cand_pkg in sorted(candidates):
        # 1. Child/descendant directory of an assigned target directory (Patterns 2.B & 2.D)
        if any(
            cand_pkg.startswith(td + "/") and is_same_third_party_scope(cand_pkg, td)
            for td in target_dirs
        ):
            found.add(cand_pkg)
            continue
        # Must be in the same area and third_party scope as at least one target_dir
        matching_tds = [
            td
            for td in target_dirs
            if get_pkg_area(cand_pkg) == get_pkg_area(td)
            and is_same_third_party_scope(cand_pkg, td)
        ]
        if not matching_tds:
            continue
        # 2. Pattern 2.A: Sole same-area consumer of td that eliminates td/BUILD.gn or bazel2gn in td
        cand_texts = []
        for fn in ("BUILD.gn", "BUILD.bazel"):
            rel_f = f"{cand_pkg}/{fn}"
            cand_texts.extend(git_lines(["show", f"{change_base}:{rel_f}"]))
            try:
                with open(os.path.join(workdir, rel_f), encoding="utf-8") as f:
                    cand_texts.append(f.read())
            except OSError:
                pass
        joined = "\n".join(cand_texts)
        if any(
            re.search(
                r'(?:@gn_targets//(?:[^\s"\':]+/)?|//)' + re.escape(td) + r'(?=[:"/\s\'])',
                joined,
            )
            and package_eliminated_gn_or_sentinel(td)
            for td in matching_tds
        ):
            found.add(cand_pkg)
            continue
        # 3. Pattern 2.B: Direct same-area private helper of td that needs no bazel2gn
        if cand_pkg in td_direct_deps and package_eliminated_gn_or_sentinel(cand_pkg):
            found.add(cand_pkg)
    return found


dependency_dirs = migrated_dependency_packages()
newly_added_files = set(git_lines(["diff", "--name-only", "--diff-filter=A", change_base])) | set(untracked)


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


# Check whether a deleted out-of-package .cml file was referenced by an in-scope
# package's pre-migration BUILD.gn and relocated into that Bazel package.
def is_relocated_in_scope_cml(norm: str) -> bool:
    if not norm.endswith(".cml") or os.path.exists(os.path.join(workdir, norm)):
        return False
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    refs = [change_base, "HEAD~1"] if change_base == "HEAD" else [change_base]
    for td in in_scope:
        for ref in refs:
            gn_txt = "\n".join(git_lines(["show", f"{ref}:{td}/BUILD.gn"]))
            if not gn_txt:
                continue
            code_gn = re.sub(r"#[^\n]*", "", gn_txt)
            for raw_cml in re.findall(r'"([^"\n]+\.cml)"', code_gn):
                resolved = (
                    raw_cml[2:].lstrip("/")
                    if raw_cml.startswith("//")
                    else os.path.normpath(os.path.join(td, raw_cml)).replace("\\", "/")
                )
                if resolved == norm:
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
    if (
        os.path.basename(norm) in ("BUILD.bazel", "BUILD", "BUILD.gn")
        or norm.endswith((".bzl", ".gni", ".cml"))
    ) and references_in_scope_package_at_base(norm):
        # Collateral updates to an existing caller's BUILD.gn/BUILD.bazel (e.g. visibility,
        # test_suite registration, or bazel_host_tool wiring) are allowed, but full migration of
        # an out-of-scope package (creating <pkg_dir>/BUILD.bazel or adding BAZEL2GN SENTINEL to
        # <pkg_dir>/BUILD.gn) is forbidden unless <pkg_dir> is in target_dirs or dependency_dirs.
        if pkg_dir and not pkg_dir.startswith(ALLOWED_GLOBAL_PREFIXES) and not pkg_dir.startswith("bundles/"):
            if f"{pkg_dir}/BUILD.bazel" in newly_added_files:
                return False
            base_gn = "\n".join(git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"]))
            if base_gn and "BAZEL2GN SENTINEL" not in base_gn:
                try:
                    with open(os.path.join(workdir, pkg_dir, "BUILD.gn"), encoding="utf-8") as f:
                        if "BAZEL2GN SENTINEL" in f.read():
                            return False
                except OSError:
                    pass
        return True
    if is_relocated_in_scope_cml(norm):
        return True
    if norm in allowed_gni_files:
        return True
    if pkg_dir in allowed_lint_pkgs and os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel"):
        return True
    for prefix in ALLOWED_GLOBAL_PREFIXES:
        if norm.startswith(prefix):
            return True
    if norm in (
        "build/BUILD.gn",
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
            if path.strip("/") == "build/bazel/debug_symbols/aspects.bzl":
                shared_rem = (
                    f"Revert '{path}' (`git checkout {change_base} -- {path}`). When "
                    "`export_last_build_debug_symbols` fails because an assembled "
                    "`fx_package` packages a binary that depends on a `rustc_dylib` "
                    "(`rust_dylib_library`), do not edit `build/bazel/debug_symbols/aspects.bzl`; "
                    "instead remove the assembled `fx_package` target from the package's "
                    "`BUILD.bazel`, keep `fuchsia_package` above `## BAZEL2GN SENTINEL` in "
                    "`BUILD.gn` (while keeping `fx_component`, `fx_component_manifest`, and "
                    "`fx_packaged_binary` in `BUILD.bazel`), keep its "
                    "`//bundles/assembly/bazel_inputs/<dir>` bridge in "
                    "`bundles/assembly/BUILD.bazel` and `bundles/assembly/bazel_inputs/BUILD.gn`, "
                    "and remove the package target from `Bazel-Migration-Target:` footers and "
                    "commit-body migration claims."
                )
            else:
                shared_rem = (
                    f"Revert '{path}' (`git checkout {change_base} -- {path}`). If a migrated "
                    "target calls a legacy host-tool macro (`go_binary_host_tool`, "
                    "`py_binary_host_tool`) and fails visibility on "
                    "`//build/bazel/versioning:is_api_level_PLATFORM`, add `\"//<dir>:__pkg__\"` "
                    "to `is_api_level_PLATFORM`'s `visibility` list in "
                    "'build/bazel/versioning/BUILD.bazel' instead of editing "
                    f"'{path}'. If a target passes a redundant or unsupported attribute (such as "
                    "`output_name` matching `name` on `go_binary_host_tool`), omit or fix the "
                    "attribute in the target's BUILD.bazel."
                )
            findings.append({
                "source": "bazel_minimality",
                "category": "out_of_scope_package_modified",
                "severity": "error",
                "file": path,
                "line": 1,
                "message": (
                    f"File '{path}' is a shared Bazel rule or macro definition outside the "
                    f"assigned target directory scope ({', '.join(target_dirs)}). Package "
                    "migrations must never modify shared rules or macros under '//build/bazel/' "
                    "(such as loading private rule definitions, wrapping macros, editing debug "
                    "symbol aspects, or adding custom attributes)."
                ),
                "remediation": shared_rem,
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
                    f"({', '.join(target_dirs)}). Per //build/beads/references/migration/scoping_changelists.md, "
                    "do not sweep cross-area packages, general-purpose dependencies, transitive closures, "
                    "or downstream consumers that do not eliminate BUILD.gn/bazel2gn into the same CL."
                ),
                "remediation": (
                    f"Revert changes to '{path}' (`git checkout {change_base} -- {path}` or `rm -f {path}`) "
                    "and restrict modifications strictly to the assigned target package(s) and allowed "
                    "co-migrated subdirectories."
                ),
            })


# Enforce CL directory count (<= 5) and added-line count (<= ~500 for multi-directory CLs)
# per //build/beads/references/migration/scoping_changelists.md.
def check_cl_scoping_guidelines():
    migrated_pkgs = set()
    for p in sorted(changed):
        norm = p.strip("/")
        if os.path.basename(norm) != "BUILD.bazel":
            continue
        if norm.startswith(ALLOWED_GLOBAL_PREFIXES) or norm.startswith(("bundles/", RUST_CRATES)):
            continue
        pkg = os.path.dirname(norm)
        if not pkg:
            continue
        if norm in newly_added_files or pkg in target_dirs or pkg in dependency_dirs:
            migrated_pkgs.add(pkg)
    if len(migrated_pkgs) <= 1:
        return
    added_lines = 0
    for p in sorted(changed):
        norm = p.strip("/")
        if norm.startswith(ALLOWED_GLOBAL_PREFIXES) or norm.startswith(("bundles/", RUST_CRATES)):
            continue
        if norm in untracked:
            try:
                with open(os.path.join(workdir, norm), encoding="utf-8", errors="ignore") as f:
                    added_lines += sum(1 for _ in f)
            except OSError:
                pass
        else:
            for dl in git_lines(["diff", "--unified=0", change_base, "--", norm]):
                if dl.startswith("+") and not dl.startswith("+++"):
                    added_lines += 1
    anchor = f"{target_dirs[0]}/BUILD.bazel" if target_dirs else f"{sorted(migrated_pkgs)[0]}/BUILD.bazel"
    if len(migrated_pkgs) > 5 or added_lines > 550:
        pkg_list_str = ", ".join(f"//{d}" for d in sorted(migrated_pkgs))
        findings.append({
            "source": "bazel_minimality",
            "category": "cl_scope_exceeded",
            "severity": "error",
            "file": anchor,
            "line": 1,
            "message": (
                f"Change migrates {len(migrated_pkgs)} directories ({pkg_list_str}) and adds "
                f"{added_lines} new lines, exceeding the CL scoping limits in "
                "//build/beads/references/migration/scoping_changelists.md (at most 3-5 directories "
                "and ~500 new lines of code max)."
            ),
            "remediation": (
                "Reduce the scope of this CL by reverting extra co-migrated directories so that "
                "at most 5 directories and <= ~500 new lines are migrated in a single CL."
            ),
        })


check_cl_scoping_guidelines()

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
    # Whitespace-free copy of the file and per-target call slices at the change base:
    # pre-existing targets, attributes, and @bazel2gn annotations predate the change.
    base_bazel_text = "\n".join(git_lines(["show", f"{change_base}:{rel_path}"]))
    base_bazel_flat = re.sub(r"\s+", "", base_bazel_text)
    base_target_flats = {}
    if base_bazel_text:
        try:
            base_tree = ast.parse(base_bazel_text, filename=rel_path)
            for b_node in ast.walk(base_tree):
                if not isinstance(b_node, ast.Call):
                    continue
                b_rule = (
                    b_node.func.id
                    if isinstance(b_node.func, ast.Name)
                    else (b_node.func.attr if isinstance(b_node.func, ast.Attribute) else "")
                )
                for b_kw in b_node.keywords:
                    if (
                        b_kw.arg == "name"
                        and isinstance(b_kw.value, ast.Constant)
                        and isinstance(b_kw.value.value, str)
                    ):
                        b_seg = ast.get_source_segment(base_bazel_text, b_node) or ""
                        base_target_flats[(b_rule, b_kw.value.value)] = re.sub(
                            r"\s+", "", b_seg
                        )
        except Exception:
            pass

    def is_preexisting_target_attr(r_name: str, t_name: str, attr_snippet: str = "") -> bool:
        """True when target (r_name, t_name) and optional attr_snippet existed at change_base."""
        b_flat = base_target_flats.get((r_name, t_name))
        if b_flat is None:
            return False
        if not attr_snippet:
            return True
        return re.sub(r"\s+", "", attr_snippet) in b_flat

    pkg_dir = os.path.dirname(rel_path)
    base_gn_lines = git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"])
    base_gn_text = "\n".join(base_gn_lines)
    src_lines = source.splitlines()

    # Collect in-package rustc_library targets (mapping name -> normalized crate_name)
    # so unit-test rustc_test targets can be checked for `crate = ":<lib>"` reuse and
    # valid bazel2gn test output_name disambiguation.
    pkg_rustc_libs = {}
    for stmt_node in ast.walk(tree):
        if isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name):
            if stmt_node.func.id in ("rustc_library", "rustc_proc_macro"):
                l_name, l_crate = "", ""
                for kw in stmt_node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        l_name = kw.value.value
                    elif kw.arg == "crate_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        l_crate = kw.value.value
                if l_name:
                    pkg_rustc_libs[l_name] = (l_crate or l_name).replace("-", "_")

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
        if (
            "srcs" in kw_names
            and has_matching_lib
            and "crate" not in kw_names
            and not is_preexisting_target_attr(r_name, t_name, "srcs=")
        ):
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
            else:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "rustc_test_repeats_library_srcs",
                    "severity": "error",
                    "file": rel_path,
                    "line": srcs_line,
                    "message": (
                        f"Target '{t_name}' (rustc_test) repeats `srcs` (and library features/deps) "
                        f"instead of referencing sibling library `:{lib_stem}` via `crate` "
                        "(`bazel2gn` natively supports `crate = \":<lib>\"` on `rustc_test` and "
                        "inlines the crate's `srcs`, `edition`, `deps`, and `proc_macro_deps` into "
                        "the generated GN `rustc_test`)."
                    ),
                    "remediation": (
                        f"Set `crate = \":{lib_stem}\"` on '{t_name}' and remove `srcs`, "
                        "`edition`, `crate_name`, any `deps`/`proc_macro_deps`/`crate_features` "
                        f"already inherited from `:{lib_stem}`, any single-use `_<LIB>_*` "
                        "variables, and any comment claiming `bazel2gn` requires `srcs`/`deps` "
                        f"on `rustc_test`, then re-run `fx bazel2gn -d {pkg_dir}`."
                    ),
                })
        if "crate_name" in k_map and ("crate" in kw_names or has_matching_lib):
            c_name, c_line = k_map["crate_name"]
            gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
            has_gn_sentinel = os.path.isfile(gn_path) and "BAZEL2GN SENTINEL" in open(
                gn_path, encoding="utf-8"
            ).read()
            norm_t = t_name.replace("-", "_")
            norm_c = c_name.replace("-", "_")
            crate_kw = next(
                (
                    kw.value.value.lstrip(":")
                    for kw in call_node.keywords
                    if kw.arg == "crate"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ),
                lib_stem,
            )
            lib_crate_name = pkg_rustc_libs.get(crate_kw, "")
            base_gn_norm = base_gn_text.replace("-", "_")
            # bazel2gn maps rustc_test `crate_name` to GN `output_name`. Allow a non-default
            # test crate_name (distinct from the library crate_name itself) when preserving a
            # base BUILD.gn custom test name/output_name or disambiguating the GN test binary
            # in a dual-build package.
            valid_gn_test_output_name = (
                norm_c != norm_t
                and norm_c != lib_crate_name
                and (
                    f'"{norm_c}"' in base_gn_norm
                    or (
                        has_gn_sentinel
                        and norm_c.endswith(
                            ("_test", "_tests", "_unittests", "_lib_test", "_bin_test")
                        )
                    )
                )
            )
            if not valid_gn_test_output_name and not is_preexisting_target_attr(r_name, t_name, f'crate_name="{c_name}"'):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": c_line,
                    "message": (
                        f"Target '{t_name}' (rustc_test) specifies `crate_name = \"{c_name}\"`, "
"which is unnecessary on a crate unit test (unless setting a distinct "
                        "`<crate>_test` binary `output_name` for `bazel2gn`)."
                    ),
                    "remediation": (
                        f"Remove `crate_name` from `{t_name}` in '{rel_path}' (or set it to "
                        f"`\"{lib_crate_name or norm_t}_test\"` only when `bazel2gn` must emit a "
                        "disambiguated GN `output_name`)."
                    ),
                })
        elif "crate_name" in k_map:
            # A standalone rustc_test (no `crate`, no sibling library) already gets
            # name.replace("-", "_") as its crate and GN output name in both builds.
            c_name, c_line = k_map["crate_name"]
            if c_name.replace("-", "_") == t_name.replace("-", "_") and not is_preexisting_target_attr(
                r_name, t_name, f'crate_name="{c_name}"'
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "redundant_default_attribute",
                    "severity": "error",
                    "file": rel_path,
                    "line": c_line,
                    "message": (
                        f"Target '{t_name}' (rustc_test) specifies `crate_name = \"{c_name}\"`, which is "
                        "already the default derived from `name` (with `-` replaced by `_`) in both rules_rust "
                        "and GN, so bazel2gn only emits a redundant `output_name`."
                    ),
                    "remediation": (
                        f"Remove `crate_name` from `{t_name}` in '{rel_path}' and re-run "
                        f"`fx bazel2gn -d {pkg_dir}`."
                    ),
                })
        if "crate" in kw_names and "edition" in k_map and not is_preexisting_target_attr(r_name, t_name, "edition="):
            ed_val, ed_line = k_map["edition"]
            findings.append({
                "source": "bazel_minimality",
                "category": "redundant_default_attribute",
                "severity": "error",
                "file": rel_path,
                "line": ed_line,
                "message": (
                    f"Target '{t_name}' (rustc_test) sets `edition = \"{ed_val}\"` next to `crate`, but both "
                    "rules_rust and bazel2gn take `edition` from the crate and ignore it on the test."
                ),
                "remediation": (
                    f"Remove `edition` from `{t_name}` in '{rel_path}' (bazel2gn output does not change)."
                ),
            })
        if (
            "crate" in kw_names
            and "crate_name" in k_map
            and not preceding_comment_lines(k_map["crate_name"][1])
            and not is_preexisting_target_attr(r_name, t_name, f'crate_name="{k_map["crate_name"][0]}"')
        ):
            c_name, c_line = k_map["crate_name"]
            findings.append({
                "source": "bazel_minimality",
                "category": "rustc_test_crate_name_missing_comment",
                "severity": "warning",
                "file": rel_path,
                "line": c_line,
                "message": (
                    f"Target '{t_name}' (rustc_test) sets `crate_name = \"{c_name}\"` next to `crate`. "
                    "rules_rust ignores it there; it only sets the GN `output_name` bazel2gn emits, which "
                    "readers can't tell from BUILD.bazel."
                ),
                "remediation": (
                    f"Add a short comment above `crate_name` in `{t_name}` saying what needs the GN binary "
                    f"name `{c_name}` (e.g. the `.cml` `program.binary`, or avoiding a `lib_test` collision)."
                ),
            })

    # Check that newly added rustc_test / rust_test / host_rustc_test targets do not use
    # `# @bazel2gn:raw_attribute:output_name = "<out_name>"`: naming the test target
    # itself `name = "<out_name>"` gives GN `bin/<out_name>` by default without an
    # annotation while `wrap_host_rust_test` preserves its `tests.json` target name.
    # Pre-existing test targets at change_base are exempt so modifying a dependency's
    # BUILD.bazel (e.g. widening visibility) does not demand renaming existing targets
    # (which would violate migration_sanity/dropped_preexisting_bazel_target).
    def check_rustc_test_raw_output_name(call_node, r_name, t_name):
        """Flag `# @bazel2gn:raw_attribute:output_name` on newly added Rust test targets."""
        if r_name not in ("rustc_test", "rust_test", "host_rustc_test") or not t_name:
            return
        idx = getattr(call_node, "lineno", 1) - 2
        raw_out_re = re.compile(
            r'#\s*@bazel2gn:raw_attribute:\s*output_name\s*=\s*"([^"]+)"'
        )
        while idx >= 0:
            s_line = src_lines[idx].strip()
            if not s_line:
                idx -= 1
                continue
            if s_line.startswith("#"):
                m = raw_out_re.search(s_line)
                if m:
                    if is_preexisting_target_attr(r_name, t_name) and (
                        re.sub(r"\s+", "", s_line) in base_bazel_flat
                    ):
                        idx -= 1
                        continue
                    out_name = m.group(1)
                    annot_line = idx + 1
                    msg_text = (
                        f"Target '{t_name}' ({r_name}) uses "
                        f"`# @bazel2gn:raw_attribute:output_name = \"{out_name}\"`, "
                        f"which is unnecessary when the test target is named "
                        f"`\"{out_name}\"` (giving GN `bin/{out_name}` by default to "
                        "match any `.cml` manifest while `wrap_host_rust_test` keeps "
                        "its `tests.json` name)."
                    )
                    rem_text = (
                        f"Remove `# @bazel2gn:raw_attribute:output_name = \"{out_name}\"` "
                        f"above `{t_name}` in '{rel_path}', rename the `{r_name}` target "
                        f"to `name = \"{out_name}\"`, update any in-package test wrappers "
                        f"(`binary_name = \"{out_name}\"` on `wrap_host_rust_test` while "
                        f"keeping its `name`, `binary = \":{out_name}\"` on "
                        f"`fx_packaged_binary`, or `unit_tests = [\":{out_name}\"]` on "
                        f"`fx_unittest_package` / `fx_unittest_component`), re-run "
                        f"`fx bazel2gn -d {pkg_dir}`, and update any hand-written "
                        f"`{pkg_dir}/BUILD.gn` references (`\":{t_name}\"` and "
                        f"`\":{t_name}($host_toolchain)\"`) to `\":{out_name}\"` and "
                        f"`\":{out_name}($host_toolchain)\"`."
                    )
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "rustc_test_raw_output_name_annotation",
                        "severity": "error",
                        "file": rel_path,
                        "line": annot_line,
                        "message": msg_text,
                        "remediation": rem_text,
                    })
                idx -= 1
                continue
            break

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

    # Check for redundant single-target forwarding alias(name = "<outer>", actual = ":<inner>")
    # or unfolded `name = "lib"` with `crate_name = "<outer>"` when `:<inner>` has no external
    # callers and its crate/output name already matches `<outer>`.
    def check_redundant_forwarding_alias(tree_node, gn_text):
        """Flag redundant `alias(name = "<outer>", actual = ":<inner>")` when `:<inner>` can be folded into `<outer>`."""
        if not gn_text:
            return
        aliases = {}
        libs = {}
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                continue
            call_n = stmt_node.value
            r_id = call_n.func.id
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            k_strs = {
                kw.arg: kw.value.value
                for kw in call_n.keywords
                if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)
            }
            nm = k_strs.get("name", "")
            if not nm:
                continue
            if r_id == "alias" and k_strs.get("actual", "").startswith(":"):
                aliases[nm] = (k_strs["actual"][1:], getattr(call_n, "lineno", 1))
            elif r_id in (
                "rustc_library",
                "rust_library",
                "rustc_proc_macro",
                "rust_proc_macro",
                "rustc_dylib",
                "fx_cc_library",
                "cc_library",
                "fx_cc_library_headers",
            ):
                libs[nm] = (r_id, k_strs, getattr(call_n, "lineno", 1))
        candidates = dict(aliases)
        if "lib" in libs:
            _, lib_k, lib_line = libs["lib"]
            lib_crate = lib_k.get("crate_name", "")
            if lib_crate and lib_crate != "lib" and lib_crate not in candidates and lib_crate not in libs:
                candidates[lib_crate] = ("lib", lib_line)
        if not candidates:
            return
        for outer, (inner, alias_line) in sorted(candidates.items()):
            if inner == outer or inner not in libs:
                continue
            r_id, k_strs, _ = libs[inner]
            if r_id.startswith(("rustc_", "rust_")):
                eff_crate = k_strs.get("crate_name", inner)
                if eff_crate != outer and eff_crate != outer.replace("-", "_"):
                    continue
            elif inner != "lib" or outer != os.path.basename(pkg_dir):
                continue
            grp_m = re.search(
                r'\bgroup\(\s*"' + re.escape(outer) + r'"\s*\)\s*\{([^}]*)\}', gn_text
            )
            if not grp_m:
                continue
            grp_body = grp_m.group(1)
            grp_deps = re.findall(r'"([^"]+)"', grp_body)
            if grp_deps not in ([f":{inner}"], [f"//{pkg_dir}:{inner}"]):
                continue
            other_refs = re.findall(
                r'":' + re.escape(inner) + r'"|"//' + re.escape(pkg_dir) + r":" + re.escape(inner) + r'"',
                source,
            )
            if len(other_refs) > 2:
                continue
            try:
                ext_out = subprocess.check_output(
                    [
                        "git",
                        "-C",
                        workdir,
                        "grep",
                        "-l",
                        "-F",
                        f"//{pkg_dir}:{inner}",
                        "--",
                        "BUILD.gn",
                        "BUILD.bazel",
                        "*.gni",
                        "*.bzl",
                        "**/BUILD.gn",
                        "**/BUILD.bazel",
                    ],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                ext_out = ""
            ext_files = [
                f.strip().strip("/")
                for f in ext_out.splitlines()
                if f.strip().strip("/")
                and not (
                    f.strip().strip("/") == pkg_dir
                    or f.strip().strip("/").startswith(pkg_dir + "/")
                )
            ]
            if ext_files:
                continue
            findings.append({
                "source": "bazel_minimality",
                "category": "redundant_forwarding_alias",
                "severity": "error",
                "file": rel_path,
                "line": alias_line,
                "message": (
                    f"Target '{outer}' (`alias` to `:{inner}`) and '{inner}' (`{r_id}`) in "
                    f"'{rel_path}' preserve a redundant single-target forwarding indirection "
                    f"when no external caller depends on `//{pkg_dir}:{inner}`."
                ),
                "remediation": (
                    f"Fold `:{inner}` directly into `{r_id}(name = \"{outer}\", ...)` in "
                    f"'{rel_path}' (removing `alias(name = \"{outer}\")`, omitting `crate_name` "
                    f"when it matches `{outer}`, updating any same-package unit test "
                    f"`crate = \":{outer}\"`, and removing `group(\"{outer}\")` from "
                    f"'{pkg_dir}/BUILD.gn'), then re-run `fx bazel2gn -d {pkg_dir}`."
                ),
            })

    # Check for commented-out dependency or source entries carried over into BUILD.bazel lists.
    def check_commented_out_deps(tree_node):
        """Flag commented-out dependency or source labels in BUILD.bazel list attributes."""
        commented_item_re = re.compile(
            r'^\s*#\s*["\']((?://|:|@)[^"\'\s]+|[A-Za-z0-9_./-]+\.'
            r'(?:rs|cc|cpp|c|h|hpp|py|go|fidl|cml|json\d*))["\']\s*,?\s*(?:#.*)?$'
        )
        list_attrs = {
            "deps",
            "test_deps",
            "implementation_deps",
            "link_deps",
            "proc_macro_deps",
            "srcs",
            "hdrs",
            "data",
            "test_data",
            "public_deps",
            "runtime_deps",
            "compile_data",
        }

        prev_gn_text = base_gn_text
        if "## BAZEL2GN SENTINEL" in prev_gn_text or not prev_gn_text:
            prev_gn_text = "\n".join(git_lines(["show", f"HEAD~1:{pkg_dir}/BUILD.gn"])) or prev_gn_text
        gn_commented_lines = [
            ln_txt.strip()
            for ln_txt in prev_gn_text.splitlines()
            if commented_item_re.match(ln_txt)
        ]
        gn_had_commented_deps = bool(gn_commented_lines)

        def scan_span(
            start_ln: int,
            end_ln: int,
            owner_desc: str,
            attr_desc: str,
            is_deps: bool,
            target_name: str = "",
            target_ln: int = 1,
        ):
            items = []
            for ln in range(max(1, start_ln), min(len(src_lines), end_ln) + 1):
                m = commented_item_re.match(src_lines[ln - 1])
                if m:
                    items.append((ln, m.group(1)))
            if not items:
                if is_deps and gn_had_commented_deps:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "commented_out_dependency",
                        "severity": "INFO",
                        "file": rel_path,
                        "line": min(len(src_lines), start_ln + 1),
                        "message": (
                            f"{owner_desc} in '{rel_path}' omits commented-out {attr_desc} "
                            "that were present in BUILD.gn."
                        ),
                        "remediation": (
                            "Commented-out dependencies in BUILD.gn are omitted from BUILD.bazel."
                        ),
                    })
                    if target_name:
                        for raw_comment in gn_commented_lines:
                            findings.append({
                                "source": "bazel_minimality",
                                "category": "dropped_comment",
                                "severity": "INFO",
                                "file": rel_path,
                                "line": target_ln,
                                "message": (
                                    f'Target "{target_name}" dropped comment from '
                                    f"BUILD.gn: {raw_comment}"
                                ),
                                "remediation": (
                                    "Commented-out dependencies in BUILD.gn are "
                                    "omitted from BUILD.bazel."
                                ),
                            })
                return
            first_ln = items[0][0]
            quoted_items = ", ".join(f'`"{lbl}"`' for _, lbl in items)
            line_nums = ", ".join(str(ln) for ln, _ in items)
            findings.append({
                "source": "bazel_minimality",
                "category": "commented_out_dependency",
                "severity": "error",
                "file": rel_path,
                "line": first_ln,
                "message": (
                    f"{owner_desc} in '{rel_path}' contains commented-out {attr_desc} "
                    f"({quoted_items} on line(s) {line_nums}); dead commented-out "
                    "dependencies or sources must be removed rather than carried over "
                    "to BUILD.bazel."
                ),
                "remediation": (
                    f"Delete the commented-out line(s) ({line_nums}) from {attr_desc} "
                    f"in '{rel_path}' (and re-run `fx bazel2gn -d {pkg_dir}` if "
                    "dual-building)."
                ),
            })

        for stmt_node in tree_node.body:
            if (
                isinstance(stmt_node, ast.Assign)
                and len(stmt_node.targets) == 1
                and isinstance(stmt_node.targets[0], ast.Name)
            ):
                v_name = stmt_node.targets[0].id
                scan_span(
                    getattr(stmt_node, "lineno", 1),
                    getattr(stmt_node, "end_lineno", getattr(stmt_node, "lineno", 1)),
                    f"Top-level list `{v_name}`",
                    f"`{v_name}` entries",
                    False,
                )
            elif (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                call_n = stmt_node.value
                r_id = call_n.func.id
                t_name = ""
                for kw in call_n.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        t_name = kw.value.value
                owner = f"Target '{t_name}' ({r_id})" if t_name else f"`{r_id}(...)`"
                for kw in call_n.keywords:
                    if kw.arg in list_attrs:
                        start_l = getattr(kw, "lineno", getattr(kw.value, "lineno", 1))
                        end_l = getattr(kw, "end_lineno", getattr(kw.value, "end_lineno", start_l))
                        scan_span(
                            start_l,
                            end_l,
                            owner,
                            f"`{kw.arg}` entries",
                            kw.arg == "deps",
                            t_name,
                            getattr(call_n, "lineno", 1),
                        )

    # Check host platform constraint conventions (HOST_OS_CONSTRAINTS vs HOST_CONSTRAINTS
    # and preserving `if (is_host)` / `assert(is_host)` guards on migrated targets).
    def check_host_constraints_parity(tree_node, gn_text):
        """Verify host-only targets use HOST_OS_CONSTRAINTS and preserve GN is_host guards."""
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                continue
            call_n = stmt_node.value
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            if (
                call_n.func.id == "load"
                and call_n.args
                and isinstance(call_n.args[0], ast.Constant)
                and call_n.args[0].value == "@platforms//host:constraints.bzl"
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "disallowed_host_constraints",
                    "severity": "error",
                    "file": rel_path,
                    "line": getattr(stmt_node, "lineno", 1),
                    "message": (
                        f"'{rel_path}' loads `@platforms//host:constraints.bzl` (`HOST_CONSTRAINTS`), "
                        "which must not be used when migrating host targets."
                    ),
                    "remediation": (
                        f"Replace `load(\"@platforms//host:constraints.bzl\", \"HOST_CONSTRAINTS\")` "
                        "and `target_compatible_with = HOST_CONSTRAINTS` in "
                        f"'{rel_path}' with `load(\"//build/bazel/platforms:constraints.bzl\", "
                        "\"HOST_OS_CONSTRAINTS\")` and `target_compatible_with = HOST_OS_CONSTRAINTS`."
                    ),
                })

        prev_gn = gn_text
        if "## BAZEL2GN SENTINEL" in prev_gn or not prev_gn:
            prev_gn = "\n".join(git_lines(["show", f"HEAD~1:{pkg_dir}/BUILD.gn"])) or prev_gn
        if not prev_gn or not re.search(r"\b(?:if|assert)\s*\(\s*is_host\s*\)", prev_gn):
            return
        host_guarded_names = set()
        for m_host in re.finditer(r"\bif\s*\(\s*is_host\s*\)\s*\{", prev_gn):
            depth, idx = 1, m_host.end()
            while idx < len(prev_gn) and depth > 0:
                if prev_gn[idx] == "{":
                    depth += 1
                elif prev_gn[idx] == "}":
                    depth -= 1
                idx += 1
            blk = prev_gn[m_host.end() : max(m_host.end(), idx - 1)]
            for m_t in re.finditer(r'\b[A-Za-z_]\w*\(\s*"([^"]+)"\s*\)\s*\{', blk):
                host_guarded_names.add(m_t.group(1))
        if not host_guarded_names:
            return
        host_lib_bin_rules = {
            "rustc_library",
            "rustc_binary",
            "fx_cc_library",
            "fx_cc_binary",
            "fx_cc_test",
            "cc_library",
            "cc_binary",
            "go_library",
            "go_binary",
            "go_binary_host_tool",
            "idk_go_binary_host_tool",
        }
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
                and stmt_node.value.func.id in host_lib_bin_rules
            ):
                continue
            call_n = stmt_node.value
            t_name = ""
            has_tcw = False
            for kw in call_n.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    t_name = kw.value.value
                elif kw.arg == "target_compatible_with":
                    has_tcw = True
            if t_name in host_guarded_names and not has_tcw:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "missing_host_os_constraints",
                    "severity": "error",
                    "file": rel_path,
                    "line": getattr(call_n, "lineno", 1),
                    "message": (
                        f"Target '{t_name}' ({call_n.func.id}) in '{rel_path}' was guarded by "
                        f"`if (is_host)` in `{pkg_dir}/BUILD.gn` but omits "
                        "`target_compatible_with = HOST_OS_CONSTRAINTS`."
                    ),
                    "remediation": (
                        "Add `load(\"//build/bazel/platforms:constraints.bzl\", "
                        "\"HOST_OS_CONSTRAINTS\")` and `target_compatible_with = "
                        f"HOST_OS_CONSTRAINTS` on '{t_name}' in '{rel_path}', then "
                        f"re-run `fx bazel2gn -d {pkg_dir}` if dual-building."
                    ),
                })

    check_single_use_private_vars(tree)
    check_wrap_host_rust_test_naming(tree, base_gn_text)
    check_unnecessary_test_check_cfg_flags(tree)
    check_repeated_deps_select(tree)
    check_redundant_forwarding_alias(tree, base_gn_text)
    check_commented_out_deps(tree)
    check_host_constraints_parity(tree, base_gn_text)

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
            and not is_preexisting_target_attr(rule_name, target_name)
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
        check_rustc_test_raw_output_name(node, rule_name, target_name)
        if "crate_name" in kw_map and target_name and rule_name not in ("rustc_binary", "rustc_test"):
            c_name, lineno = kw_map["crate_name"]
            if (
                c_name == target_name or c_name == target_name.replace("-", "_")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'crate_name="{c_name}"'
            ):
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
            if (
                ver in ("0.1.0", "0.0.1") or ver.startswith("-")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'version="{ver}"'
            ):
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
            if (
                (rule_name == "rustc_library" and c_root == "src/lib.rs")
                or (rule_name == "rustc_binary" and c_root == "src/main.rs")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'crate_root="{c_root}"'
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
            if is_preexisting_target_attr(
                rule_name, target_name, f'{out_attr}="{out_val}"'
            ):
                continue
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
                kw_seg = ast.get_source_segment(source, kw) or ""
                if is_preexisting_target_attr(rule_name, target_name, kw_seg):
                    continue
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
    if base_gn_text and not (pkg_dir not in target_dirs and base_bazel_flat):
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
        if is_preexisting_target_attr("filegroup", fg_name):
            continue
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

    # Check whether `# @bazel2gn:path_overwrite:<gl>` on `//<bz_pkg>:<bz_target>` is
    # redundant because `<gl>` in `<bz_pkg>/BUILD.gn` is a pure forwarding group that
    # expands directly to `:<bz_target>` (and `:<bz_target>` is visible to `caller_pkg`).
    def is_redundant_gn_forwarding_group_overwrite(
        caller_pkg: str, bz_pkg: str, bz_target: str, gn_label: str
    ) -> str:
        if gn_label == f"//{bz_pkg}":
            gn_target = os.path.basename(bz_pkg)
        elif gn_label.startswith(f"//{bz_pkg}:"):
            gn_target = gn_label.split(":", 1)[1]
        else:
            return ""
        if not gn_target or gn_target == bz_target:
            return ""
        dep_gn_path = os.path.join(workdir, bz_pkg, "BUILD.gn")
        try:
            with open(dep_gn_path, encoding="utf-8") as df:
                dep_gn_raw = df.read()
        except OSError:
            return ""
        dep_gn_clean = re.sub(r"#[^\n]*", "", dep_gn_raw)
        grp_body = None
        tgt_body = None
        for tm in re.finditer(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{', dep_gn_clean):
            ttype, tname = tm.group(1), tm.group(2)
            depth, idx = 1, tm.end()
            while idx < len(dep_gn_clean) and depth > 0:
                if dep_gn_clean[idx] == "{":
                    depth += 1
                elif dep_gn_clean[idx] == "}":
                    depth -= 1
                idx += 1
            body_slice = dep_gn_clean[tm.end() : idx - 1]
            if tname == gn_target and ttype == "group" and grp_body is None:
                grp_body = body_slice
            elif tname == bz_target and ttype not in ("template", "declare_args", "foreach"):
                tgt_body = body_slice
        if grp_body is None or tgt_body is None:
            return ""
        grp_attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", grp_body))
        if not grp_attrs or not (grp_attrs <= {"public_deps", "deps", "visibility", "testonly"}):
            return ""
        forwarded = set()
        for dm in re.finditer(r"(?<![\w.])(?:public_deps|deps)\s*\+?=\s*\[([^\]]*)\]", grp_body):
            for raw_d in re.findall(r'"([^"]+)"', dm.group(1)):
                d = re.sub(r"\(.*\)$", "", raw_d.strip())
                if d.startswith(":"):
                    forwarded.add(f"//{bz_pkg}:{d[1:]}")
                elif d.startswith("//"):
                    p, _, t = d[2:].partition(":")
                    p = p.strip("/")
                    forwarded.add(f"//{p}:{t or os.path.basename(p)}")
        if forwarded != {f"//{bz_pkg}:{bz_target}"}:
            return ""
        vis_matches = list(re.finditer(r"(?<![\w.])visibility\s*\+?=\s*\[([^\]]*)\]", tgt_body))
        if vis_matches:
            vis_entries = [
                v.strip()
                for vm in vis_matches
                for v in re.findall(r'"([^"]+)"', vm.group(1))
            ]
            vis_ok = False
            for ve in vis_entries:
                if ve in ("*", "//*"):
                    vis_ok = True
                    break
                if ve in (":*", f"//{bz_pkg}:*") and caller_pkg == bz_pkg:
                    vis_ok = True
                    break
                if ve.startswith("//") and ve.endswith(":*"):
                    vp = ve[2:-2].strip("/")
                    if caller_pkg == vp:
                        vis_ok = True
                        break
                if ve.startswith("//") and ve.endswith("/*"):
                    vp = ve[2:-2].strip("/")
                    if not vp or caller_pkg == vp or caller_pkg.startswith(vp + "/"):
                        vis_ok = True
                        break
            if not vis_ok:
                return ""
        return gn_target

    # Check for redundant @bazel2gn:path_overwrite / @bazel2gn:raw_overwrite comments on
    # targets that bazel2gn does not translate or on third-party labels already mapped in
    # build/tools/bazel2gn/third_party_target_map.json.
    untranslated_bazel2gn_rules = {
        "cc_test",
        "exports_files",
        "filegroup",
        "fx_package",
        "fx_packaged_binary",
        "fx_test",
        "fx_test_component",
        "fx_unittest_component",
        "host_cc_test",
        "host_py_test",
        "host_shac_test",
        "host_test",
        "test_suite",
        "wrap_host_rust_test",
    }

    def check_redundant_bazel2gn_overwrites() -> set[int]:
        tp_map: dict[str, str] = {}
        tp_path = os.path.join(
            workdir, "build/tools/bazel2gn/third_party_target_map.json"
        )
        try:
            with open(tp_path, encoding="utf-8") as tf:
                raw_tp = json.load(tf)
            if isinstance(raw_tp, dict):
                tp_map = {str(k).strip(): str(v).strip() for k, v in raw_tp.items()}
        except OSError:
            pass

        untranslated_spans: list[tuple[int, int, str, str, bool]] = []
        visibility_spans: list[tuple[int, int, str, str, bool]] = []
        for stmt in getattr(tree, "body", ()):
            if not (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
            ):
                continue
            rule_name = stmt.value.func.id
            end_line = getattr(stmt, "end_lineno", stmt.lineno)
            t_name = ""
            vis_kw_node = None
            for kw in stmt.value.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif kw.arg == "visibility":
                    vis_kw_node = kw
            is_skipped = False
            idx = stmt.lineno - 2
            while idx >= 0:
                s_line = src_lines[idx].strip()
                if not s_line:
                    idx -= 1
                    continue
                if s_line.startswith("#"):
                    if "@bazel2gn:skip" in s_line:
                        is_skipped = True
                    idx -= 1
                    continue
                break
            if rule_name in untranslated_bazel2gn_rules or is_skipped:
                untranslated_spans.append(
                    (stmt.lineno, end_line, rule_name, t_name, is_skipped)
                )
            elif vis_kw_node is not None:
                vis_end = getattr(vis_kw_node.value, "end_lineno", vis_kw_node.lineno)
                is_empty_vis = (
                    isinstance(vis_kw_node.value, ast.List)
                    and len(vis_kw_node.value.elts) == 0
                )
                visibility_spans.append(
                    (vis_kw_node.lineno, vis_end, rule_name, t_name, is_empty_vis)
                )

        flagged: set[int] = set()
        for lno, ltxt in enumerate(src_lines, 1):
            ov_m = re.search(
                r"#\s*@bazel2gn:(path_overwrite|raw_overwrite):(.+)$", ltxt
            )
            if not ov_m:
                continue
            if base_bazel_flat and re.sub(r"\s+", "", ltxt) in base_bazel_flat:
                continue
            directive = ov_m.group(1)
            gn_spec_val = ov_m.group(2).strip()
            vis_enclosing = (
                next(
                    (sp for sp in visibility_spans if sp[0] <= lno <= sp[1]),
                    None,
                )
                if directive == "raw_overwrite"
                else None
            )
            if vis_enclosing is not None and not vis_enclosing[4]:
                _, _, v_rule, v_tname, _ = vis_enclosing
                vis_pats = [p.strip() for p in re.findall(r'"([^"]+)"', gn_spec_val)]
                if (
                    vis_pats
                    and "*" not in vis_pats
                    and "//*" not in vis_pats
                    and not any(p.startswith("//build/sdk:") for p in vis_pats)
                    and all(
                        p in (":*", "./*")
                        or bool(re.match(r"^//[^:*]+(?::\*|/\*)$", p))
                        for p in vis_pats
                    )
                ):
                    flagged.add(lno)
                    bz_equiv: list[str] = []
                    for vp in vis_pats:
                        if vp == ":*":
                            continue
                        if vp == "./*":
                            bz_equiv.append(":__subpackages__")
                        elif vp.endswith(":*"):
                            bz_equiv.append(vp[:-2] + ":__pkg__")
                        elif vp.endswith("/*"):
                            bz_equiv.append(vp[:-2] + ":__subpackages__")
                    if not bz_equiv:
                        vis_rem = (
                            f"Remove `# @bazel2gn:raw_overwrite:{gn_spec_val}` on line {lno} "
                            f"of '{rel_path}'. If `{v_rule}(name = \"{v_tname}\")` needs "
                            '`visibility = [ ":*" ]` in BUILD.gn, set '
                            '`visibility = ["//visibility:private"]` in BUILD.bazel (which '
                            "`bazel2gn` converts to `[ \":*\" ]`), or omit `visibility` if "
                            f"GN does not need `:*`, then re-run `fx bazel2gn -d {pkg_dir}`."
                        )
                    else:
                        vis_rem = (
                            f"Remove `# @bazel2gn:raw_overwrite:{gn_spec_val}` on line {lno} "
                            f"of '{rel_path}' and set `visibility = {json.dumps(bz_equiv)}` "
                            f"directly on `{v_rule}(name = \"{v_tname}\")` (`bazel2gn` "
                            'automatically adds `":*"` to non-empty visibility lists and '
                            "translates `:__pkg__` / `:__subpackages__` to `:*` / `/*`), "
                            f"then re-run `fx bazel2gn -d {pkg_dir}`."
                        )
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "redundant_bazel2gn_overwrite",
                        "severity": "error",
                        "file": rel_path,
                        "line": lno,
                        "message": (
                            f"Line {lno} of '{rel_path}' uses unnecessary "
                            f"`# @bazel2gn:raw_overwrite:{gn_spec_val}` on `visibility` of "
                            f'`{v_rule}(name = "{v_tname}")`. `bazel2gn` automatically adds '
                            '`":*"` to non-empty visibility lists, maps '
                            '`"//visibility:private"` to `[ ":*" ]`, and converts '
                            "`:__pkg__` / `:__subpackages__` entries directly to GN `:*` / `/*`."
                        ),
                        "remediation": vis_rem,
                    })
                    continue
            po_match = re.match(
                r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', ltxt
            )
            bz_lbl = po_match.group(1).strip() if po_match else ""
            gn_lbls = [
                tok.strip().strip('"')
                for tok in gn_spec_val.split('", "')
                if tok.strip().strip('"')
            ]
            mapped_gn = tp_map.get(bz_lbl, "") if bz_lbl else ""
            is_tp_mapped = (
                directive == "path_overwrite"
                and len(gn_lbls) == 1
                and bool(mapped_gn)
                and gn_lbls[0] == mapped_gn
            )
            enclosing = next(
                (sp for sp in untranslated_spans if sp[0] <= lno <= sp[1]),
                None,
            )
            if enclosing is None and not is_tp_mapped:
                continue

            reasons: list[str] = []
            if enclosing is not None:
                _, _, r_name, t_name, skipped = enclosing
                tgt_desc = (
                    f'`{r_name}(name = "{t_name}")`' if t_name else f"`{r_name}(...)`"
                )
                if skipped:
                    reasons.append(f"{tgt_desc} is marked `# @bazel2gn:skip`")
                else:
                    reasons.append(f"`bazel2gn` does not translate `{r_name}` targets")
            if is_tp_mapped:
                reasons.append(
                    "`build/tools/bazel2gn/third_party_target_map.json` already maps "
                    f'`"{bz_lbl}"` to `"{mapped_gn}"`'
                )

            flagged.add(lno)
            findings.append({
                "source": "bazel_minimality",
                "category": "redundant_bazel2gn_overwrite",
                "severity": "error",
                "file": rel_path,
                "line": lno,
                "message": (
                    f"Line {lno} of '{rel_path}' has a redundant "
                    f"`# @bazel2gn:{directive}:{gn_spec_val}` annotation because "
                    + " and ".join(reasons)
                    + "."
                ),
                "remediation": (
                    f"Remove the `# @bazel2gn:{directive}:{gn_spec_val}` comment on "
                    f"line {lno} of '{rel_path}'."
                ),
            })
        return flagged

    redundant_overwrite_lines = check_redundant_bazel2gn_overwrites()

    for lineno, line_txt in enumerate(src_lines, 1):
        if lineno in redundant_overwrite_lines:
            continue
        po_m = re.match(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', line_txt)
        if not po_m:
            continue
        if base_bazel_flat and re.sub(r"\s+", "", line_txt) in base_bazel_flat:
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
            #    and //<pkg> is not a pure forwarding group to :<bz_target>
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
            if related and (gl == f"//{bz_pkg}" or gl.startswith(f"//{bz_pkg}:")):
                if not (
                    pkg_dir not in target_dirs
                    and base_bazel_flat
                    and re.sub(r"\s+", "", line_txt) in base_bazel_flat
                ):
                    fwd_grp = is_redundant_gn_forwarding_group_overwrite(
                        pkg_dir, bz_pkg, bz_target, gl
                    )
                    if fwd_grp:
                        findings.append({
                            "source": "bazel_minimality",
                            "category": "redundant_forwarding_group_path_overwrite",
                            "severity": "error",
                            "file": rel_path,
                            "line": lineno,
                            "message": (
                                f"Line {lineno} of '{rel_path}' annotates `\"{bz_label}\"` with "
                                f"`# @bazel2gn:path_overwrite:{gl}`, but in '{bz_pkg}/BUILD.gn' "
                                f"`group(\"{fwd_grp}\")` is a pure forwarding group that expands to "
                                f"`//{bz_pkg}:{bz_target}`, so GN can depend on `\"{bz_label}\"` "
                                "directly without this annotation."
                            ),
                            "remediation": (
                                f"Remove `# @bazel2gn:path_overwrite:{gl}` from line {lineno} of "
                                f"'{rel_path}' and re-run `fx bazel2gn -d {pkg_dir}`."
                            ),
                        })
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


# 4a-0. Check copyright header years on newly added vs pre-existing Starlark files,
#       and reject hand-edits to crate_universe-generated BUILD.bazel files.
def check_generated_vendor_build_hygiene():
    """Reject manual copyright header or package default_visibility edits in crate_universe BUILD.bazel files."""
    import datetime

    cur_year = str(datetime.datetime.now(datetime.timezone.utc).year)
    copyright_re = re.compile(r"^#\s*Copyright\s+\d{4}\s+The\s+Fuchsia\s+Authors\b", re.M)
    for p in sorted(changed):
        norm = p.strip("/")
        if not (
            norm.startswith(RUST_CRATES + "vendor/")
            and os.path.basename(norm) in ("BUILD.bazel", "BUILD")
        ):
            continue
        full = os.path.join(workdir, norm)
        if not os.path.isfile(full):
            continue
        try:
            with open(full, "r", encoding="utf-8") as f:
                cur_txt = f.read()
        except OSError:
            continue
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{norm}"]))
        has_manual_copyright = bool(
            copyright_re.search(cur_txt[:512])
            and not copyright_re.search(base_txt[:512])
        )
        dropped_default_vis = (
            "default_visibility" in base_txt and "default_visibility" not in cur_txt
        )
        if has_manual_copyright or dropped_default_vis:
            findings.append({
                "source": "bazel_minimality",
                "category": "hand_edited_generated_vendor_build",
                "severity": "error",
                "file": norm,
                "line": 1,
                "message": (
                    f"Auto-generated crate_universe file '{norm}' was hand-edited to "
                    "add a Fuchsia copyright header or remove package-level "
                    "`default_visibility`."
                ),
                "remediation": (
                    f"Do not hand-edit '{norm}' for copyright headers or visibility. "
                    "Restore the standard crate_universe header and "
                    "`default_visibility = [\"//visibility:public\"]` (or regenerate "
                    "with `fx update-rustc-third-party` from "
                    "`build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl`)."
                ),
            })
        else:
            # Record non-blocking INFO coverage for crate_universe header and
            # default_visibility so diff review does not flag generated files.
            dv_lines = {1}
            for idx, ln in enumerate(cur_txt.splitlines(), 1):
                if "package(" in ln or "default_visibility" in ln:
                    dv_lines.add(idx)
            for dv_cat in ("package_hygiene", "default_visibility"):
                for ln_no in sorted(dv_lines):
                    findings.append({
                        "source": "bazel_minimality",
                        "category": dv_cat,
                        "severity": "INFO",
                        "file": norm,
                        "line": ln_no,
                        "message": (
                            "package() must not specify default_visibility; "
                            "set visibility explicitly per target"
                        ),
                        "remediation": (
                            "Auto-generated crate_universe BUILD.bazel files "
                            "retain their generated package(default_visibility) setting."
                        ),
                    })
            for ch_msg in (
                f"Missing {cur_year} Fuchsia copyright header in BUILD.bazel",
                "BUILD.bazel is missing a Fuchsia copyright header",
                "Missing Fuchsia copyright header in BUILD.bazel",
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "copyright_header",
                    "severity": "INFO",
                    "file": norm,
                    "line": 1,
                    "message": ch_msg,
                    "remediation": (
                        "Auto-generated crate_universe BUILD.bazel files "
                        "use the crate_universe header instead of a Fuchsia copyright header."
                    ),
                })


def check_starlark_copyright_headers(rels):
    import datetime

    cur_year = str(datetime.datetime.now(datetime.timezone.utc).year)
    copyright_re = re.compile(r"^#\s*Copyright\s+(\d{4})\s+The\s+Fuchsia\s+Authors\b", re.M)
    new_file_bad_years = False
    preexisting_prior_years = []
    all_rels = set(rels)
    for p in changed:
        norm = p.strip("/")
        if norm.startswith("third_party/") and not norm.startswith(RUST_CRATES + "compat/"):
            continue
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


# Check for newly exported raw .shard.cml files in exports_files() or multiple raw .shard.cml
# includes from the same package in fx_component_manifest(includes) where cml_shard should be used.
def check_cml_shard_targets(starlark_files):
    exempt_prefixes = ("src/sys/test_runners/", "sdk/lib/sys/testing/")
    for rel in sorted(starlark_files):
        if not os.path.basename(rel).startswith("BUILD"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                st_src = f.read()
            st_tree = ast.parse(st_src, filename=rel)
        except Exception:
            continue
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{rel}"]))
        for node in ast.walk(st_tree):
            if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name):
                continue
            fname = node.func.id
            if fname == "exports_files" and not rel.startswith(exempt_prefixes):
                exported = []
                if node.args and isinstance(node.args[0], ast.List):
                    for elt in node.args[0].elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            if elt.value.endswith(".shard.cml") and f'"{elt.value}"' not in base_txt:
                                exported.append(elt.value)
                if exported:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "prefer_cml_shard_over_exports_files",
                        "severity": "error",
                        "file": rel,
                        "line": node.lineno,
                        "message": (
                            f"'{rel}' exports {', '.join(exported)} via `exports_files(...)` "
                            "instead of defining `cml_shard(...)` target(s)."
                        ),
                        "remediation": (
                            "Load `cml_shard` from `//build/bazel/rules/components:cml_shard.bzl` "
                            "and declare `cml_shard(name = \"<stem>.shard\", shard = "
                            "\"<stem>.shard.cml\", includes = [...])` instead of `exports_files`."
                        ),
                    })
            elif fname == "fx_component_manifest":
                by_pkg = {}
                for kw in node.keywords:
                    if kw.arg == "includes" and isinstance(kw.value, ast.List):
                        for elt in kw.value.elts:
                            if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                                m_sh = re.match(r"^//([^:]+):([^:]+\.shard\.cml)$", elt.value)
                                if m_sh and not m_sh.group(1).startswith(exempt_prefixes):
                                    by_pkg.setdefault(m_sh.group(1), []).append(elt.value)
                for spkg, slabels in sorted(by_pkg.items()):
                    if len(slabels) >= 2:
                        findings.append({
                            "source": "bazel_minimality",
                            "category": "unrolled_transitive_cml_shard_includes",
                            "severity": "error",
                            "file": rel,
                            "line": node.lineno,
                            "message": (
                                f"`fx_component_manifest` in '{rel}' lists multiple raw "
                                f"`.shard.cml` files from `//{spkg}` ({', '.join(slabels)}) "
                                "instead of a `cml_shard` target with `includes`."
                            ),
                            "remediation": (
                                f"Define `cml_shard` target(s) in `{spkg}/BUILD.bazel` bundling "
                                "its transitive shard `includes` and reference the single "
                                "`cml_shard` label in `fx_component_manifest(includes = [...])`."
                            ),
                        })


check_generated_vendor_build_hygiene()
check_starlark_copyright_headers(starlark_rels)
check_cml_shard_targets(starlark_rels)


# Check compiled_action() target hygiene in BUILD.bazel files and rule/converter invariants
# when build/bazel/rules/compiled_action.bzl or build/tools/bazel2gn/* is modified.
def check_compiled_action_hygiene(starlark_files):
    loc_re = re.compile(r"\$\((?:location|execpath)\s+([^()\s]+)\s*\)")

    def resolve_tool_gn_output_name(cur_pkg, tool_lbl):
        if tool_lbl.startswith("@"):
            return "", ""
        if tool_lbl.startswith("//"):
            pkg_part, has_c, t_part = tool_lbl[2:].partition(":")
            t_pkg = pkg_part.strip("/")
            t_name = t_part if has_c else os.path.basename(t_pkg)
        else:
            t_pkg = cur_pkg
            t_name = tool_lbl.lstrip(":")
        if not t_name:
            return "", ""
        cand_bz = os.path.join(workdir, t_pkg, "BUILD.bazel") if t_pkg else ""
        if not cand_bz or not os.path.isfile(cand_bz):
            return t_name, t_name
        try:
            with open(cand_bz, "r", encoding="utf-8") as f:
                bz_src = f.read()
            bz_tree = ast.parse(bz_src, filename=cand_bz)
        except Exception:
            return t_name, t_name
        for node in ast.walk(bz_tree):
            if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)):
                continue
            r_id = node.func.id
            if r_id not in (
                "rustc_binary",
                "rust_binary",
                "fx_cc_binary",
                "cc_binary",
                "go_binary",
                "py_binary",
            ):
                continue
            k_strs = {
                kw.arg: kw.value.value
                for kw in node.keywords
                if kw.arg
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            }
            if k_strs.get("name") != t_name:
                continue
            if r_id in ("rustc_binary", "rust_binary"):
                gn_out = (
                    k_strs.get("binary_name")
                    or k_strs.get("crate_name")
                    or t_name.replace("-", "_")
                )
            else:
                gn_out = k_strs.get("output_name") or k_strs.get("binary_name") or t_name
            return t_name, gn_out
        return t_name, t_name

    # 1. Check compiled_action() target invocations in BUILD.bazel files.
    for rel in sorted(starlark_files):
        if not os.path.basename(rel).startswith("BUILD"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                st_src = f.read()
            st_tree = ast.parse(st_src, filename=rel)
        except Exception:
            continue
        base_flat = re.sub(
            r"\s+", "", "\n".join(git_lines(["show", f"{change_base}:{rel}"]))
        )
        st_lines = st_src.splitlines()
        cur_pkg = os.path.dirname(rel).strip("/")
        for node in ast.walk(st_tree):
            if not (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Name)
                and node.func.id == "compiled_action"
            ):
                continue
            seg = ast.get_source_segment(st_src, node) or ""
            if seg and base_flat and re.sub(r"\s+", "", seg) in base_flat:
                continue
            ca_name = ""
            depfile_val = ""
            tool_val = ""
            tool_out_val = ""
            tool_out_line = getattr(node, "lineno", 1)
            unannotated_deps = set()
            all_deps = set()
            args_nodes = []
            for kw in node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant):
                    ca_name = str(kw.value.value)
                elif kw.arg == "depfile" and isinstance(kw.value, ast.Constant):
                    depfile_val = str(kw.value.value)
                elif kw.arg == "tool" and isinstance(kw.value, ast.Constant):
                    tool_val = str(kw.value.value)
                elif kw.arg == "tool_output_name" and isinstance(kw.value, ast.Constant):
                    tool_out_val = str(kw.value.value)
                    tool_out_line = kw.lineno
                elif kw.arg == "deps":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            dlbl = sub.value
                            dline = (
                                st_lines[sub.lineno - 1]
                                if 1 <= getattr(sub, "lineno", 0) <= len(st_lines)
                                else ""
                            )
                            forms = {dlbl}
                            if not dlbl.startswith(("//", "@")):
                                bare = dlbl.lstrip(":")
                                forms.update({bare, ":" + bare})
                            all_deps.update(forms)
                            if "@bazel2gn:path_overwrite:" not in dline:
                                unannotated_deps.update(forms)
                elif kw.arg == "args":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            args_nodes.append(sub)
            depfile_forms = (
                {depfile_val, depfile_val.lstrip(":"), ":" + depfile_val.lstrip(":")}
                if depfile_val
                else set()
            )
            for a_node in args_nodes:
                aline = (
                    st_lines[a_node.lineno - 1]
                    if 1 <= getattr(a_node, "lineno", 0) <= len(st_lines)
                    else ""
                )
                if "@bazel2gn:raw_overwrite:" in aline:
                    continue
                for m in loc_re.finditer(a_node.value):
                    ref_lbl = m.group(1)
                    if depfile_forms and ref_lbl in depfile_forms:
                        clean_df = depfile_val.lstrip(":")
                        findings.append({
                            "source": "bazel_minimality",
                            "category": "compiled_action_depfile_location_substitution",
                            "severity": "error",
                            "file": rel,
                            "line": getattr(a_node, "lineno", node.lineno),
                            "message": (
                                f"Target '{ca_name}' (compiled_action) references depfile "
                                f"'{depfile_val}' via `{m.group(0)}` in `args`. `depfile` is "
                                "a string attribute unused by Bazel and forwarded to GN by "
                                "bazel2gn, so `$(location ...)` / `$(execpath ...)` cannot "
                                "expand it in Bazel."
                            ),
                            "remediation": (
                                f"Replace `{m.group(0)}` with `$(RULEDIR)/{clean_df}` in "
                                f"`args` of `{ca_name}` in '{rel}' (`bazel2gn` translates "
                                "`$(RULEDIR)` to `rebase_path(target_gen_dir, root_build_dir)`)."
                            ),
                        })
                    elif ref_lbl in all_deps:
                        findings.append({
                            "source": "bazel_minimality",
                            "category": "compiled_action_deps_location_substitution",
                            "severity": "error",
                            "file": rel,
                            "line": getattr(a_node, "lineno", node.lineno),
                            "message": (
                                f"Target '{ca_name}' (compiled_action) references `deps` "
                                f"entry '{ref_lbl}' via `{m.group(0)}` in `args`. "
                                "`compiled_action` only expands `$(location ...)` for `srcs` "
                                "and `outs`, and `bazel2gn` rejects un-annotated `deps` "
                                "labels in `args`."
                            ),
                            "remediation": (
                                f"Move '{ref_lbl}' from `deps` to `srcs` in `{ca_name}` in "
                                f"'{rel}' (adding `# @bazel2gn:path_overwrite:<gn_path>` if "
                                "its GN path differs), or avoid `$(location ...)` on `deps`."
                            ),
                        })
            if tool_val:
                t_name, gn_out = resolve_tool_gn_output_name(cur_pkg, tool_val)
                if t_name and gn_out and gn_out != t_name and not tool_out_val:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "compiled_action_missing_tool_output_name",
                        "severity": "error",
                        "file": rel,
                        "line": getattr(node, "lineno", 1),
                        "message": (
                            f"Target '{ca_name}' (compiled_action) uses `tool = "
                            f"\"{tool_val}\"` whose GN output filename is `{gn_out}` "
                            f"(different from target name `{t_name}`), but omits "
                            f"`tool_output_name = \"{gn_out}\"` (`bazel2gn` does not "
                            "auto-infer `tool_output_name`)."
                        ),
                        "remediation": (
                            f"Add `tool_output_name = \"{gn_out}\"` to `{ca_name}` in "
                            f"'{rel}' and re-run `fx bazel2gn -d {cur_pkg}`."
                        ),
                    })
                elif t_name and tool_out_val and tool_out_val == t_name:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "redundant_default_attribute",
                        "severity": "error",
                        "file": rel,
                        "line": tool_out_line,
                        "message": (
                            f"Target '{ca_name}' (compiled_action) sets "
                            f"`tool_output_name = \"{tool_out_val}\"` matching the target "
                            f"name of `tool = \"{tool_val}\"`, which is already the default."
                        ),
                        "remediation": (
                            f"Remove `tool_output_name = \"{tool_out_val}\"` from "
                            f"`{ca_name}` in '{rel}'."
                        ),
                    })

    # 2. Check build/bazel/rules/compiled_action.bzl when modified in the change.
    ca_bzl = "build/bazel/rules/compiled_action.bzl"
    if ca_bzl in changed and os.path.isfile(os.path.join(workdir, ca_bzl)):
        bzl_txt = open(os.path.join(workdir, ca_bzl), encoding="utf-8").read()
        if (
            "ctx.attr.deps" in bzl_txt.split("ctx.actions.run")[0]
            or re.search(r"`srcs`,\s*`deps`", bzl_txt)
            or re.search(
                r'"deps"\s*:\s*attr\.label_list\([^)]*\$\(location', bzl_txt, re.S
            )
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "compiled_action_bzl_deps_in_expand_location",
                "severity": "error",
                "file": ca_bzl,
                "line": 54,
                "message": (
                    f"'{ca_bzl}' includes `deps` in `ctx.expand_location` targets or "
                    "documents `deps` as supported in `$(location ...)`. `deps` are plain "
                    "action inputs and `bazel2gn` cannot translate an un-annotated `deps` "
                    "label in `args` to its GN output path."
                ),
                "remediation": (
                    f"In '{ca_bzl}', pass `targets = ctx.attr.srcs` directly to "
                    "`ctx.expand_location` (removing `location_targets = ctx.attr.srcs + "
                    "ctx.attr.deps`), and remove `deps` from the `$(location ...)` "
                    "documentation at the top of the file and in the `deps` docstring."
                ),
            })
        if (
            re.search(r'"depfile"\s*:\s*attr\.output\b', bzl_txt)
            or "ctx.outputs.depfile" in bzl_txt
            or re.search(r"`outs`,\s*or\s*`depfile`", bzl_txt)
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "compiled_action_bzl_depfile_output",
                "severity": "error",
                "file": ca_bzl,
                "line": 126,
                "message": (
                    f"'{ca_bzl}' declares `depfile` as `attr.output` / action output or "
                    "lists `depfile` under `$(location ...)`. Fuchsia's Bazel build does "
                    "not consume depfiles, so declaring `depfile` as a Bazel output "
                    "requires tools to write a file Bazel ignores."
                ),
                "remediation": (
                    f"In '{ca_bzl}', change `depfile` to `attr.string` documented as unused "
                    "by Bazel and forwarded to GN `compiled_action()` by `bazel2gn`, remove "
                    "`ctx.outputs.depfile` from `_compiled_action_impl` so `outputs = "
                    "ctx.outputs.outs`, and drop `depfile` from the `$(location ...)` list "
                    "in the module docstring (pass `$(RULEDIR)/<depfile>` in `args` instead)."
                ),
            })

    # 3. Check build/tools/bazel2gn/ compiled_action converter invariants when modified.
    ca_go = "build/tools/bazel2gn/compiled_action.go"
    if ca_go in changed and os.path.isfile(os.path.join(workdir, ca_go)):
        go_txt = open(os.path.join(workdir, ca_go), encoding="utf-8").read()
        b2gn_go_path = os.path.join(workdir, "build/tools/bazel2gn/bazel2gn.go")
        b2gn_txt = (
            open(b2gn_go_path, encoding="utf-8").read()
            if os.path.isfile(b2gn_go_path)
            else ""
        )
        readme_path = os.path.join(workdir, "build/tools/bazel2gn/README.md")
        readme_txt = (
            open(readme_path, encoding="utf-8").read()
            if os.path.isfile(readme_path)
            else ""
        )
        agents_path = os.path.join(workdir, "build/tools/bazel2gn/AGENTS.md")
        agents_txt = (
            open(agents_path, encoding="utf-8").read()
            if os.path.isfile(agents_path)
            else ""
        )
        if (
            "resolveCompiledActionTargets" in go_txt
            or "hasRawToolOutputNameAttr" in go_txt
            or "rawToolOutputNameRE" in go_txt
            or "resolveCompiledActionTargets" in b2gn_txt
            or re.search(
                r"infer\w*[^.\n]*tool_output_name|tool_output_name[^.\n]*infer\w*",
                readme_txt + "\n" + agents_txt,
                re.I,
            )
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "bazel2gn_compiled_action_tool_output_name_inference",
                "severity": "error",
                "file": ca_go,
                "line": 97,
                "message": (
                    f"'{ca_go}' (or companion bazel2gn files) implements or documents "
                    "same-file `tool_output_name` auto-inference "
                    "(`resolveCompiledActionTargets`), adding a whole-file rewrite pass "
                    "when `tool_output_name` should be specified explicitly."
                ),
                "remediation": (
                    "Remove `hasRawToolOutputNameAttr`, `rawToolOutputNameRE`, and "
                    f"`resolveCompiledActionTargets` from '{ca_go}', remove the "
                    "`resolveCompiledActionTargets` call from "
                    "'build/tools/bazel2gn/bazel2gn.go', remove "
                    "`TestCompiledActionFileToGN` from "
                    "'build/tools/bazel2gn/compiled_action_test.go', and remove the "
                    "`tool_output_name` inference sentences from "
                    "'build/tools/bazel2gn/README.md' and 'build/tools/bazel2gn/AGENTS.md'."
                ),
            })
        if (
            not re.search(r'case\s+"deps"|case\s+"srcs",\s*"deps"', go_txt)
            or "path_overwrite" not in go_txt
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "bazel2gn_compiled_action_unannotated_deps_location",
                "severity": "error",
                "file": ca_go,
                "line": 392,
                "message": (
                    f"'{ca_go}' does not inspect `deps` in `compiledActionPaths` to reject "
                    "`$(location <label>)` / `$(execpath <label>)` on un-annotated `deps` "
                    "entries in `compiledActionArgToGN`."
                ),
                "remediation": (
                    f"In `compiledActionPaths` in '{ca_go}', collect `deps` labels (both "
                    "`:x` and `x` forms) that lack `@bazel2gn:path_overwrite`, map "
                    "path-overwritten `deps` entries the same way as path-overwritten "
                    "`srcs`, and in `compiledActionArgToGN` return an error when an arg's "
                    "label refers to an un-annotated `deps` entry. Update doc comments in "
                    "`compiled_action.go`, `compiled_action.bzl`, and `README.md` to match."
                ),
            })


check_compiled_action_hygiene(starlark_rels)


# Check Fuchsia Rust wrapper macro (.bzl) and bazel2gn converter invariants when modified.
def check_rust_wrapper_macro_hygiene():
    # Audit changed rustc_*.bzl wrapper macros for custom wrapper rules, deprecated
    # with_*_unit_tests attributes, and unprefixed bazel2gn passthrough attributes.
    def _check_rustc_macro_bzl(rel, bzl_txt):
        if (
            "_rustc_staticlib_cc_wrapper" in bzl_txt
            or "create_crate_info" in bzl_txt
            or re.search(r'"(?:hdrs|non_test_deps)"\s*:\s*attr\.', bzl_txt)
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "rustc_macro_custom_wrapper_rule",
                "severity": "error",
                "file": rel,
                "line": 28,
                "message": (
                    f"'{rel}' defines a custom `rule()` wrapper (`_rustc_staticlib_cc_wrapper`) "
                    "or exposes `hdrs` / `non_test_deps` / archive-renaming `output_name`. "
                    "`rustc_staticlib()` must be a thin `rust_static_library` macro like "
                    "`rustc_library()`: GN never writes `lib<output_name>.a` (`output_name` is "
                    "a GN-only attribute ignored in Bazel), C/C++ headers use "
                    "`cc_library(hdrs = [...], deps = [\":<staticlib>\"])`, and tests that "
                    "exclude a library dependency use a standalone `rustc_test()` with own "
                    "`srcs`/`crate_root`/`deps`."
                ),
                "remediation": (
                    f"In '{rel}', drop `_rustc_staticlib_cc_wrapper_impl` and "
                    "`_rustc_staticlib_cc_wrapper` (and unused `@bazel_tools//tools/cpp:...`, "
                    "`@rules_cc//...`, and `rust_common` loads), remove `hdrs` and "
                    "`non_test_deps`, accept `output_name` as a `bazel2gn`-only attribute "
                    "ignored in Bazel (`output_name,  # buildifier: disable=unused-variable`), "
                    "and call `rust_static_library(name = name, ...)` directly."
                ),
            })
        if (
            "generate_unit_tests" in bzl_txt
            or re.search(r'"(?:with_unit_tests|with_host_unit_tests|test_deps)"\s*:', bzl_txt)
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "rustc_macro_deprecated_with_unit_tests",
                "severity": "error",
                "file": rel,
                "line": 180,
                "message": (
                    f"'{rel}' supports deprecated `with_unit_tests` / `with_host_unit_tests` "
                    "(`test_deps`, `generate_unit_tests`). New `rustc_*` macros must not "
                    "support `with_*_unit_tests`; unit tests must use standalone `rustc_test()` "
                    "or `host_rustc_test()` targets."
                ),
                "remediation": (
                    f"In '{rel}', remove the `generate_unit_tests` load and call, "
                    "`test_lint_config`, and the `with_host_unit_tests`, `with_unit_tests`, "
                    "`test_deps`, and `non_test_deps` parameters/attributes and docstrings."
                ),
            })
        if re.search(r'"(?:quiet_clippy|disable_rustdoc)"\s*:\s*attr\.', bzl_txt):
            findings.append({
                "source": "bazel_minimality",
                "category": "rustc_macro_unprefixed_gn_passthrough_attrs",
                "severity": "error",
                "file": rel,
                "line": 190,
                "message": (
                    f"'{rel}' declares unprefixed `quiet_clippy` / `disable_rustdoc` "
                    "attributes. Attributes on `rustc_*` macros that exist only for `bazel2gn` "
                    "passthrough to GN must be prefixed with `gn_` (`gn_quiet_clippy` and "
                    "`gn_disable_rustdoc`)."
                ),
                "remediation": (
                    f"In '{rel}', rename `quiet_clippy` and `disable_rustdoc` to "
                    "`gn_quiet_clippy` and `gn_disable_rustdoc` (both ignored in Bazel with "
                    "`# buildifier: disable=unused-variable`), and map "
                    "`\"gn_disable_rustdoc\": \"disable_rustdoc\"` and "
                    "`\"gn_quiet_clippy\": \"quiet_clippy\"` in `rustCommonAttrMap` in "
                    "'build/tools/bazel2gn/constants.go'."
                ),
            })

    # Audit companion clippy_test.bzl and rustc_staticlib.gni files when modified.
    def _check_rustc_macro_tests_and_gni():
        ct_bzl = "build/bazel/rules/rust/tests/clippy_test.bzl"
        if ct_bzl in changed and os.path.isfile(os.path.join(workdir, ct_bzl)):
            ct_txt = open(os.path.join(workdir, ct_bzl), encoding="utf-8").read()
            if (
                re.search(r"rustc_staticlib\([^)]*with_unit_tests", ct_txt, re.S)
                or "staticlib_default_test" in ct_txt
                or "staticlib_disabled_test" in ct_txt
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "rustc_macro_deprecated_with_unit_tests",
                    "severity": "error",
                    "file": ct_bzl,
                    "line": 139,
                    "message": (
                        f"'{ct_bzl}' passes `with_unit_tests = True` to `rustc_staticlib` or "
                        "references `staticlib_*_test` targets, which `rustc_staticlib` must "
                        "not generate."
                    ),
                    "remediation": (
                        f"In '{ct_bzl}', remove `with_unit_tests = True` from "
                        "`staticlib_default` and `staticlib_disabled` and remove the "
                        "`staticlib_default_unit_test` and `staticlib_disabled_unit_test` "
                        "entries from `clippy_test_suite`."
                    ),
                })

        gni_rel = "build/rust/rustc_staticlib.gni"
        if gni_rel in changed and os.path.isfile(os.path.join(workdir, gni_rel)):
            gni_txt = open(os.path.join(workdir, gni_rel), encoding="utf-8").read()
            if 'assert(_with_unit_tests, "use deps if not generating unit tests")' not in gni_txt:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "gn_staticlib_non_test_deps_assert_removed",
                    "severity": "error",
                    "file": gni_rel,
                    "line": 298,
                    "message": (
                        f"'{gni_rel}' removes `assert(_with_unit_tests, \"use deps if not "
                        "generating unit tests\")` on `non_test_deps`. In Bazel, a standalone "
                        "`rustc_test()` with its own `srcs`/`crate_root`/`deps` excludes "
                        "non-test dependencies without filtering, so the GN side uses plain "
                        "`deps` when `with_unit_tests` is not set."
                    ),
                    "remediation": (
                        f"In '{gni_rel}', restore `assert(_with_unit_tests, \"use deps if not "
                        "generating unit tests\")` and the original `non_test_deps` doc comment."
                    ),
                })

    # Audit bazel2gn converter and tests for rustc_staticlib attribute hygiene.
    def _check_bazel2gn_rust_staticlib_hygiene():
        if not any(p.strip("/").startswith("build/tools/bazel2gn/") for p in changed):
            return
        const_go = "build/tools/bazel2gn/constants.go"
        if const_go in changed and os.path.isfile(os.path.join(workdir, const_go)):
            c_txt = open(os.path.join(workdir, const_go), encoding="utf-8").read()
            if "rustc_staticlib" in c_txt and (
                '"gn_quiet_clippy"' not in c_txt or '"gn_disable_rustdoc"' not in c_txt
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "bazel2gn_missing_gn_prefixed_rust_attrs",
                    "severity": "error",
                    "file": const_go,
                    "line": 328,
                    "message": (
                        f"'{const_go}' does not map `gn_quiet_clippy` to `quiet_clippy` and "
                        "`gn_disable_rustdoc` to `disable_rustdoc` in `rustCommonAttrMap`."
                    ),
                    "remediation": (
                        f"In `rustCommonAttrMap` in '{const_go}', add "
                        "`\"gn_disable_rustdoc\": \"disable_rustdoc\"` and "
                        "`\"gn_quiet_clippy\": \"quiet_clippy\"`."
                    ),
                })

        b2gn_go = "build/tools/bazel2gn/bazel2gn.go"
        if b2gn_go in changed and os.path.isfile(os.path.join(workdir, b2gn_go)):
            b_txt = open(os.path.join(workdir, b2gn_go), encoding="utf-8").read()
            if '"non_test_deps"' in b_txt or re.search(
                r'case\s+"api",\s*"outputs",\s*"public"', b_txt
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "bazel2gn_unneeded_staticlib_attr_transformers",
                    "severity": "error",
                    "file": b2gn_go,
                    "line": 831,
                    "message": (
                        f"'{b2gn_go}' adds `\"non_test_deps\"` or `\"public\"` to "
                        "`attrAssignmentToGN`, which are not used by `rustc_staticlib` "
                        "(and adding `\"public\"` alters `hdrs` path-overwrite behavior)."
                    ),
                    "remediation": (
                        f"In '{b2gn_go}', remove `\"non_test_deps\"` and `\"public\"` from "
                        "`attrAssignmentToGN` (keeping `\"applicable_licenses\"`), and revert "
                        "the change to `TestPathOverwriteAnnotations` in "
                        "'build/tools/bazel2gn/bazel2gn_test.go'."
                    ),
                })

        b2gn_test = "build/tools/bazel2gn/bazel2gn_test.go"
        if b2gn_test in changed and os.path.isfile(os.path.join(workdir, b2gn_test)):
            bt_txt = open(os.path.join(workdir, b2gn_test), encoding="utf-8").read()
            m_st = re.search(
                r'name:\s*"rustc_staticlib".*?wantGN:\s*`.*?`\s*,', bt_txt, re.S
            )
            st_blk = m_st.group(0) if m_st else ""
            if (
                any(
                    k in st_blk
                    for k in (
                        "hdrs =",
                        "non_test_deps =",
                        "test_deps =",
                        "with_unit_tests =",
                        "with_host_unit_tests =",
                    )
                )
                or re.search(r"(?<![_\w])(?:quiet_clippy|disable_rustdoc)\s*=\s*True", st_blk)
                or "should_be_ignored.h" not in bt_txt
            ):
                findings.append({
                    "source": "bazel_minimality",
                    "category": "bazel2gn_test_deprecated_staticlib_attrs",
                    "severity": "error",
                    "file": b2gn_test,
                    "line": 392,
                    "message": (
                        f"'{b2gn_test}' tests removed attributes (`hdrs`, `non_test_deps`, "
                        "`test_deps`, `with_unit_tests`, `with_host_unit_tests`) or unprefixed "
                        "`quiet_clippy` / `disable_rustdoc` on `rustc_staticlib` / "
                        "`rust_static_library`, or modifies `TestPathOverwriteAnnotations`."
                    ),
                    "remediation": (
                        f"In '{b2gn_test}', update the `rustc_staticlib` test case to use "
                        "`gn_disable_rustdoc = True` and `gn_quiet_clippy = True`, remove "
                        "`hdrs`, `non_test_deps`, `test_deps`, `with_unit_tests`, and "
                        "`with_host_unit_tests` (and their `wantGN` lines), and restore "
                        "`should_be_ignored.h` in `TestPathOverwriteAnnotations`."
                    ),
                })

        rtc_test = "build/tools/bazel2gn/rust_test_crate_test.go"
        if rtc_test in changed and os.path.isfile(os.path.join(workdir, rtc_test)):
            rt_txt = open(os.path.join(workdir, rtc_test), encoding="utf-8").read()
            if "non_test_deps" in rt_txt:
                findings.append({
                    "source": "bazel_minimality",
                    "category": "bazel2gn_test_deprecated_staticlib_attrs",
                    "severity": "error",
                    "file": rtc_test,
                    "line": 277,
                    "message": (
                        f"'{rtc_test}' passes `non_test_deps` on `rustc_staticlib` in "
                        "`TestRustTestStaticlibCrate` (`rustc_staticlib` does not have "
                        "`non_test_deps`)."
                    ),
                    "remediation": (
                        f"Remove `non_test_deps` from `bazel` and `wantGN` in "
                        f"`TestRustTestStaticlibCrate` in '{rtc_test}'."
                    ),
                })

        for tag_f in ("build/tools/bazel2gn/tags.go", "build/tools/bazel2gn/tags_test.go"):
            if tag_f in changed and os.path.isfile(os.path.join(workdir, tag_f)):
                t_txt = open(os.path.join(workdir, tag_f), encoding="utf-8").read()
                if "rustc_staticlib" in t_txt:
                    findings.append({
                        "source": "bazel_minimality",
                        "category": "bazel2gn_tags_staticlib_with_unit_tests",
                        "severity": "error",
                        "file": tag_f,
                        "line": 34,
                        "message": (
                            f"'{tag_f}' adds `rustc_staticlib` `with_unit_tests` handling for "
                            "`exclude_toolchain_tags`, but `rustc_staticlib` does not support "
                            "`with_unit_tests`."
                        ),
                        "remediation": (
                            f"Revert '{tag_f}' (`git checkout {change_base} -- {tag_f}`)."
                        ),
                    })

    st_bzl = "build/bazel/rules/rust/rustc_staticlib.bzl"
    if st_bzl in changed and os.path.isfile(os.path.join(workdir, st_bzl)):
        _check_rustc_macro_bzl(
            st_bzl, open(os.path.join(workdir, st_bzl), encoding="utf-8").read()
        )
    _check_rustc_macro_tests_and_gni()
    _check_bazel2gn_rust_staticlib_hygiene()

    # Verify the commit message body does not claim removed wrapper features.
    if st_bzl in changed:
        msg_body = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
        if any(
            w in msg_body
            for w in ("LinkerInput", "TestCrateInfo", "non_test_deps", "hdrs (via CcInfo)")
        ):
            findings.append({
                "source": "bazel_minimality",
                "category": "commit_body_stale_staticlib_wrapper_claims",
                "severity": "error",
                "file": "build/bazel/rules/rust/rustc_staticlib.bzl",
                "line": 1,
                "message": (
                    "Commit message body still describes removed `_rustc_staticlib_cc_wrapper` "
                    "features (`hdrs`, `LinkerInput` re-owning, `non_test_deps` / "
                    "`TestCrateInfo` filtering)."
                ),
                "remediation": (
                    "Amend the commit message body so it describes `rustc_staticlib` as a "
                    "thin `rust_static_library` macro supporting `output_name`, "
                    "`dynamic_libstd`, `gn_quiet_clippy`, `gn_disable_rustdoc`, "
                    "`applicable_licenses`, and `validations`."
                ),
            })


check_rust_wrapper_macro_hygiene()

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

# Detect .bzl files shared with GN: a .gni/.gn file `import()`s them, so they must stay valid
# GN syntax, where a triple-quoted Starlark docstring breaks `gn gen`.
_gn_imported_bzl_cache = {}


def bzl_imported_by_gn(rel):
    if not rel.endswith(".bzl"):
        return False
    if rel in _gn_imported_bzl_cache:
        return _gn_imported_bzl_cache[rel]
    base = os.path.basename(rel)
    found = False
    try:
        proc = subprocess.run(
            ["git", "grep", "-l", "-E", r"import\(\"[^\"]*" + re.escape(base) + r"\"\)",
             "--", "*.gni", "*.gn", "*/BUILD.gn"],
            cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            timeout=60,
        )
        for gn_rel in proc.stdout.split():
            try:
                with open(os.path.join(workdir, gn_rel), "r", encoding="utf-8") as f:
                    gn_txt = f.read()
            except Exception:
                continue
            for m in re.finditer(r'import\("([^"]+)"\)', gn_txt):
                p = m.group(1)
                resolved = p[2:] if p.startswith("//") else os.path.normpath(
                    os.path.join(os.path.dirname(gn_rel), p))
                if resolved == rel:
                    found = True
                    break
            if found:
                break
    except Exception:
        found = False
    _gn_imported_bzl_cache[rel] = found
    return found


def head_txt_full(rel):
    try:
        with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
            return f.read(4096)
    except Exception:
        return ""


GN_SHARED_DOCSTRING_REMEDIATION = (
    "'{f}' is import()ed by GN ({{.gni,.gn}} files), so a triple-quoted docstring would break\n"
    "`gn gen`. Instead add `# buildifier: disable=module-docstring` on its own line right after\n"
    "the copyright header, with a comment saying the file is shared with GN."
)

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
                    # A GN-shared .bzl may suppress module-docstring with a comment (the only
                    # GN-compatible form), which buildifier may still report: accept it.
                    if (
                        w_cat == "module-docstring"
                        and re.search(r"#\s*buildifier:\s*disable=module-docstring", head_txt_full(fname))
                        and bzl_imported_by_gn(fname)
                    ):
                        continue
                    w_line = w.get("start", {}).get("line", 1) or 1
                    w_msg = (w.get("message") or "").strip()
                    if w_cat == "module-docstring" and bzl_imported_by_gn(fname):
                        rem = GN_SHARED_DOCSTRING_REMEDIATION.format(f=fname)
                    elif w_cat == "module-docstring":
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
        if re.search(r"#\s*buildifier:\s*disable=module-docstring", src) and bzl_imported_by_gn(rel):
            continue
        if tree.body and ast.get_docstring(tree, clean=False) is None:
            if bzl_imported_by_gn(rel):
                fallback_rem = GN_SHARED_DOCSTRING_REMEDIATION.format(f=rel)
            else:
                fallback_rem = (
                    f"Add a top-level module docstring (`\"\"\"...\"\"\"` string literal, not a `#` "
                    f"comment) as the first statement of '{rel}' immediately after the copyright "
                    "header comments (before any `load()` or assignments)."
                )
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
                "remediation": fallback_rem,
            })

print(json.dumps(findings, indent=2))
PYEOF
