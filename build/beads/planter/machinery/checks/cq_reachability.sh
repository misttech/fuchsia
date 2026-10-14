#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check verifying that migrated targets and tests in BUILD.bazel /
# BUILD.gn remain reachable in the build & CQ graph:
# 1. Every test target (`*_test`, `fuchsia_unittest_package`, `fuchsia_test_package`)
#    in the migrated package must be wired into the package's `group("tests")`
#    (or referenced by a test package in the same BUILD file that is wired into
#    `tests`).
# 2. The package must be registered in a parent BUILD file so CQ builds it.
# 3. Every Bazel test that Fuchsia test runners can run (`fx_test`, `host_rustc_test`,
#    `host_go_test`, `host_py_test`, `host_test`, `wrap_host_rust_test`, and the
#    `<name>_test` host test of `rustc_*(with_unit_tests = "host"|"both")` or
#    `with_host_unit_tests = True`) must be listed,
#    directly or through a Bazel `test_suite`, in a GN `bazel_test_suite` (`fx_test` in
#    `target_tests`, host tests in `host_tests`) that the GN `group("tests")` of its
#    directory (or the parent BUILD.gn) references. Otherwise it is missing from
#    tests.json and neither `fx test` nor infra runs it. Tests move to Bazel and bazel2gn
#    does not translate them, so no Bazel test is reached through a GN twin.
# 4. Every Rust device test binary (`rustc_test`, or the `<name>_test` of
#    `rustc_*(with_unit_tests = "fuchsia"|"both")`) must be packaged by an
#    `fx_packaged_binary` in the same BUILD.bazel (for an `fx_test`), otherwise nothing runs
#    it (`unpackaged_rust_device_test`).
#    Only runs when the checkout has `build/bazel/rules/testing/fx_test.bzl` (older
#    checkouts keep tests in GN).
# 5. The change must merge cleanly onto `origin/main` without rebase conflicts
#    (`git merge-tree --write-tree`) or leftover conflict markers so Gerrit CQ
#    (`checkout|jiri patch`) does not fail with `Failed to rebase`.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
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
raw_dirs = sys.argv[2]
change_base = sys.argv[3].strip() or "HEAD"
target_dirs = [d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")]

TEST_RULES = {
    "rustc_test",
    "cc_test",
    "go_test",
    "python_host_test",
    "fuchsia_unittest_package",
    "fuchsia_test_package",
}

FETCH_TTL_SECS = 600

findings = []

DEVICE_TEST_RULES = {"fx_test"}
HOST_TEST_RULES = {"host_rustc_test", "host_go_test", "host_py_test", "host_test", "wrap_host_rust_test"}
# Tests move to Bazel and bazel2gn does not translate them (migration_sanity enforces the
# `# @bazel2gn:skip`), so no Bazel host test has a GN twin that runs it instead.
GN_CONVERTED_HOST_TEST_RULES = set()
RUST_TEST_OWNERS = {"rustc_library", "rustc_binary", "rustc_proc_macro"}
# Checkouts without fx_test keep tests in GN (the old path): skip the export check there.
FX_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl"))
SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)
SUITE_RE = re.compile(r'\bbazel_test_suite\(\s*"([^"]+)"\s*\)\s*\{')
SUITE_LIST_RE = re.compile(r"\b(host_tests|target_tests)\s*\+?=\s*\[([^\]]*)\]")


def bazel_strings(node):
    if isinstance(node, (ast.List, ast.Tuple)):
        return [e.value for e in node.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)]
    return []


def is_true(node):
    return isinstance(node, ast.Constant) and node.value is True


def unit_test_envs(kws):
    """Environments (subset of {"fuchsia", "host"}) of the unit tests of a rustc_* target."""
    if is_true(kws.get("with_host_unit_tests")):
        return {"host"}
    v = kws.get("with_unit_tests")
    if isinstance(v, ast.Constant):
        return {"fuchsia": {"fuchsia"}, "host": {"host"}, "both": {"fuchsia", "host"}}.get(v.value, set())
    return set()


def split_label(label, pkg):
    """(package, name) of a Bazel label written in package pkg; None for other repositories."""
    label = re.sub(r"^@@?//", "//", label.strip())
    if label.startswith("@"):
        return None
    if label.startswith("//"):
        path, _, name = label[2:].partition(":")
        path = path.strip("/")
        return path, name or os.path.basename(path)
    return pkg, label.lstrip(":")


def local_name(label, pkg):
    parts = split_label(label, pkg)
    return parts[1] if parts and parts[0] == pkg else None


def label_target(label, pkg):
    """Name of the target in pkg that an (absolute) bazel_test_suite label designates."""
    if not re.match(r"^@{0,2}//", label.strip()):
        return None  # Relative labels do not resolve: bazel_test_suite labels must be absolute.
    return local_name(label, pkg)


def gn_code(text):
    """Blanks out GN comments (outside strings) without moving offsets."""
    return re.sub(r'("(?:\\.|[^"\\])*")|#[^\n]*', lambda m: m.group(1) or " " * len(m.group(0)), text)


def closing(code, i):
    depth, in_str = 0, False
    while i < len(code):
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


_gn_cache = {}


def read_gn(rel):
    if rel not in _gn_cache:
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                _gn_cache[rel] = gn_code(f.read())
        except Exception:
            _gn_cache[rel] = ""
    return _gn_cache[rel]


def gn_block(code, template, name):
    m = re.search(r"\b" + re.escape(template) + r'\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', code)
    return code[m.end() : closing(code, m.end() - 1)] if m else ""


def bazel_test_suites_for(pkg):
    """[(gn_rel, suite, line, param, labels)] for GN bazel_test_suite targets that may list tests
    of pkg: in pkg's BUILD.gn, its ancestors' BUILD.gn, any BUILD.gn mentioning //pkg, and the
    BUILD.gn next to any BUILD.bazel mentioning //pkg (whose test_suite may aggregate pkg's tests)."""
    rels = []
    d = pkg
    while True:
        rels.append(os.path.join(d, "BUILD.gn") if d else "BUILD.gn")
        if not d:
            break
        d = os.path.dirname(d)
    try:
        out = subprocess.run(
            ["git", "-C", workdir, "grep", "-l", "--fixed-strings", "-e", f"//{pkg}:", "-e", f'//{pkg}"',
             "--", "BUILD.gn", "*/BUILD.gn", "BUILD.bazel", "*/BUILD.bazel"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=300,
        ).stdout
        rels += [re.sub(r"BUILD\.bazel$", "BUILD.gn", l.strip()) for l in out.splitlines() if l.strip()]
    except (OSError, subprocess.SubprocessError):
        pass
    result = []
    for rel in dict.fromkeys(rels):
        code = read_gn(rel)
        for m in SUITE_RE.finditer(code):
            body = code[m.end() : closing(code, m.end() - 1)]
            line = code.count("\n", 0, m.start()) + 1
            for lm in SUITE_LIST_RE.finditer(body):
                result.append((rel, m.group(1), line, lm.group(1), re.findall(r'"([^"]+)"', lm.group(2))))
    return result


def refs_local(code, d, name):
    prefix = f"(?://{re.escape(d)})?:" if d else ":"
    return bool(re.search(r'"' + prefix + re.escape(name) + r'(?:\([^"]*\))?"', code))


def refs_from_parent(parent_code, d, name):
    if not parent_code or not d:
        return False
    base = os.path.basename(d)
    pat = r'"(?://' + re.escape(d) + r"|" + re.escape(base) + r"):" + re.escape(name) + r'(?:\([^"]*\))?"'
    return bool(re.search(pat, parent_code))


def suite_is_wired(gn_rel, suite):
    """Whether `group("tests")` next to the suite (directly or via intermediate groups), or the parent BUILD.gn, references it."""
    if suite == "tests":
        return True
    d = os.path.dirname(gn_rel)
    code = read_gn(gn_rel)
    parent_code = read_gn(os.path.join(os.path.dirname(d), "BUILD.gn")) if d else ""
    if refs_from_parent(parent_code, d, suite):
        return True
    groups = {}
    for m in re.finditer(r'\bgroup\(\s*"([^"]+)"\s*\)\s*\{', code):
        groups[m.group(1)] = groups.get(m.group(1), "") + "\n" + code[m.end() : closing(code, m.end() - 1)]
    queue = [g for g in groups if g == "tests" or refs_from_parent(parent_code, d, g)]
    seen = set(queue)
    while queue:
        body = groups[queue.pop(0)]
        if refs_local(body, d, suite):
            return True
        for g in groups:
            if g not in seen and refs_local(body, d, g):
                seen.add(g)
                queue.append(g)
    return False


def list_strings(node):
    """String elements of a Bazel list expression, including `a + b` concatenations."""
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return list_strings(node.left) + list_strings(node.right)
    return bazel_strings(node)


_bazel_suites_cache = {}


def bazel_suites_of(pkg):
    """{test_suite name: [test labels], or None when `tests` is omitted} of pkg's BUILD.bazel."""
    if pkg not in _bazel_suites_cache:
        found = {}
        try:
            with open(os.path.join(workdir, pkg, "BUILD.bazel"), "r", encoding="utf-8") as f:
                t = ast.parse(f.read())
            for node in ast.walk(t):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "test_suite":
                    kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
                    n = kws.get("name")
                    if isinstance(n, ast.Constant) and isinstance(n.value, str):
                        found[n.value] = None if kws.get("tests") is None else list_strings(kws["tests"])
        except Exception:
            pass
        _bazel_suites_cache[pkg] = found
    return _bazel_suites_cache[pkg]


def foreign_suite_tests(label, pkg, td, seen):
    """Names of td's targets that label (written in pkg) runs, following Bazel test_suite targets
    of other packages (e.g. an ancestor's aggregate suite that a GN bazel_test_suite exports)."""
    parts = split_label(label, pkg)
    if not parts or parts in seen:
        return set()
    seen.add(parts)
    l_pkg, l_name = parts
    if l_pkg == td:
        return {l_name}
    out = set()
    for m in bazel_suites_of(l_pkg).get(l_name) or []:  # Without `tests`: only l_pkg's own tests.
        out |= foreign_suite_tests(m, l_pkg, td, seen)
    return out


def git_out(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        ).strip()
    except Exception:
        return ""


def git_lines(args):
    out = git_out(args)
    return [l.strip() for l in out.splitlines() if l.strip()]


def is_ancestor(anc, desc):
    if not anc or not desc:
        return False
    return subprocess.run(
        ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    ).returncode == 0


def maybe_refresh_origin_main(parent_sha):
    if os.environ.get("PLANTER_SKIP_FETCH", "").strip() not in ("", "0"):
        return
    origin_sha = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    stamp_name = (
        "planter-origin-main-fetch-"
        + hashlib.sha256(workdir.encode("utf-8")).hexdigest()[:16]
        + ".stamp"
    )
    stamp_path = os.path.join(tempfile.gettempdir(), stamp_name)
    now = time.time()
    fresh_stamp = False
    try:
        if now - os.path.getmtime(stamp_path) < FETCH_TTL_SECS:
            fresh_stamp = True
    except OSError:
        pass
    fresh_ref = False
    for ref_rel in ("logs/refs/remotes/origin/main", "refs/remotes/origin/main"):
        gp = git_out(["rev-parse", "--git-path", ref_rel])
        if gp:
            abs_gp = gp if os.path.isabs(gp) else os.path.join(workdir, gp)
            try:
                if now - os.path.getmtime(abs_gp) < FETCH_TTL_SECS:
                    fresh_ref = True
                    break
            except OSError:
                pass
    behind_parent = bool(
        origin_sha
        and parent_sha
        and origin_sha != parent_sha
        and is_ancestor(origin_sha, parent_sha)
    )
    if origin_sha and (fresh_stamp or fresh_ref) and not behind_parent:
        return
    try:
        subprocess.run(
            [
                "git",
                "-C",
                workdir,
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
    except Exception:
        pass
    try:
        with open(stamp_path, "w", encoding="utf-8") as sf:
            sf.write(str(int(now)))
    except OSError:
        pass


# 1. Check for unfinished git rebase/merge state or unmerged index entries.
# Note: Do not check standalone `REBASE_HEAD`, which git can leave behind after `git rebase --continue`
# finishes even when `rebase-merge` and `rebase-apply` are gone.
active_rebase_or_merge = None
for state_name in ("rebase-merge", "rebase-apply", "MERGE_HEAD"):
    git_path = git_out(["rev-parse", "--git-path", state_name])
    if git_path:
        abs_git_path = git_path if os.path.isabs(git_path) else os.path.join(workdir, git_path)
        if os.path.exists(abs_git_path):
            active_rebase_or_merge = state_name
            break
unmerged = git_lines(["diff", "--name-only", "--diff-filter=U"])
if active_rebase_or_merge or unmerged:
    state_desc = active_rebase_or_merge or "unmerged index entries"
    findings.append({
        "source": "cq_reachability",
        "category": "unresolved_rebase_or_conflict_markers",
        "severity": "error",
        "file": unmerged[0] if unmerged else "",
        "line": 1,
        "message": (
            f"An unfinished git rebase/merge (`{state_desc}`) is in progress in the checkout"
            + (f" with unmerged file(s): {', '.join(unmerged)}" if unmerged else "")
            + "."
        ),
        "remediation": (
            "Resolve all conflicted files (remove `<<<<<<<`/`=======`/`>>>>>>>` markers and keep entries sorted), "
            "stage them with `git add <file>`, and finish the rebase with `git -c core.editor=true rebase --continue` "
            "(or abort a broken rebase with `git rebase --abort` and re-run `git fetch origin main && git rebase origin/main`)."
        ),
    })

head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
maybe_refresh_origin_main(parent_sha)
origin_main = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"])
effective_diff_base = change_base
if parent_sha and (
    not base_sha
    or (base_sha == head_sha and head_sha != origin_main)
    or not is_ancestor(base_sha, head_sha)
):
    effective_diff_base = "HEAD~1"

# 2. Check changed and target files for leftover conflict markers.
changed_files = set(git_lines(["diff", "--name-only", effective_diff_base]))
changed_files.update(git_lines(["diff", "--name-only", "HEAD"]))
changed_files.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
for td in target_dirs:
    for bname in ("BUILD.bazel", "BUILD.gn"):
        rel_p = os.path.join(td, bname)
        if os.path.isfile(os.path.join(workdir, rel_p)):
            changed_files.add(rel_p)

CONFLICT_MARKER_RE = re.compile(r"^(?:<{7}\s|={7}\s*$|>{7}\s)")
for rel_p in sorted(changed_files):
    abs_p = os.path.join(workdir, rel_p)
    if not os.path.isfile(abs_p):
        continue
    try:
        with open(abs_p, "r", encoding="utf-8", errors="replace") as f:
            for idx, line in enumerate(f, 1):
                if CONFLICT_MARKER_RE.match(line):
                    findings.append({
                        "source": "cq_reachability",
                        "category": "unresolved_rebase_or_conflict_markers",
                        "severity": "error",
                        "file": rel_p,
                        "line": idx,
                        "message": f"Leftover git merge conflict marker `{line.strip()}` in '{rel_p}' at line {idx}.",
                        "remediation": (
                            f"Edit '{rel_p}' to remove all `<<<<<<<`, `=======`, and `>>>>>>>` conflict markers, "
                            "keep both upstream `origin/main` entries and this change's additions in sorted order, "
                            "and stage the resolved file with `git add`."
                        ),
                    })
                    break
    except OSError:
        pass

# 3. Check whether the change merges cleanly onto `origin/main` (`git merge-tree --write-tree`).
if origin_main and head_sha and head_sha != origin_main:
    merge_base = git_out(["merge-base", origin_main, head_sha])
    if merge_base and merge_base != origin_main:
        dirty_tracked = git_lines(["diff", "--name-only", "--ignore-submodules=all", "HEAD"])
        untracked_td = []
        for td in target_dirs:
            if os.path.isdir(os.path.join(workdir, td)):
                untracked_td.extend(git_lines(["ls-files", "--others", "--exclude-standard", "--", td]))
        worktree_tree = ""
        if not dirty_tracked and not untracked_td:
            worktree_tree = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{tree}"])
        else:
            git_dir = git_out(["rev-parse", "--absolute-git-dir"])
            if git_dir and os.path.isdir(git_dir):
                fd, tmp_idx = tempfile.mkstemp(dir=git_dir, prefix=".planter-cq-merge-index.")
                os.close(fd)
                try:
                    real_idx = os.path.join(git_dir, "index")
                    if os.path.isfile(real_idx):
                        shutil.copyfile(real_idx, tmp_idx)
                    else:
                        os.unlink(tmp_idx)
                    env = dict(os.environ, GIT_INDEX_FILE=tmp_idx)
                    if dirty_tracked:
                        subprocess.run(
                            ["git", "-C", workdir, "add", "-A", "--"] + dirty_tracked,
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
                        )
                    for td in target_dirs:
                        if os.path.exists(os.path.join(workdir, td)):
                            subprocess.run(
                                ["git", "-C", workdir, "add", "-A", "--", td],
                                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
                            )
                    wt_proc = subprocess.run(
                        ["git", "-C", workdir, "write-tree"],
                        env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, check=False,
                    )
                    if wt_proc.returncode == 0:
                        worktree_tree = wt_proc.stdout.strip()
                finally:
                    try:
                        os.unlink(tmp_idx)
                    except OSError:
                        pass
        if worktree_tree:
            mt_proc = subprocess.run(
                [
                    "git", "-C", workdir, "merge-tree", "--write-tree", "--name-only",
                    f"--merge-base={merge_base}", origin_main, worktree_tree,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            if mt_proc.returncode != 0:
                mt_lines = [l.strip() for l in (mt_proc.stdout or "").splitlines()]
                conflicted_files = []
                conflict_msgs = []
                in_files = True
                for idx, line in enumerate(mt_lines):
                    if idx == 0 and re.fullmatch(r"[0-9a-f]{40}", line):
                        continue
                    if not line:
                        in_files = False
                        continue
                    if in_files and not line.startswith(("Auto-merging ", "CONFLICT ")):
                        if line not in conflicted_files:
                            conflicted_files.append(line)
                    elif line.startswith("CONFLICT "):
                        conflict_msgs.append(line)
                first_file = conflicted_files[0] if conflicted_files else ""
                files_desc = ", ".join(f"'{f}'" for f in conflicted_files) if conflicted_files else "modified files"
                detail_desc = f" ({'; '.join(conflict_msgs)})" if conflict_msgs else ""
                findings.append({
                    "source": "cq_reachability",
                    "category": "upstream_rebase_conflict",
                    "severity": "error",
                    "file": first_file,
                    "line": 1,
                    "message": (
                        f"Change based on {merge_base[:11]} has a merge conflict with upstream `origin/main` "
                        f"({origin_main[:11]}) in {files_desc}{detail_desc}, which causes Gerrit CQ "
                        "(`checkout|jiri patch`) to fail across all builders with `Failed to rebase`."
                    ),
                    "remediation": (
                        "Rebase the change onto the latest `origin/main` so Gerrit CQ can patch it cleanly: "
                        "run `git fetch origin main && git rebase origin/main` (commit or `git stash` any uncommitted edits first), "
                        "resolve each conflicted file keeping both `origin/main`'s entries and this change's additions "
                        "stage the resolved file(s) with `git add <file>`, and finish with "
                        "`git -c core.editor=true rebase --continue` (preserving the existing `Change-Id` footer)."
                    ),
                })

unwired_reported = set()  # (gn_rel, suite) already reported as unwired
for td in target_dirs:
    bazel_rel = os.path.join(td, "BUILD.bazel")
    bazel_full = os.path.join(workdir, bazel_rel)
    gn_rel = os.path.join(td, "BUILD.gn")
    gn_full = os.path.join(workdir, gn_rel)

    if not os.path.isfile(bazel_full):
        continue

    try:
        with open(bazel_full, "r", encoding="utf-8") as f:
            bazel_src = f.read()
        tree = ast.parse(bazel_src, filename=bazel_rel)
    except Exception:
        continue

    gn_src = ""
    if os.path.isfile(gn_full):
        try:
            with open(gn_full, "r", encoding="utf-8") as f:
                gn_src = f.read()
        except Exception:
            gn_src = ""

    test_targets = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = ""
        if isinstance(node.func, ast.Name):
            rule_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule_name = node.func.attr
        if rule_name not in TEST_RULES:
            continue
        t_name = ""
        lineno = getattr(node, "lineno", 1)
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                t_name = kw.value.value
                lineno = kw.lineno
        if t_name:
            test_targets.append((t_name, rule_name, lineno))

    parent_gn = os.path.join(workdir, os.path.dirname(td), "BUILD.gn")
    parent_gn_src = ""
    if os.path.isfile(parent_gn):
        try:
            with open(parent_gn, "r", encoding="utf-8") as f:
                parent_gn_src = f.read()
        except Exception:
            parent_gn_src = ""

    for t_name, rule_name, lineno in test_targets:
        ref_patterns = [
            f'":{t_name}"',
            f'"{t_name}"',
            f":{t_name}\"",
            f"{os.path.basename(td)}:{t_name}",
        ]
        referenced_in_gn = any(p in gn_src for p in ref_patterns)
        referenced_in_parent = any(p in parent_gn_src for p in ref_patterns) or (
            f'"{os.path.basename(td)}:tests"' in parent_gn_src
        )
        referenced_in_bazel = bazel_src.count(f'":{t_name}"') >= 1 or bazel_src.count(f'"{t_name}"') >= 2
        if not (referenced_in_gn or referenced_in_parent or referenced_in_bazel):
            findings.append({
                "source": "cq_reachability",
                "category": "orphaned_test_target",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": (
                    f"Test target ':{t_name}' ({rule_name}) in '{bazel_rel}' is not referenced by "
                    "`group(\"tests\")`, a test package, or the parent directory's `tests` group, "
                    "so it would be silently dropped from CQ."
                ),
                "remediation": (
                    f"Wire ':{t_name}' into `group(\"tests\")` (or its enclosing `fuchsia_unittest_package` / "
                    "parent `BUILD.gn` `tests` group) so CQ continues to build and run this test."
                ),
            })

    # 3. Bazel tests visible to Fuchsia test runners only via a GN bazel_test_suite.
    exportable = []  # (name, kind, rule, lineno, GN twin: the bazel2gn-generated GN test it runs, or None)
    suites = {}  # test_suite name -> list of member names, or None for "every test"
    all_tests = []  # names of non-manual test targets of the package (for empty test_suite)
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = node.func.id if isinstance(node.func, ast.Name) else (
            node.func.attr if isinstance(node.func, ast.Attribute) else ""
        )
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        name_v = kws.get("name")
        if not (isinstance(name_v, ast.Constant) and isinstance(name_v.value, str)):
            continue
        t_name, lineno = name_v.value, getattr(node, "lineno", 1)
        manual = "manual" in bazel_strings(kws.get("tags"))
        if rule_name in DEVICE_TEST_RULES:
            exportable.append((t_name, "target", rule_name, lineno, None))
        elif rule_name in HOST_TEST_RULES:
            twin = t_name if rule_name in GN_CONVERTED_HOST_TEST_RULES else None
            exportable.append((t_name, "host", rule_name, lineno, twin))
        elif rule_name in RUST_TEST_OWNERS and "host" in unit_test_envs(kws):
            exportable.append((t_name + "_test", "host", f"{rule_name}(with_unit_tests)", lineno, None))
        elif rule_name == "test_suite":
            tests_v = kws.get("tests")
            suites[t_name] = None if tests_v is None else [
                local_name(s, td) for s in bazel_strings(tests_v)
            ]
        if not manual and (rule_name in DEVICE_TEST_RULES | HOST_TEST_RULES or rule_name.endswith("_test")):
            all_tests.append(t_name)
        if rule_name in RUST_TEST_OWNERS and "host" in unit_test_envs(kws):
            all_tests.append(t_name + "_test")
        if rule_name == "rustc_test" or (rule_name in RUST_TEST_OWNERS and "fuchsia" in unit_test_envs(kws)):
            device_bin = t_name if rule_name == "rustc_test" else t_name + "_test"
            if FX_TEST_AVAILABLE and not re.search(
                r'\bbinary\s*=\s*"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"', bazel_src
            ):
                findings.append({
                    "source": "cq_reachability",
                    "category": "unpackaged_rust_device_test",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": (
                        f"Rust device test binary ':{device_bin}' ({rule_name}) in '{bazel_rel}' is not packaged by any "
                        "`fx_packaged_binary`, so no `fx_test` runs it and it is missing from tests.json."
                    ),
                    "remediation": (
                        f"Add `fx_packaged_binary(testonly = True, binary = \":{device_bin}\", binary_name = ...)` "
                        "(GN's output name, e.g. `<crate>_lib_test`), a `meta/<component>.cml` including "
                        "`//src/sys/test_runners/rust/default.shard.cml` and `syslog/use.shard.cml` with "
                        "`program.binary` = `bin/<binary_name>`, `fx_component_manifest`, `fx_test_component`, "
                        "`fx_package(test_components = ...)` and `fx_test`, exported by a GN `bazel_test_suite` "
                        "(see \"Migrating Tests\" in the coder instructions). If the tests only ran on host in GN, "
                        "use `with_unit_tests = \"host\"` instead."
                    ),
                })
    if not exportable or not FX_TEST_AVAILABLE:
        continue

    def members(suite, seen):
        """Local test names a Bazel test_suite of this package runs (recursively)."""
        if suite in seen:
            return set()
        seen.add(suite)
        listed = suites[suite] if suites[suite] is not None else all_tests
        out = set()
        for m in listed:
            if m is None:
                continue
            if m in suites:
                out |= members(m, seen)
            else:
                out.add(m)
        return out

    listed_in = {}  # local test name -> [(gn_rel, suite, param, line)]
    for s_rel, s_name, s_line, param, labels in bazel_test_suites_for(td):
        for label in labels:
            m = label_target(label, td)
            if m is not None:
                found = {m}
            elif re.match(r"^@{0,2}//", label.strip()):
                found = foreign_suite_tests(label, td, td, set())
            else:
                continue
            names = set()
            for f_name in found:
                names |= members(f_name, set()) if f_name in suites else {f_name}
            for n in names:
                listed_in.setdefault(n, []).append((s_rel, s_name, param, s_line))

    base_ref = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
    base_gn_src = ""
    for ref in ([base_ref, "HEAD~1"] if base_ref == "HEAD" else [base_ref]):
        try:
            out = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"], stderr=subprocess.DEVNULL, text=True
            )
            if "## BAZEL2GN SENTINEL" not in out:
                base_gn_src = gn_code(out)
                break
            if not base_gn_src:
                base_gn_src = gn_code(out)
        except Exception:
            pass

    gn_pre = gn_src[: SENTINEL_RE.search(gn_src).start()] if SENTINEL_RE.search(gn_src) else gn_src
    for t_name, kind, rule_name, lineno, twin in exportable:
        param = "target_tests" if kind == "target" else "host_tests"
        entries = listed_in.get(t_name, [])
        if twin and SENTINEL_RE.search(gn_src):
            # GN already builds this test (bazel2gn output): it is reached through GN as today.
            # Exporting it too would run the same test twice when GN runs it on host.
            gn_host = re.search(r'":' + re.escape(twin) + r'\(', gn_pre) or f"//{td}:{twin}(" in parent_gn_src
            if entries and gn_host:
                s_rel, s_name, _, s_line = entries[0]
                findings.append({
                    "source": "cq_reachability",
                    "category": "duplicate_host_test_export",
                    "severity": "error",
                    "file": s_rel,
                    "line": s_line,
                    "message": (
                        f"`bazel_test_suite(\"{s_name}\")` in '{s_rel}' exports {rule_name} ':{t_name}', but GN "
                        f"already runs the bazel2gn-generated `:{twin}` on host (referenced with a toolchain in "
                        f"'{gn_rel}'), so the same test would run twice."
                    ),
                    "remediation": (
                        f"Remove `//{td}:{t_name}` from the suite (and a `wrap_host_rust_test` added only for it); "
                        f"keep `\":{twin}($host_toolchain)\"` in `group(\"tests\")`."
                    ),
                })
                continue
            was_wired_in_base_gn = bool(
                base_gn_src and re.search(r'":' + re.escape(twin) + r'(?:\(|")', base_gn_src)
            )
            had_with_unit_tests_in_base_gn = bool(
                base_gn_src and re.search(r"\bwith_unit_tests\s*=\s*true\b", base_gn_src)
            )
            if (
                gn_host
                or (
                    twin == t_name
                    and (re.search(r'":' + re.escape(twin) + r'"', gn_pre) or f"//{td}:{twin}\"" in parent_gn_src)
                )
                or (
                    twin == t_name
                    and not entries
                    and had_with_unit_tests_in_base_gn
                    and not was_wired_in_base_gn
                    and f"//{td}:{twin}" not in parent_gn_src
                    and f'"{os.path.basename(td)}:tests"' not in parent_gn_src
                )
            ):
                continue
        if not entries:
            findings.append({
                "source": "cq_reachability",
                "category": "unexported_bazel_test",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not listed in any GN "
                    f"`bazel_test_suite({param} = [...])`, so it is missing from tests.json: neither "
                    "`fx test` nor infra runs it, and defining it (or running it with `fx bazel test`) "
                    "does not change that."
                ),
                "remediation": (
                    f"In '{gn_rel}' (above any BAZEL2GN SENTINEL), add "
                    "`import(\"//build/bazel/bazel_test_suite.gni\")` and "
                    f"`bazel_test_suite(\"<name>\") {{ {param} = [ \"//{td}:{t_name}\" ] }}` (absolute Bazel "
                    "labels; a Bazel `test_suite` label also works), named after the GN test package it "
                    "replaces, and list `\":<name>\"` in `group(\"tests\")`."
                ),
            })
            continue
        for s_rel, s_name, s_param, s_line in entries:
            if s_param != param:
                findings.append({
                    "source": "cq_reachability",
                    "category": "bazel_test_suite_kind_mismatch",
                    "severity": "error",
                    "file": s_rel,
                    "line": s_line,
                    "message": (
                        f"`bazel_test_suite(\"{s_name}\")` in '{s_rel}' lists {rule_name} ':{t_name}' in "
                        f"`{s_param}`, but {'device tests (`fx_test`)' if kind == 'target' else 'host tests'} "
                        f"belong in `{param}` (they are built in a different Bazel configuration)."
                    ),
                    "remediation": f"Move `//{td}:{t_name}` (or its `test_suite`) from `{s_param}` to `{param}`.",
                })
        if not any(suite_is_wired(s_rel, s_name) for s_rel, s_name, _, _ in entries):
            s_rel, s_name, _, s_line = entries[0]
            if (s_rel, s_name) in unwired_reported:
                continue
            unwired_reported.add((s_rel, s_name))
            findings.append({
                "source": "cq_reachability",
                "category": "unwired_bazel_test_suite",
                "severity": "error",
                "file": s_rel,
                "line": s_line,
                "message": (
                    f"Bazel test ':{t_name}' ({rule_name}) is listed in `bazel_test_suite(\"{s_name}\")` in "
                    f"'{s_rel}', but no `group(\"tests\")` references that suite, so it is not reachable from "
                    "the test roots and stays out of tests.json."
                ),
                "remediation": (
                    f"Add `\":{s_name}\"` to `group(\"tests\")` in '{s_rel}' (or reference "
                    f"`//{os.path.dirname(s_rel)}:{s_name}` from the parent directory's `tests` group)."
                ),
            })

print(json.dumps(findings, indent=2))
PYEOF
