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
#    tests.json and neither `fx test` nor infra runs it. The GN copy bazel2gn generates for a
#    dual-build library's test runs the GN build of the test, not the Bazel one, so the Bazel
#    test needs the export too.
# 4. Every Rust device test binary (`rustc_test`, or the `<name>_test` of
#    `rustc_*(with_unit_tests = "fuchsia"|"both")`) must be packaged by an
#    `fx_unittest_package` or an `fx_packaged_binary` (for an `fx_test`) in the same
#    BUILD.bazel, otherwise nothing runs it (`unpackaged_rust_device_test`).
#    Only runs when the checkout has `build/bazel/rules/testing/fx_test.bzl` (older
#    checkouts keep tests in GN).
# 5. The change must merge cleanly onto `origin/main` without rebase conflicts
#    (`git merge-tree --write-tree`) or leftover conflict markers so Gerrit CQ
#    (`checkout|jiri patch`) does not fail with `Failed to rebase`.
# 6. Every workspace Bazel package referenced by `BUILD.bazel` in the change must
#    exist in `origin/main` or in the top commit `HEAD` (never only in an unmerged
#    local ancestor commit between `merge-base(origin/main, HEAD)` and `HEAD~1`),
#    otherwise Gerrit CQ fails with `no such package '<pkg>': BUILD file not found`.

# 6. Every main-repository `//<pkg>:...` dependency referenced in a migrated
#    `BUILD.bazel` must resolve to an existing `<pkg>/BUILD.bazel` or `<pkg>/BUILD`
#    package in the checkout (and not be missing from the commit ancestry when a
#    stacked change drops its parent dependency commit).

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
    "host_go_test",
    "host_rustc_test",
    "python_host_test",
    "fx_unittest_package",
    "fuchsia_unittest_package",
    "fuchsia_test_package",
}

FETCH_TTL_SECS = (
    60
    if os.environ.get("PLANTER_SKIP_BUILD", "").strip() in ("1", "true")
    else 15
)

findings = []

DEVICE_TEST_RULES = {"fx_test", "fx_unittest_package"}
HOST_TEST_RULES = {"host_rustc_test", "host_go_test", "host_py_test", "host_test", "wrap_host_rust_test"}
# The GN copy bazel2gn generates for a test runs the GN build of the test, so it never stands
# in for exporting the Bazel test: both run while the library is in both build systems.
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


# Check whether a dual-built test binary is packaged by a GN test package/component kept
# above the BAZEL2GN SENTINEL in BUILD.gn.
def is_packaged_by_kept_gn_test_package(gn_text, pkg_dir, bin_name):
    if not gn_text:
        return False
    pre = (
        gn_text[: SENTINEL_RE.search(gn_text).start()]
        if SENTINEL_RE.search(gn_text)
        else gn_text
    )
    pre_clean = gn_code(pre)
    if not re.search(
        r"\b(?:fuchsia_test_package|fuchsia_unittest_package)\s*\(", pre_clean
    ):
        return False
    ref_re = re.compile(
        r'"(?::|//'
        + re.escape(pkg_dir)
        + r":)"
        + re.escape(bin_name)
        + r'(?:\([^"]*\))?"'
    )
    return ref_re.search(pre_clean) is not None


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


ROOT_TEST_GROUPS = {
    "tests",
    "host_tests",
    "target_tests",
    "hermetic_tests",
    "non_hermetic_tests",
    "e2e_tests",
    "tests_no_e2e",
}
GN_TESTS_ROOT_RE = re.compile(
    r'\b(?:group|standardized_test_groups)\(\s*"tests"\s*\)'
)
GN_GROUP_OR_STD_TEST_GROUPS_RE = re.compile(
    r'\b(group|standardized_test_groups)\(\s*"([^"]+)"\s*\)\s*\{'
)


# Check whether a GN `bazel_test_suite` is wired into a root test group (`group("tests")`,
# `standardized_test_groups(...)`, or a standard root group such as `host_tests`/`hermetic_tests`),
# either directly, via intermediate local groups, or from the parent `BUILD.gn`.
def suite_is_wired(gn_rel, suite):
    if suite in ROOT_TEST_GROUPS:
        return True
    d = os.path.dirname(gn_rel)
    code = read_gn(gn_rel)
    parent_code = read_gn(os.path.join(os.path.dirname(d), "BUILD.gn")) if d else ""
    if refs_from_parent(parent_code, d, suite):
        return True
    groups = {}
    std_roots = set()
    for m in GN_GROUP_OR_STD_TEST_GROUPS_RE.finditer(code):
        tmpl, g_name = m.group(1), m.group(2)
        body = code[m.end() : closing(code, m.end() - 1)]
        groups[g_name] = groups.get(g_name, "") + "\n" + body
        if tmpl == "standardized_test_groups":
            std_roots.add(g_name)
    queue = [
        g
        for g in groups
        if g in ROOT_TEST_GROUPS
        or g in std_roots
        or refs_from_parent(parent_code, d, g)
    ]
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


# Find closing `]` matching `code[i] == "["` outside GN string literals.
def closing_bracket(code, i):
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
        elif c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


# Detect duplicate `<attr> = [ <non-empty> ]` assignments in the same GN `{ ... }` scope,
# which cause `gn gen` to fail with `Replacing nonempty list`.
def find_gn_duplicate_list_assignments(gn_text):
    code = gn_code(gn_text)
    if not code.strip():
        return []
    scopes = [(0, len(code))]
    in_str = False
    i = 0
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
            end_i = closing(code, i)
            scopes.append((i + 1, end_i))
        i += 1

    dups = []
    assign_re = re.compile(r"(?m)^\s*([A-Za-z_]\w*)\s*(=|\+=|-=)\s*\[")
    for s_start, s_end in scopes:
        buf = list(code[s_start:s_end])
        b_in_str = False
        b_brack = 0
        j = 0
        while j < len(buf):
            ch = buf[j]
            if b_in_str:
                if ch == "\\":
                    j += 2
                    continue
                b_in_str = ch != '"'
            elif ch == '"':
                b_in_str = True
            elif ch == "[":
                b_brack += 1
            elif ch == "]":
                b_brack = max(0, b_brack - 1)
            elif ch == "{" and b_brack == 0:
                sub_end = closing(code, s_start + j) - s_start
                for k in range(j, min(sub_end + 1, len(buf))):
                    if buf[k] != "\n":
                        buf[k] = " "
                j = sub_end + 1
                continue
            j += 1
        direct = "".join(buf)
        seen_nonempty = {}
        for m in assign_re.finditer(direct):
            var_name, op = m.group(1), m.group(2)
            open_b = m.end() - 1
            close_b = closing_bracket(direct, open_b)
            inner = direct[open_b + 1 : close_b].strip()
            line_no = code.count("\n", 0, s_start + m.start()) + 1
            if op == "=":
                if not inner:
                    seen_nonempty.pop(var_name, None)
                elif var_name in seen_nonempty:
                    dups.append((var_name, seen_nonempty[var_name], line_no))
                else:
                    seen_nonempty[var_name] = line_no
    return dups


# Detect duplicate top-level rule target `name = "..."` definitions in a BUILD.bazel file.
def find_bazel_duplicate_targets(bazel_text, bazel_rel=""):
    if not bazel_text.strip():
        return []
    try:
        tree = ast.parse(bazel_text, filename=bazel_rel or "<BUILD.bazel>")
    except Exception:
        return []
    seen = {}
    dups = []
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        rule_name = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "target")
        )
        t_name = None
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                t_name = kw.value.value
                break
        if not t_name:
            continue
        line_no = getattr(stmt, "lineno", 1)
        if t_name in seen:
            dups.append((t_name, rule_name, seen[t_name], line_no))
        else:
            seen[t_name] = line_no
    return dups


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


# Find the nearest ancestor BUILD.bazel test_suite exported by a wired ancestor bazel_test_suite.
def nearest_ancestor_test_suite(td, param):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    if not ancestors:
        return None
    anc_set = set(ancestors)
    queue = []
    discovered = []
    seen = set()
    for anc in ancestors:
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        code = read_gn(anc_gn_rel)
        if not code:
            continue
        for m in SUITE_RE.finditer(code):
            gn_suite = m.group(1)
            if not suite_is_wired(anc_gn_rel, gn_suite):
                continue
            body = code[m.end() : closing(code, m.end() - 1)]
            for lm in SUITE_LIST_RE.finditer(body):
                if lm.group(1) != param:
                    continue
                for lbl in re.findall(r'"([^"]+)"', lm.group(2)):
                    sp = split_label(lbl, anc)
                    if (
                        sp
                        and sp[0] in anc_set
                        and sp[1] in bazel_suites_of(sp[0])
                        and sp not in seen
                    ):
                        seen.add(sp)
                        queue.append(sp)
                        discovered.append(sp)
    while queue:
        cur_pkg, cur_suite = queue.pop(0)
        for m_lbl in bazel_suites_of(cur_pkg).get(cur_suite) or []:
            sp = split_label(m_lbl, cur_pkg)
            if (
                sp
                and sp[0] in anc_set
                and sp[1] in bazel_suites_of(sp[0])
                and sp not in seen
            ):
                seen.add(sp)
                queue.append(sp)
                discovered.append(sp)
    if not discovered:
        return None
    discovered.sort(key=lambda item: (item[0].count("/"), len(item[0]), item[1] == param))
    return discovered[-1]


# Find the canonical ancestor area directory where a flat BUILD.bazel test_suite and BUILD.gn
# bazel_test_suite should be created when no ancestor suite for `param` exists yet.
def preferred_ancestor_area_suite(td, param):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    if not ancestors:
        return None
    # Prefer the nearest ancestor that already defines any wired bazel_test_suite or test_suite.
    for anc in reversed(ancestors):
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        code = read_gn(anc_gn_rel)
        if code and any(
            suite_is_wired(anc_gn_rel, m.group(1)) for m in SUITE_RE.finditer(code)
        ):
            return (anc, param)
        if bazel_suites_of(anc) and code and re.search(
            r'\bgroup\(\s*"tests(?:_no_e2e)?"\s*\)\s*\{', code
        ):
            return (anc, param)
    # Otherwise, resolve the top-level area root (e.g., //<root>/<area> or //<top>).
    cand = None
    if parts[0] == "src" and len(parts) >= 3:
        cand = "/".join(parts[:2])
    elif parts[:2] == ["zircon", "system"] and len(parts) >= 4:
        cand = "/".join(parts[:3])
    elif len(parts) >= 2 and parts[0] not in ("src", "zircon", "third_party"):
        cand = parts[0]
    if cand and cand != td:
        cand_gn = read_gn(os.path.join(cand, "BUILD.gn"))
        if cand_gn and re.search(r'\bgroup\(\s*"tests(?:_no_e2e)?"\s*\)\s*\{', cand_gn):
            return (cand, param)
    return None


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


def git_success(args):
    try:
        return subprocess.run(
            ["git", "-C", workdir] + args,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode == 0
    except Exception:
        return False


# Refresh local `refs/remotes/origin/main` when stale so `git merge-tree` checks
# against current upstream `origin/main` and only caches a stamp on success.
def maybe_refresh_origin_main(parent_sha):
    if os.environ.get("PLANTER_SKIP_FETCH", "").strip() not in ("", "0"):
        return
    force_fetch = os.environ.get("PLANTER_FORCE_FETCH", "").strip() in ("1", "true")
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
        with open(stamp_path, "r", encoding="utf-8") as sf:
            parts = sf.read().strip().split()
        if (
            len(parts) == 2
            and now - int(parts[0]) < FETCH_TTL_SECS
            and origin_sha
            and parts[1] == origin_sha
        ):
            fresh_stamp = True
    except (OSError, ValueError):
        pass
    fresh_ref = False
    if origin_sha and origin_sha != parent_sha:
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
    if origin_sha and not force_fetch and (fresh_stamp or fresh_ref) and not behind_parent:
        return
    # Fast-path: check remote refs/heads/main SHA first to avoid a full fetch when unchanged.
    remote_sha = ""
    try:
        ls_proc = subprocess.run(
            ["git", "-C", workdir, "ls-remote", "--exit-code", "origin", "refs/heads/main"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=15,
            check=False,
        )
        if ls_proc.returncode == 0 and ls_proc.stdout:
            remote_sha = ls_proc.stdout.strip().split()[0]
    except Exception:
        remote_sha = ""
    if origin_sha and remote_sha and remote_sha == origin_sha:
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(f"{int(now)} {origin_sha}\n")
        except OSError:
            pass
        return
    fetch_ok = False
    try:
        fetch_proc = subprocess.run(
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
            timeout=90,
            check=False,
        )
        fetch_ok = fetch_proc.returncode == 0
    except Exception:
        fetch_ok = False
    new_origin_sha = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    if fetch_ok and new_origin_sha and (not remote_sha or new_origin_sha == remote_sha):
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(f"{int(time.time())} {new_origin_sha}\n")
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
            "Resolve all conflicted files (remove `<<<<<<<`/`=======`/`>>>>>>>` markers and keep "
            "entries sorted; if a conflicted dependency `BUILD.bazel`/`BUILD.gn` outside the "
            "target directory was only edited for `visibility` and `origin/main` already grants "
            "sufficient visibility such as `[\"//:__subpackages__\"]`, take `origin/main`'s "
            "version cleanly with `git checkout origin/main -- <file>`), stage resolved files "
            "with `git add <file>`, and finish with `git -c core.editor=true rebase --continue` "
            "(or abort with `git rebase --abort` and re-run "
            "`git fetch origin main && git rebase origin/main`)."
        ),
    })

head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
maybe_refresh_origin_main(parent_sha)
origin_main = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
head_is_on_origin_main = bool(
    origin_main and head_sha and is_ancestor(head_sha, origin_main)
)
base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"])
effective_diff_base = change_base
wt_changed_files = (
    git_lines(["diff", "--name-only", "HEAD"])
    + git_lines(["ls-files", "--others", "--exclude-standard"])
)
wt_dirty = bool(wt_changed_files)
wt_touches_target = bool(target_dirs) and any(
    any(p.startswith(td + "/") for td in target_dirs) for p in wt_changed_files
)
head_commit_files = (
    git_lines(["diff", "--name-only", "HEAD~1", "HEAD"]) if parent_sha else []
)
head_touches_target = bool(target_dirs) and any(
    any(p.startswith(td + "/") for td in target_dirs) for p in head_commit_files
)
if parent_sha and (
    not base_sha
    or (
        change_base == "HEAD"
        and base_sha == head_sha
        and not head_is_on_origin_main
        and not wt_dirty
        and head_touches_target
    )
    or not is_ancestor(base_sha, head_sha)
):
    effective_diff_base = "HEAD~1"
# Check whether HEAD is the task's own commit rather than an upstream origin/main commit,
# PLANTER_CHANGE_BASE, or a shared base commit during a parallel uncommitted working-tree migration.
head_is_task_commit = bool(
    head_sha
    and not head_is_on_origin_main
    and (not base_sha or base_sha != head_sha or effective_diff_base == "HEAD~1")
    and not (wt_touches_target and not head_touches_target)
)

# 2. Check changed and target files for leftover conflict markers.
changed_files = set(git_lines(["diff", "--name-only", effective_diff_base]))
changed_files.update(git_lines(["diff", "--name-only", "HEAD"]))
changed_files.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
for td in target_dirs:
    for bname in ("BUILD.bazel", "BUILD.gn"):
        rel_p = os.path.join(td, bname)
        if os.path.isfile(os.path.join(workdir, rel_p)):
            changed_files.add(rel_p)

# Collect changed and ancestor BUILD.gn / BUILD.bazel files to audit for duplicate definitions.
audit_build_files = set(changed_files)
for td in target_dirs:
    cur_anc = os.path.dirname(td)
    while cur_anc:
        for bname in ("BUILD.bazel", "BUILD.gn"):
            anc_rel = os.path.join(cur_anc, bname)
            if os.path.isfile(os.path.join(workdir, anc_rel)):
                audit_build_files.add(anc_rel)
        cur_anc = os.path.dirname(cur_anc)

worktree_gn_dups = {}
worktree_bz_dups = {}
for rel_p in sorted(audit_build_files):
    abs_p = os.path.join(workdir, rel_p)
    if not os.path.isfile(abs_p):
        continue
    try:
        with open(abs_p, "r", encoding="utf-8", errors="replace") as f:
            src_txt = f.read()
    except OSError:
        continue
    if rel_p.endswith((".gn", ".gni")):
        gn_dups = find_gn_duplicate_list_assignments(src_txt)
        worktree_gn_dups[rel_p] = {(v, p_ln, c_ln) for v, p_ln, c_ln in gn_dups}
        for var_name, prev_ln, cur_ln in gn_dups:
            findings.append({
                "source": "cq_reachability",
                "category": "duplicate_gn_list_assignment",
                "severity": "error",
                "file": rel_p,
                "line": cur_ln,
                "message": (
                    f"'{rel_p}' assigns non-empty list `{var_name} = [...]` at line {cur_ln} "
                    f"after already assigning `{var_name} = [...]` at line {prev_ln} in the "
                    "same GN scope, which causes `gn gen` to fail across CQ builders with "
                    "`ERROR: Replacing nonempty list`."
                ),
                "remediation": (
                    f"Remove the duplicate `{var_name} = [...]` assignment at line {cur_ln} in "
                    f"'{rel_p}' (or merge its entries into the `{var_name} = [...]` list at "
                    f"line {prev_ln} in sorted order)."
                ),
            })
    elif os.path.basename(rel_p) in ("BUILD.bazel", "BUILD"):
        bz_dups = find_bazel_duplicate_targets(src_txt, rel_p)
        worktree_bz_dups[rel_p] = {(t, r) for t, r, _, _ in bz_dups}
        for t_name, rule_name, prev_ln, cur_ln in bz_dups:
            findings.append({
                "source": "cq_reachability",
                "category": "duplicate_bazel_target",
                "severity": "error",
                "file": rel_p,
                "line": cur_ln,
                "message": (
                    f"'{rel_p}' defines `{rule_name}(name = \"{t_name}\")` at line {cur_ln}, "
                    f"duplicating the `{t_name}` target already defined at line {prev_ln} in "
                    "the same package."
                ),
                "remediation": (
                    f"Merge the duplicate `{rule_name}(name = \"{t_name}\")` blocks at lines "
                    f"{prev_ln} and {cur_ln} in '{rel_p}' into a single `{rule_name}(name = "
                    f"\"{t_name}\")` definition combining their target lists in sorted order."
                ),
            })

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
                            f"Edit '{rel_p}' to remove all `<<<<<<<`, `=======`, and `>>>>>>>` "
                            "conflict markers, keep both upstream `origin/main` entries and this "
                            "change's additions in sorted order (or run "
                            f"`git checkout origin/main -- {rel_p}` if '{rel_p}' is an external "
                            "dependency package where `origin/main` already widened visibility), "
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
        dirty_tracked = git_lines(
            ["diff", "--name-only", "--ignore-submodules=all", "HEAD"]
        )
        untracked_build = [
            f
            for f in git_lines(["ls-files", "--others", "--exclude-standard"])
            if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn", "BUILD")
            or f.endswith((".bzl", ".gni", ".cml"))
        ]
        for td in target_dirs:
            if os.path.isdir(os.path.join(workdir, td)):
                for f in git_lines(
                    ["ls-files", "--others", "--exclude-standard", "--", td]
                ):
                    if f not in untracked_build:
                        untracked_build.append(f)
        worktree_tree = ""
        if not dirty_tracked and not untracked_build:
            worktree_tree = git_out(
                ["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]
            )
        else:
            git_dir = git_out(["rev-parse", "--absolute-git-dir"])
            if git_dir and os.path.isdir(git_dir):
                fd, tmp_idx = tempfile.mkstemp(
                    dir=git_dir, prefix=".planter-cq-merge-index."
                )
                os.close(fd)
                try:
                    real_idx = os.path.join(git_dir, "index")
                    if os.path.isfile(real_idx):
                        shutil.copyfile(real_idx, tmp_idx)
                    else:
                        os.unlink(tmp_idx)
                    env = dict(os.environ, GIT_INDEX_FILE=tmp_idx)
                    to_stage = list(dict.fromkeys(dirty_tracked + untracked_build))
                    if to_stage:
                        subprocess.run(
                            ["git", "-C", workdir, "add", "-A", "--"] + to_stage,
                            env=env,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        )
                    wt_proc = subprocess.run(
                        ["git", "-C", workdir, "write-tree"],
                        env=env,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL,
                        text=True,
                        check=False,
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
                    "git",
                    "-C",
                    workdir,
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    f"--merge-base={merge_base}",
                    origin_main,
                    worktree_tree,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            if mt_proc.returncode != 0:
                mt_lines = [
                    l.strip() for l in (mt_proc.stdout or "").splitlines()
                ]
                conflicted_files = []
                conflict_msgs = []
                in_files = True
                for idx, line in enumerate(mt_lines):
                    if idx == 0 and re.fullmatch(r"[0-9a-f]{40}", line):
                        continue
                    if not line:
                        in_files = False
                        continue
                    if in_files and not line.startswith(
                        ("Auto-merging ", "CONFLICT ")
                    ):
                        if line not in conflicted_files:
                            conflicted_files.append(line)
                    elif line.startswith("CONFLICT "):
                        conflict_msgs.append(line)
                first_file = conflicted_files[0] if conflicted_files else ""
                files_desc = (
                    ", ".join(f"'{f}'" for f in conflicted_files)
                    if conflicted_files
                    else "modified files"
                )
                detail_desc = (
                    f" ({'; '.join(conflict_msgs)})" if conflict_msgs else ""
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "upstream_rebase_conflict",
                    "severity": "error",
                    "file": first_file,
                    "line": 1,
                    "message": (
                        f"Change based on {merge_base[:11]} has a merge conflict "
                        f"with upstream `origin/main` ({origin_main[:11]}) in "
                        f"{files_desc}{detail_desc}, which causes Gerrit CQ "
                        "(`checkout|jiri patch`) to fail across all builders "
                        "with `Failed to rebase`."
                    ),
                    "remediation": (
                        "Rebase the change onto the latest `origin/main` so Gerrit "
                        "CQ can patch it cleanly: run `git fetch origin main && "
                        "git rebase origin/main` (commit or `git stash` any "
                        "uncommitted edits first), resolve each conflicted file "
                        "keeping both `origin/main`'s entries and this change's "
                        "additions in sorted order without duplicate entries, "
                        "stage the resolved file(s) with `git add <file>`, and "
                        "finish with `git -c core.editor=true rebase --continue` "
                        "(preserving the existing `Change-Id` footer)."
                    ),
                })
            else:
                mt_lines = [l.strip() for l in (mt_proc.stdout or "").splitlines() if l.strip()]
                merged_tree = (
                    mt_lines[0]
                    if mt_lines and re.fullmatch(r"[0-9a-f]{40}", mt_lines[0])
                    else ""
                )
                if merged_tree:
                    upstream_touched = set(
                        git_lines(["diff", "--name-only", merge_base, origin_main])
                    )
                    for rel_p in sorted(changed_files & upstream_touched):
                        if not (
                            rel_p.endswith((".gn", ".gni"))
                            or os.path.basename(rel_p) in ("BUILD.bazel", "BUILD")
                        ):
                            continue
                        merged_txt = git_out(["show", f"{merged_tree}:{rel_p}"])
                        if not merged_txt:
                            continue
                        if rel_p.endswith((".gn", ".gni")):
                            m_gn_dups = find_gn_duplicate_list_assignments(merged_txt)
                            if m_gn_dups and not worktree_gn_dups.get(rel_p):
                                var_name, prev_ln, cur_ln = m_gn_dups[0]
                                findings.append({
                                    "source": "cq_reachability",
                                    "category": "upstream_rebase_conflict",
                                    "severity": "error",
                                    "file": rel_p,
                                    "line": cur_ln,
                                    "message": (
                                        f"3-way merge of '{rel_p}' with upstream `origin/main` "
                                        f"({origin_main[:11]}) produces duplicate non-empty "
                                        f"`{var_name} = [...]` assignments (lines {prev_ln} and "
                                        f"{cur_ln}), which causes Gerrit CQ (`gn gen`) to fail "
                                        "with `Replacing nonempty list`."
                                    ),
                                    "remediation": (
                                        "Rebase onto the latest `origin/main` (`git fetch origin "
                                        "main && git rebase origin/main`) and deduplicate the "
                                        f"`{var_name} = [...]` assignment in '{rel_p}' so "
                                        f"`{var_name}` is assigned only once."
                                    ),
                                })
                        else:
                            m_bz_dups = find_bazel_duplicate_targets(merged_txt, rel_p)
                            if m_bz_dups and not worktree_bz_dups.get(rel_p):
                                t_name, rule_name, prev_ln, cur_ln = m_bz_dups[0]
                                findings.append({
                                    "source": "cq_reachability",
                                    "category": "upstream_rebase_conflict",
                                    "severity": "error",
                                    "file": rel_p,
                                    "line": cur_ln,
                                    "message": (
                                        f"3-way merge of '{rel_p}' with upstream `origin/main` "
                                        f"({origin_main[:11]}) produces duplicate "
                                        f"`{rule_name}(name = \"{t_name}\")` definitions "
                                        f"(lines {prev_ln} and {cur_ln})."
                                    ),
                                    "remediation": (
                                        "Rebase onto the latest `origin/main` (`git fetch origin "
                                        "main && git rebase origin/main`) and merge the duplicate "
                                        f"`{rule_name}(name = \"{t_name}\")` blocks in '{rel_p}' "
                                        "into a single target definition."
                                    ),
                                })

# Check whether HEAD is stacked on a local ancestor commit that has already landed on
# origin/main, or still carries a 'Depends on:' reference to a CL now in origin/main.
def check_landed_ancestor_commits(merge_base, origin_main, parent_sha, head_sha):
    landed_ancestors = []
    upstream_log = ""
    if merge_base and merge_base != origin_main:
        upstream_log = git_out(
            ["log", "--format=%H%n%B%n---END---", f"{merge_base}..{origin_main}"]
        )
    if (
        merge_base
        and parent_sha
        and merge_base != origin_main
        and merge_base != parent_sha
        and is_ancestor(merge_base, parent_sha)
    ):
        upstream_cids = set(
            re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", upstream_log, re.M)
        )
        cherry_lines = git_lines(["cherry", origin_main, parent_sha, merge_base])
        cherry_landed = {
            line[2:].strip() for line in cherry_lines if line.startswith("- ")
        }
        for anc in git_lines(["rev-list", f"{merge_base}..{parent_sha}"]):
            anc_msg = git_out(["log", "-1", "--format=%B", anc])
            anc_cids = set(
                re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", anc_msg, re.M)
            )
            if anc in cherry_landed or (anc_cids & upstream_cids):
                anc_subj = (
                    anc_msg.splitlines()[0] if anc_msg.splitlines() else anc[:11]
                )
                landed_ancestors.append((anc[:11], anc_subj))

    head_msg = git_out(["log", "-1", "--format=%B", head_sha]) if head_sha else ""
    if re.search(r"(?m)^Reviewed-on:\s+\S+", head_msg) and not head_touches_target:
        return
    head_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", head_msg, re.M))
    stale_depends_on = []
    unlanded_depends_on = []
    # Extract only explicit 'Depends on:' / 'Depends-on:' lines and bulleted/indented blocks
    # so Gerrit 'Reviewed-on:' footers or prose URLs in the commit body are not misread.
    dep_cont_re = (
        r"(?:[ \t]*[-*].*|[ \t]+\S.*"
        r"|(?:https?://|fxrev\.dev/|fxr/|I[0-9a-f]{40}\b).*)"
    )
    dep_section_re = re.compile(
        r"(?im)^Depends[- ]on:[ \t]*([^\n]*)"
        r"(?:\n((?:" + dep_cont_re + r"(?:\n|$))+))?"
    )
    dep_text = "\n".join(
        f"{m.group(1) or ''}\n{m.group(2) or ''}"
        for m in dep_section_re.finditer(head_msg)
    )
    dep_cl_re = (
        r"(?:fxrev\.dev|fxr|fuchsia-review\.googlesource\.com/c/fuchsia/\+)/(\d+)"
    )
    for cl_num in re.findall(dep_cl_re, dep_text):
        rev_pat = r"Reviewed-on:[^\n]*/\+/" + re.escape(cl_num) + r"\b"
        ref = f"fxrev.dev/{cl_num}"
        if re.search(rev_pat, upstream_log) or git_out(
            [
                "log",
                "-1",
                "--format=%H",
                f"--grep=Reviewed-on: .*/+/{cl_num}\\b",
                origin_main,
            ]
        ):
            if ref not in stale_depends_on:
                stale_depends_on.append(ref)
        elif ref not in unlanded_depends_on:
            unlanded_depends_on.append(ref)

    # Check Change-Id references in 'Depends on:' sections against origin/main.
    for cid in re.findall(r"\b(I[0-9a-f]{40})\b", dep_text):
        if cid in head_cids:
            continue
        landed_sha = git_out(
            ["log", "-1", "--format=%H", f"--grep=^Change-Id: {cid}$", origin_main]
        )
        if landed_sha:
            if cid not in stale_depends_on:
                stale_depends_on.append(cid)
        elif cid not in unlanded_depends_on:
            unlanded_depends_on.append(cid)

    if landed_ancestors or stale_depends_on:
        anc_desc = ", ".join(f"{sha} ({subj!r})" for sha, subj in landed_ancestors)
        dep_desc = ", ".join(stale_depends_on)
        msg_parts = []
        if landed_ancestors:
            msg_parts.append(
                f"HEAD is stacked on local ancestor commit(s) {anc_desc} that have "
                f"already landed on `origin/main` ({origin_main[:11]}), leaving the "
                "branch diverged from `origin/main` on a closed prerequisite change."
            )
        if stale_depends_on:
            msg_parts.append(
                f"Commit message still lists `Depends on:` {dep_desc}, which has "
                "already landed on `origin/main`."
            )
        rem_parts = []
        if landed_ancestors:
            rem_parts.append(
                "Run `git fetch origin main && git rebase origin/main` so git drops the "
                "already-landed prerequisite commit(s) and rebases onto `origin/main`."
            )
        if stale_depends_on:
            if unlanded_depends_on:
                keep_desc = ", ".join(unlanded_depends_on)
                rem_parts.append(
                    f"Remove the landed `Depends on:` entry/entries ({dep_desc}) from "
                    f"the commit message with `git commit --amend` while keeping still-"
                    f"unmerged `Depends on:` entry/entries ({keep_desc}), any local "
                    "unmerged prerequisite ancestor commit(s), and the existing "
                    "`Change-Id`."
                )
            else:
                rem_parts.append(
                    f"Remove the obsolete `Depends on:` block ({dep_desc}) from the "
                    "commit message with `git commit --amend` (preserving the existing "
                    "`Change-Id`)."
                )
        findings.append({
            "source": "cq_reachability",
            "category": "stale_landed_parent_commit",
            "severity": "error",
            "file": "/COMMIT_MSG",
            "line": 1,
            "message": " ".join(msg_parts),
            "remediation": " ".join(rem_parts),
        })


# Check that in-tree Bazel dependency packages exist on disk and on origin/main when standalone.
def is_non_dependency_bazel_label(val):
    if (
        not val.startswith("//")
        or val.startswith(
            (
                "//build",
                "//tools",
                "//conditions:",
                "//visibility:",
                "//command_line_option:",
            )
        )
        or val.endswith((":__pkg__", ":__subpackages__"))
        or bool(re.search(r"\s", val))
    ):
        return True
    return False


def collect_non_dep_ast_node_ids(tree):
    non_dep_attrs = (
        "visibility",
        "default_visibility",
        "copts",
        "cxxopts",
        "conlyopts",
        "linkopts",
        "rustc_flags",
        "args",
        "cmd",
        "cmd_bash",
    )
    node_ids = set()
    for call_node in ast.walk(tree):
        if isinstance(call_node, ast.Call):
            for kw in call_node.keywords:
                if kw.arg in non_dep_attrs:
                    for sub in ast.walk(kw.value):
                        node_ids.add(id(sub))
    return node_ids


def check_unlanded_bazel_dependencies(origin_main, parent_sha):
    if not origin_main or not head_is_task_commit:
        return
    is_standalone_on_origin = parent_sha == origin_main or is_ancestor(
        parent_sha, origin_main
    )
    checked_packages = set()
    for td in target_dirs:
        bazel_rel = os.path.join(td, "BUILD.bazel")
        bazel_full = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(bazel_full):
            continue
        try:
            with open(bazel_full, "r", encoding="utf-8") as f:
                bazel_src = f.read()
            tree = ast.parse(bazel_src, filename=bazel_rel)
        except Exception:
            continue

        non_dep_nodes = collect_non_dep_ast_node_ids(tree)

        for node in ast.walk(tree):
            if id(node) in non_dep_nodes:
                continue
            if not isinstance(node, ast.Constant) or not isinstance(node.value, str):
                continue
            val = node.value.strip()
            if is_non_dependency_bazel_label(val):
                continue
            parts = split_label(val, td)
            if not parts:
                continue
            dep_pkg, _ = parts
            if (
                not dep_pkg
                or dep_pkg in ("conditions", "visibility", "command_line_option")
                or dep_pkg == td
                or dep_pkg in target_dirs
                or dep_pkg in checked_packages
            ):
                continue
            checked_packages.add(dep_pkg)

            # Check if this in-tree package exists on disk
            dep_bazel_rel = os.path.join(dep_pkg, "BUILD.bazel")
            dep_build_rel = os.path.join(dep_pkg, "BUILD")
            on_disk = os.path.isfile(
                os.path.join(workdir, dep_bazel_rel)
            ) or os.path.isfile(os.path.join(workdir, dep_build_rel))
            if not on_disk:
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_bazel_dependency_package",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target in '{bazel_rel}' depends on '{val}', but package "
                        f"directory '{dep_pkg}' does not contain a BUILD.bazel or "
                        "BUILD file."
                    ),
                    "remediation": (
                        f"Fix the dependency label or create the missing "
                        f"'{dep_bazel_rel}' file."
                    ),
                })
                continue

            # Skip dependency packages added/modified in HEAD (co-migrated in this CL)
            # or residing in a nested jiri git repository / submodule outside the root repo.
            dep_in_head = (
                dep_bazel_rel in changed_files
                or dep_build_rel in changed_files
                or git_success(["cat-file", "-e", f"{head_sha}:{dep_bazel_rel}"])
                or git_success(["cat-file", "-e", f"{head_sha}:{dep_build_rel}"])
            )
            if dep_in_head:
                continue

            dep_abs_dir = os.path.join(workdir, dep_pkg)
            try:
                dep_top = subprocess.check_output(
                    ["git", "-C", dep_abs_dir, "rev-parse", "--show-toplevel"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                ).strip()
            except Exception:
                dep_top = ""
            if dep_top and os.path.realpath(dep_top) != os.path.realpath(workdir):
                continue

            # Check if this in-tree package exists on origin/main
            dep_on_origin = git_success(
                ["cat-file", "-e", f"{origin_main}:{dep_bazel_rel}"]
            ) or git_success(["cat-file", "-e", f"{origin_main}:{dep_build_rel}"])
            if dep_on_origin:
                continue

            # If it does not exist on origin/main and HEAD is parented on origin/main:
            # Pushing standalone to refs/for/main will fail Tricium.
            if is_standalone_on_origin:
                findings.append({
                    "source": "cq_reachability",
                    "category": "unlanded_bazel_dependency_package",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target in '{bazel_rel}' depends on in-tree package "
                        f"'//{dep_pkg}', but '{dep_bazel_rel}' does not exist on "
                        "'origin/main', and HEAD is parented directly on origin/main. "
                        "Uploading or dry-running this CL standalone will fail Tricium "
                        f"with 'ERROR: no such package \"{dep_pkg}\"'. Wait for the "
                        f"prerequisite CL migrating '//{dep_pkg}' to land on "
                        "origin/main, or stack this change on top of it."
                    ),
                    "remediation": (
                        f"Ensure the prerequisite migration CL for '//{dep_pkg}' lands "
                        "on 'origin/main' before pushing or running CQ dry runs "
                        "standalone. If developing stacked changes, ensure this commit "
                        "is stacked on top of the prerequisite commit and includes "
                        "'Depends on:' in the commit message."
                    ),
                })


if origin_main and head_is_task_commit:
    check_landed_ancestor_commits(
        git_out(["merge-base", origin_main, head_sha]),
        origin_main,
        parent_sha,
        head_sha,
    )
    check_unlanded_bazel_dependencies(origin_main, parent_sha)


# Check that referenced workspace Bazel packages and targets exist in origin/main or the top commit
# when HEAD is the task's own commit (PLANTER_CHANGE_BASE != HEAD).
def check_unmerged_parent_bazel_package_deps():
    if change_base == "HEAD" or not (
        origin_main and head_sha and parent_sha and base_sha and base_sha != head_sha
    ):
        return
    stack_base = git_out(["merge-base", origin_main, head_sha])
    if not (
        stack_base
        and parent_sha != stack_base
        and is_ancestor(stack_base, parent_sha)
    ):
        return
    anc_files = set(git_lines(["diff", "--name-only", stack_base, "HEAD~1"]))
    top_files = set(git_lines(["diff", "--name-only", "HEAD~1", "HEAD"]))
    top_files.update(git_lines(["diff", "--name-only", "HEAD"]))
    top_files.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
    scan_dirs = list(target_dirs)
    for rel_f in sorted(top_files):
        if os.path.basename(rel_f) in ("BUILD.bazel", "BUILD"):
            pkg_d = os.path.dirname(rel_f).strip("/")
            if pkg_d and pkg_d not in scan_dirs:
                scan_dirs.append(pkg_d)
    for td in scan_dirs:
        bazel_rel = os.path.join(td, "BUILD.bazel")
        bazel_full = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(bazel_full):
            continue
        try:
            with open(bazel_full, "r", encoding="utf-8") as f:
                td_tree = ast.parse(f.read(), filename=bazel_rel)
        except Exception:
            continue
        vis_nodes = set()
        for call_node in ast.walk(td_tree):
            if isinstance(call_node, ast.Call):
                for kw in call_node.keywords:
                    if kw.arg == "visibility" and kw.value is not None:
                        for sub in ast.walk(kw.value):
                            vis_nodes.add(id(sub))
        missing_pkgs = {}
        for node in ast.walk(td_tree):
            if id(node) in vis_nodes:
                continue
            if not (isinstance(node, ast.Constant) and isinstance(node.value, str)):
                continue
            s = re.sub(r"^@@?//", "//", node.value.strip())
            if not s.startswith("//") or s.startswith(("//visibility:", "//conditions:")):
                continue
            parts = split_label(s, td)
            if not parts:
                continue
            dep_pkg, dep_target = parts
            if (
                not dep_pkg
                or dep_pkg == td
                or dep_pkg in missing_pkgs
                or dep_target in ("__pkg__", "__subpackages__")
            ):
                continue
            dep_bazel = f"{dep_pkg}/BUILD.bazel"
            dep_build = f"{dep_pkg}/BUILD"
            if not (
                os.path.isfile(os.path.join(workdir, dep_bazel))
                or os.path.isfile(os.path.join(workdir, dep_build))
            ):
                continue
            in_origin = (
                git_out(["cat-file", "-t", f"{origin_main}:{dep_bazel}"]) == "blob"
                or git_out(["cat-file", "-t", f"{origin_main}:{dep_build}"]) == "blob"
            )
            in_top = dep_bazel in top_files or dep_build in top_files
            if not in_origin and not in_top and (dep_bazel in anc_files or dep_build in anc_files):
                missing_pkgs[dep_pkg] = getattr(node, "lineno", 1)
            elif (
                in_origin
                and not in_top
                and dep_target
                and (dep_bazel in anc_files or dep_build in anc_files)
            ):
                orig_blob = git_out(["show", f"{origin_main}:{dep_bazel}"]) or git_out(
                    ["show", f"{origin_main}:{dep_build}"]
                )
                name_pat = r'\bname\s*=\s*"' + re.escape(dep_target) + r'"'
                if orig_blob and not re.search(name_pat, orig_blob):
                    missing_pkgs[dep_pkg] = getattr(node, "lineno", 1)
        for dep_pkg, lineno in sorted(missing_pkgs.items()):
            msg = (
                f"'{bazel_rel}' depends on `//{dep_pkg}`, whose `BUILD.bazel` does not "
                f"exist in `origin/main` ({origin_main[:11]}) and is only present in an "
                f"unmerged local ancestor commit (`{stack_base[:11]}..HEAD~1`). Because "
                "Gerrit CQ tests the uploaded commit against `origin/main`, Bazel cquery "
                f"(`tests.json` generation) fails in CQ with `no such package '{dep_pkg}': "
                "BUILD file not found`."
            )
            rem = (
                f"Include `{dep_pkg}/BUILD.bazel` (and `{dep_pkg}/BUILD.gn` if modified) in "
                "the top commit `HEAD` so the uploaded CL is self-contained on `origin/main`: "
                "fold the unmerged ancestor commit(s) into `HEAD` with "
                '`git reset --soft "$(git merge-base origin/main HEAD)" && '
                "git commit --amend --no-edit` (removing any `Depends on:` body lines for "
                "the folded task, adding its targets to `Bazel-Migration-Target:` footers, "
                "and listing its files in `modified_files`)."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "unmerged_parent_bazel_package_dep",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg,
                "remediation": rem,
            })


# Disabled: stacked tasks with unlanded dependencies are uploaded as WIP and gated
# by planter until their parent dependency CLs land. Folding HEAD~1 into HEAD
# duplicates the parent CL's BUILD.bazel/BUILD.gn into the child CL and causes
# guaranteed add/add merge conflicts when the parent CL lands (see
# stacked_bazel_dependency_package below).
# check_unmerged_parent_bazel_package_deps()

# Check that every main-repo Bazel package referenced (directly or transitively)
# in BUILD.bazel dependency attributes has a BUILD.bazel or BUILD file in the
# checkout, and note when a dependency package is added or updated by an unmerged
# stacked parent commit above origin/main (using 3-dot diff against the task's
# parent commit so stacked ancestor commits are distinguished from HEAD even when
# PLANTER_CHANGE_BASE points to the base of the stack).
task_parent_ref = (
    "HEAD~1"
    if (
        parent_sha
        and head_touches_target
        and not (wt_touches_target and not head_touches_target)
    )
    else effective_diff_base
)
task_changed_files = (
    (set(head_commit_files) | set(wt_changed_files))
    if task_parent_ref == "HEAD~1"
    else set(changed_files)
)
stacked_parent_files = (
    set(git_lines(["diff", "--name-only", f"{origin_main}...{task_parent_ref}"]))
    if origin_main and task_parent_ref and origin_main != task_parent_ref
    else set()
)
head_commit_msg = git_out(["log", "-1", "--format=%B", "HEAD"]) if head_sha else ""

DEP_ATTRS = {
    "actual",
    "deps",
    "implementation_deps",
    "link_deps",
    "non_test_deps",
    "test_deps",
    "proc_macro_deps",
    "data",
    "test_data",
    "tools",
    "unit_tests",
    "binary",
    "crate",
    "package",
    "compiled_manifest",
    "manifest",
    "includes",
    "tests",
    "lint_config",
    "components",
    "test_components",
    "subpackages",
    "resources",
    "target_compatible_with",
    "exec_compatible_with",
}


def has_bazel_pkg_in_worktree(pkg):
    if not pkg:
        return True
    pkg_dir = os.path.join(workdir, pkg)
    for bfile in ("BUILD.bazel", "BUILD"):
        if os.path.isfile(os.path.join(pkg_dir, bfile)):
            return True
    return False


def has_bazel_pkg_at_ref(pkg, ref):
    if not pkg or not ref:
        return True
    for bfile in ("BUILD.bazel", "BUILD"):
        rc = subprocess.run(
            ["git", "-C", workdir, "cat-file", "-e", f"{ref}:{pkg}/{bfile}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        if rc == 0:
            return True
    return False


def iter_dep_label_strings(val_node, var_defs=None, seen_vars=None):
    if isinstance(val_node, ast.Constant) and isinstance(val_node.value, str):
        yield val_node.value, getattr(val_node, "lineno", 1)
    elif isinstance(val_node, (ast.List, ast.Tuple)):
        for elt in val_node.elts:
            yield from iter_dep_label_strings(elt, var_defs, seen_vars)
    elif isinstance(val_node, ast.BinOp) and isinstance(val_node.op, ast.Add):
        yield from iter_dep_label_strings(val_node.left, var_defs, seen_vars)
        yield from iter_dep_label_strings(val_node.right, var_defs, seen_vars)
    elif isinstance(val_node, ast.Call):
        for arg in val_node.args:
            if isinstance(arg, ast.Dict):
                for k in arg.keys:
                    if (
                        isinstance(k, ast.Constant)
                        and isinstance(k.value, str)
                        and not k.value.startswith("//conditions:")
                    ):
                        yield k.value, getattr(k, "lineno", 1)
                for v in arg.values:
                    yield from iter_dep_label_strings(v, var_defs, seen_vars)
    elif isinstance(val_node, ast.Name) and var_defs and val_node.id in var_defs:
        seen = seen_vars or set()
        if val_node.id not in seen:
            for item, _ in iter_dep_label_strings(
                var_defs[val_node.id], var_defs, seen | {val_node.id}
            ):
                yield item, getattr(val_node, "lineno", 1)


def iter_pkg_dep_labels(cur_tree):
    var_defs = {}
    for stmt in getattr(cur_tree, "body", []):
        if isinstance(stmt, ast.Assign):
            for t in stmt.targets:
                if isinstance(t, ast.Name):
                    var_defs[t.id] = stmt.value
    for node in ast.walk(cur_tree):
        if not isinstance(node, ast.Call):
            continue
        if (
            isinstance(node.func, ast.Name)
            and node.func.id == "load"
            and node.args
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
        ):
            yield node.args[0].value, getattr(node.args[0], "lineno", 1)
            continue
        for kw in node.keywords:
            if kw.arg in DEP_ATTRS:
                yield from iter_dep_label_strings(kw.value, var_defs)


def check_missing_bazel_dep_packages(bazel_rel, td, tree):
    reported_pkgs = set()
    visited_pkgs = {td}
    queue = [(bazel_rel, td, tree, None)]
    while queue:
        cur_rel, cur_pkg, cur_tree, root_line = queue.pop(0)
        for raw_label, ln in iter_pkg_dep_labels(cur_tree):
            parsed = split_label(raw_label, cur_pkg)
            if not parsed:
                continue
            dep_pkg, _ = parsed
            if not dep_pkg or dep_pkg == cur_pkg:
                continue
            report_ln = root_line if root_line is not None else ln
            if not has_bazel_pkg_in_worktree(dep_pkg):
                if dep_pkg not in reported_pkgs:
                    reported_pkgs.add(dep_pkg)
                    msg = (
                        f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                        f"package `//{dep_pkg}`, but neither '{dep_pkg}/BUILD.bazel' "
                        f"nor '{dep_pkg}/BUILD' exists in the checkout, causing CQ "
                        "`tests.json` generation (`bazel query`) to fail with "
                        f"`no such package '{dep_pkg}': BUILD file not found`."
                    )
                    rem = (
                        f"Migrate `//{dep_pkg}` to Bazel in this change (or ensure "
                        "its stacked migration commit remains in the git parent "
                        "ancestry before `HEAD` when rebasing onto `origin/main`)."
                    )
                    findings.append({
                        "source": "cq_reachability",
                        "category": "missing_bazel_dependency_package",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": report_ln,
                        "message": msg,
                        "remediation": rem,
                    })
                continue
            in_stacked_parent = (
                f"{dep_pkg}/BUILD.bazel" in stacked_parent_files
                or f"{dep_pkg}/BUILD" in stacked_parent_files
                or (origin_main and not has_bazel_pkg_at_ref(dep_pkg, origin_main))
            )
            if (
                origin_main
                and f"{dep_pkg}/BUILD.bazel" not in task_changed_files
                and f"{dep_pkg}/BUILD" not in task_changed_files
                and in_stacked_parent
                and dep_pkg not in reported_pkgs
            ):
                reported_pkgs.add(dep_pkg)
                msg = (
                    f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                    f"package `//{dep_pkg}`, whose `BUILD.bazel` is provided by a "
                    "stacked parent commit above `origin/main` rather than "
                    "`origin/main` itself."
                )
                rem = (
                    f"Keep the stacked parent commit that provides '{dep_pkg}/BUILD.bazel' "
                    "in the git ancestry between `origin/main` and `HEAD` (when rebasing, "
                    "rebase the full stack rather than dropping parent commits with "
                    "`git rebase --onto origin/main HEAD~1`, and if a prior CQ run failed "
                    f"with `no such package '{dep_pkg}'` before the stack was re-cherry-picked, "
                    f"do not duplicate '{dep_pkg}/BUILD.bazel' into `HEAD` or add entries to "
                    "`disputed_findings`), or include "
                    f"'{dep_pkg}/BUILD.bazel' in this change if the parent CL is not "
                    "stacked in Gerrit, so CQ `tests.json` (`bazel query`) resolves "
                    f"`//{dep_pkg}`."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "stacked_bazel_dependency_package",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": report_ln,
                    "message": msg,
                    "remediation": rem,
                })
            elif (
                origin_main
                and f"{dep_pkg}/BUILD.bazel" not in task_changed_files
                and f"{dep_pkg}/BUILD" not in task_changed_files
                and not in_stacked_parent
                and dep_pkg not in reported_pkgs
                and head_commit_msg
                and (
                    re.search(
                        r"(?<![A-Za-z0-9_-])"
                        + re.escape(dep_pkg.replace("/", "-").replace("_", "-"))
                        + r"(?![A-Za-z0-9_/-])",
                        head_commit_msg.replace("_", "-"),
                    )
                    or re.search(
                        r"//" + re.escape(dep_pkg) + r"(?![A-Za-z0-9_/-])",
                        head_commit_msg,
                    )
                )
            ):
                reported_pkgs.add(dep_pkg)
                msg = (
                    f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                    f"package `//{dep_pkg}`, which is listed in `Depends on:` and "
                    "has now landed in `origin/main` and `HEAD~1`."
                )
                rem = (
                    f"'{dep_pkg}/BUILD.bazel' is now present in `HEAD~1` after rebasing onto "
                    "`origin/main` (resolving any earlier CQ `no such package` failure from "
                    "a pre-rebase patchset); run `run_checks.sh` once and report with "
                    "`disputed_findings: []` so the rebased patchset is uploaded."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "rebased_bazel_dependency_package",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": report_ln,
                    "message": msg,
                    "remediation": rem,
                })
            if dep_pkg not in visited_pkgs and (
                f"{dep_pkg}/BUILD.bazel" in task_changed_files
                or f"{dep_pkg}/BUILD" in task_changed_files
                or in_stacked_parent
            ):
                visited_pkgs.add(dep_pkg)
                for bfile in ("BUILD.bazel", "BUILD"):
                    dep_rel = os.path.join(dep_pkg, bfile)
                    dep_full = os.path.join(workdir, dep_rel)
                    if os.path.isfile(dep_full):
                        try:
                            with open(dep_full, "r", encoding="utf-8") as df:
                                dep_tree = ast.parse(df.read(), filename=dep_rel)
                            queue.append((dep_rel, dep_pkg, dep_tree, report_ln))
                        except Exception:
                            pass
                        break


# Verify that any modified non-target BUILD.bazel (such as an ancestor test_suite
# BUILD.bazel) does not reference a non-existent Bazel package in the worktree.
def check_modified_ancestor_bazel_dep_packages():
    target_set = set(target_dirs)
    for rel_f in sorted(task_changed_files):
        if os.path.basename(rel_f) not in ("BUILD.bazel", "BUILD"):
            continue
        pkg_d = os.path.dirname(rel_f).strip("/")
        if pkg_d in target_set:
            continue
        full_f = os.path.join(workdir, rel_f)
        if not os.path.isfile(full_f):
            continue
        try:
            with open(full_f, "r", encoding="utf-8") as f:
                anc_tree = ast.parse(f.read(), filename=rel_f)
        except Exception:
            continue
        seen_missing = set()
        for raw_label, ln in iter_pkg_dep_labels(anc_tree):
            parsed = split_label(raw_label, pkg_d)
            if not parsed:
                continue
            dep_pkg, _ = parsed
            if (
                not dep_pkg
                or dep_pkg == pkg_d
                or dep_pkg in seen_missing
                or has_bazel_pkg_in_worktree(dep_pkg)
            ):
                continue
            seen_missing.add(dep_pkg)
            findings.append({
                "source": "cq_reachability",
                "category": "missing_bazel_dependency_package",
                "severity": "error",
                "file": rel_f,
                "line": ln,
                "message": (
                    f"Dependency `{raw_label}` in '{rel_f}' references Bazel "
                    f"package `//{dep_pkg}`, but neither '{dep_pkg}/BUILD.bazel' "
                    f"nor '{dep_pkg}/BUILD' exists in the checkout, causing CQ "
                    "`tests.json` generation (`bazel query`) to fail with "
                    f"`no such package '{dep_pkg}': BUILD file not found`."
                ),
                "remediation": (
                    f"Remove the stale `{raw_label}` entry from '{rel_f}', or "
                    f"ensure '{dep_pkg}/BUILD.bazel' is present in the checkout "
                    "(or in a stacked parent commit before `HEAD`)."
                ),
            })


check_modified_ancestor_bazel_dep_packages()


# Check that component manifest includes (and other `*.shard` labels) name a target the
# owning package declares. A package that exists but lacks the cml_shard passes the package
# check above, yet `fx bazel build` fails analysis with `no such target '<pkg>:<x>.shard'`.
def iter_shard_labels(tree):
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for kw in node.keywords:
            if kw.arg not in DEP_ATTRS:
                continue
            for raw_label, ln in iter_dep_label_strings(kw.value):
                name_part = raw_label.rpartition(":")[2]
                if kw.arg == "includes" or name_part.endswith(".shard"):
                    yield raw_label, ln


def bazel_pkg_declares(pkg, name):
    """True when <pkg>/BUILD(.bazel) quotes "<name>" (target or exports_files) or it is a file."""
    if os.path.isfile(os.path.join(workdir, pkg, name)):
        return True
    for bfile in ("BUILD.bazel", "BUILD"):
        full = os.path.join(workdir, pkg, bfile)
        if os.path.isfile(full):
            try:
                with open(full, "r", encoding="utf-8") as bf:
                    return f'"{name}"' in bf.read()
            except Exception:
                return True
    return True  # A missing package is reported as missing_bazel_dependency_package.


def check_missing_shard_targets(bazel_rel, td, tree):
    reported = set()
    for raw_label, ln in iter_shard_labels(tree):
        parsed = split_label(raw_label, td)
        if not parsed or not parsed[0] or not parsed[1]:
            continue
        dep_pkg, name = parsed
        if (dep_pkg, name) in reported or bazel_pkg_declares(dep_pkg, name):
            continue
        reported.add((dep_pkg, name))
        shard_src = f"{name}.cml" if name.endswith(".shard") else "<stem>.shard.cml"
        msg = (
            f"`{raw_label}` in '{bazel_rel}' names target `{name}`, which "
            f"'{dep_pkg}/BUILD.bazel' does not declare, so `fx bazel build` fails analysis "
            f"with `no such target '//{dep_pkg}:{name}'`."
        )
        rem = (
            f"Add `cml_shard(name = \"{name}\", shard = \"{shard_src}\", "
            "includes = [<cml_shard targets of the shards it includes>], "
            "visibility = [...])` (load from "
            "`//build/bazel/rules/components:cml_shard.bzl`) to "
            f"'{dep_pkg}/BUILD.bazel' as part of this change, the same way a dependency "
            "without a Bazel target is migrated. If another agent owns that file in this "
            "task, ask for that exact edit and keep the label; do not drop the include."
        )
        findings.append({
            "source": "cq_reachability",
            "category": "missing_bazel_dependency_target",
            "severity": "error",
            "file": bazel_rel,
            "line": ln,
            "message": msg,
            "remediation": rem,
        })


for _td in target_dirs:
    _bz_rel = os.path.join(_td, "BUILD.bazel")
    try:
        with open(os.path.join(workdir, _bz_rel), "r", encoding="utf-8") as _f:
            check_missing_shard_targets(_bz_rel, _td, ast.parse(_f.read(), filename=_bz_rel))
    except Exception:
        pass

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

    check_missing_bazel_dep_packages(bazel_rel, td, tree)

    gn_src = ""
    if os.path.isfile(gn_full):
        try:
            with open(gn_full, "r", encoding="utf-8") as f:
                gn_src = f.read()
        except Exception:
            gn_src = ""

    has_explicit_rustc_test = any(
        isinstance(n, ast.Call)
        and (
            (isinstance(n.func, ast.Name) and n.func.id in ("rustc_test", "host_rustc_test"))
            or (isinstance(n.func, ast.Attribute) and n.func.attr in ("rustc_test", "host_rustc_test"))
        )
        for n in ast.walk(tree)
    )
    base_gn_ref = change_base if change_base != "HEAD" else "HEAD~1"
    base_gn_src = git_out(["show", f"{base_gn_ref}:{gn_rel}"]) if gn_rel else ""

    test_targets = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = ""
        if isinstance(node.func, ast.Name):
            rule_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule_name = node.func.attr
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        t_name = ""
        call_lineno = getattr(node, "lineno", 1)
        lineno = call_lineno
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                t_name = kw.value.value
                lineno = kw.lineno
        if not t_name:
            continue
        if rule_name in TEST_RULES:
            test_targets.append((t_name, rule_name, lineno, [call_lineno]))
        elif rule_name in RUST_TEST_OWNERS and unit_test_envs(kws) and not has_explicit_rustc_test:
            unit_t_name = t_name + "_test"
            has_gn_tests_group = bool(
                re.search(r'\bgroup\(\s*"tests"\s*\)|\bbazel_test_suite\(', gn_src)
                or re.search(r'":' + re.escape(unit_t_name) + r'(?:\(|")', base_gn_src)
            )
            if has_gn_tests_group:
                for kw in node.keywords:
                    if kw.arg in ("with_unit_tests", "with_host_unit_tests"):
                        test_targets.append(
                            (unit_t_name, f"{rule_name}({kw.arg})", kw.lineno, [call_lineno, kw.lineno])
                        )
                        break

    parent_gn = os.path.join(workdir, os.path.dirname(td), "BUILD.gn")
    parent_gn_src = ""
    if os.path.isfile(parent_gn):
        try:
            with open(parent_gn, "r", encoding="utf-8") as f:
                parent_gn_src = f.read()
        except Exception:
            parent_gn_src = ""

    def strip_bazel_test_suites(code):
        out = list(code)
        for m in SUITE_RE.finditer(code):
            end = closing(code, m.end() - 1)
            for idx in range(m.start(), min(end + 1, len(out))):
                if out[idx] != "\n":
                    out[idx] = " "
        return "".join(out)

    # Return sorted ancestor GN package directories whose BUILD.gn references pkg_dir:target_name
    # directly outside bazel_test_suite(...).
    # Visibility a test wired into an ancestor test_suite needs: only the package of that
    # test_suite. GN files that list the test (group("tests") in an ancestor BUILD.gn) refer to
    # the GN target, and bazel_test_suite builds the Bazel test as a top-level target, so neither
    # is a Bazel rdep; naming their packages would conflict with visibility_audit
    # (false_positive_rdep_visibility).
    def ancestor_suite_visibility(anc_pkg):
        return [f"//{anc_pkg}:__pkg__"]

    def direct_gn_test_callers(pkg_dir, target_name):
        callers = []
        anc = os.path.dirname(pkg_dir)
        while True:
            anc_rel = os.path.join(anc, "BUILD.gn") if anc else "BUILD.gn"
            anc_code = strip_bazel_test_suites(read_gn(anc_rel))
            if anc_code:
                rel_from_anc = os.path.relpath(pkg_dir, anc) if anc else pkg_dir
                label_re = (
                    r'"(?://'
                    + re.escape(pkg_dir)
                    + r"|"
                    + re.escape(rel_from_anc)
                    + r"):"
                    + re.escape(target_name)
                    + r'(?:\(|")'
                )
                if target_name == os.path.basename(pkg_dir):
                    label_re += (
                        r'|"(?://'
                        + re.escape(pkg_dir)
                        + r"|"
                        + re.escape(rel_from_anc)
                        + r')(?:\(|")'
                    )
                if re.search(label_re, anc_code):
                    callers.append(anc)
            if not anc:
                break
            anc = os.path.dirname(anc)
        return sorted(set(callers))

    # Check whether group("tests") in pkg_dir/BUILD.gn is referenced by any ancestor BUILD.gn
    # or any other .gn/.gni file in the checkout.
    def gn_tests_group_is_referenced(pkg_dir):
        if direct_gn_test_callers(pkg_dir, "tests"):
            return True
        abs_tests_re = re.compile(
            r'"//'
            + re.escape(pkg_dir)
            + (
                r'(?::tests)?(?:\([^"]*\))?"'
                if os.path.basename(pkg_dir) == "tests"
                else r':tests(?:\([^"]*\))?"'
            )
        )
        for cand_rel in git_lines(
            [
                "grep",
                "-l",
                "--fixed-strings",
                f"//{pkg_dir}",
                "--",
                "*.gn",
                "*.gni",
            ]
        ):
            if os.path.dirname(cand_rel).strip("/") == pkg_dir:
                continue
            if abs_tests_re.search(read_gn(cand_rel)):
                return True
        return False

    # Check whether test target t_name of pkg_dir is run by a GN bazel_test_suite that is itself
    # wired into a root test group (directly, or via Bazel test_suite targets such as an
    # ancestor's aggregate `host_tests` suite exported by `bazel_test_suite("bazel_tests")`).
    # This covers Bazel-only host tests (e.g. host tools bridged to GN with bazel_host_tool()
    # instead of bazel2gn), which intentionally have no GN test target or group("tests").
    def wired_via_bazel_test_suite(pkg_dir, t_name):
        local_suites = bazel_suites_of(pkg_dir)

        def expand(name, seen):
            if name in seen:
                return set()
            seen.add(name)
            if name not in local_suites:
                return {name}
            out = set()
            for m in local_suites[name] or []:
                ln = local_name(m, pkg_dir)
                if ln is not None:
                    out |= expand(ln, seen)
            return out

        for s_rel, s_name, _, _, labels in bazel_test_suites_for(pkg_dir):
            if not suite_is_wired(s_rel, s_name):
                continue
            for label in labels:
                m = label_target(label, pkg_dir)
                if m is not None:
                    found = {m}
                elif re.match(r"^@{0,2}//", label.strip()):
                    found = foreign_suite_tests(label, pkg_dir, pkg_dir, set())
                else:
                    continue
                names = set()
                for f_name in found:
                    names |= expand(f_name, set())
                if t_name in names:
                    return True
        return False

    def has_skip_above(lines, line_1based):
        idx = line_1based - 2
        while idx >= 0:
            s = lines[idx].strip() if idx < len(lines) else ""
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    return True
                idx -= 1
                continue
            break
        return False

    bazel_lines = bazel_src.splitlines()
    base_gn_clean = gn_code(base_gn_src) if base_gn_src else ""

    # Check whether a test target ran on host in base BUILD.gn (`:<name>($host_toolchain)`).
    def ran_on_host_in_base_gn(t_name):
        if not base_gn_clean:
            return False
        return bool(
            re.search(
                r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"',
                base_gn_clean,
            )
        )

    # Verify that unit tests and group("tests") present in base BUILD.gn are not dropped.
    def check_base_gn_test_preservation():
        if not base_gn_clean or not FX_TEST_AVAILABLE:
            return
        had_base_tests_group = bool(re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', base_gn_clean))
        has_now_tests_group = bool(
            gn_src and re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_code(gn_src))
        )
        all_tests_wired_in_anc_gn = bool(
            test_targets
            and not gn_tests_group_is_referenced(td)
            and all(direct_gn_test_callers(td, tn) for tn, _, _, _ in test_targets)
        )
        # Bazel-only tests (no GN test target left in BUILD.gn) reached through a wired
        # bazel_test_suite keep CI coverage, provided nothing still depends on `//td:tests`.
        all_tests_wired_via_bazel_suite = bool(
            test_targets
            and not gn_tests_group_is_referenced(td)
            and all(wired_via_bazel_test_suite(td, tn) for tn, _, _, _ in test_targets)
        )
        if (
            had_base_tests_group
            and gn_src
            and not has_now_tests_group
            and not all_tests_wired_in_anc_gn
            and not all_tests_wired_via_bazel_suite
        ):
            anc_info = (
                nearest_ancestor_test_suite(td, "target_tests")
                or nearest_ancestor_test_suite(td, "host_tests")
                or preferred_ancestor_area_suite(td, "target_tests")
                or preferred_ancestor_area_suite(td, "host_tests")
            )
            suite_guidance = (
                f"wire the migrated Bazel test targets into the ancestor `test_suite` "
                f"`//{anc_info[0]}:{anc_info[1]}` in '{anc_info[0]}/BUILD.bazel'"
                if anc_info
                else f"wire the migrated Bazel test targets into `bazel_test_suite(\"bazel_tests\")` in '{gn_rel}'"
            )
            msg_text = (
                f"'{gn_rel}' defined `group(\"tests\")` at `{change_base}`, "
                "but `group(\"tests\")` was removed during migration while callers or "
                "package tests still depend on it, dropping CI build and test verification."
            )
            rem_text = (
                f"Restore `group(\"tests\") {{ testonly = true deps = [ ... ] }}` "
                f"in '{gn_rel}' (or wire the generated GN test target directly into the "
                f"parent `BUILD.gn` `group(\"tests\")` when nothing references `//{td}:tests`) "
                f"and {suite_guidance}."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "dropped_tests_group",
                "severity": "error",
                "file": gn_rel,
                "line": 1,
                "message": msg_text,
                "remediation": rem_text,
            })
        for m_ut in re.finditer(
            r'\b(rustc_library|rustc_binary|rustc_proc_macro)\(\s*"([^"]+)"\s*\)\s*\{',
            base_gn_clean,
        ):
            lib_name = m_ut.group(2)
            lib_body = base_gn_clean[m_ut.end() : closing(base_gn_clean, m_ut.end() - 1)]
            if not re.search(r"\bwith_unit_tests\s*=\s*true\b", lib_body):
                continue
            ut_name = f"{lib_name}_test"
            if not re.search(r'":' + re.escape(ut_name) + r'(?:\(|")', base_gn_clean):
                continue
            if not test_targets:
                msg_text = (
                    f"`{m_ut.group(1)}(\"{lib_name}\")` in '{gn_rel}' had "
                    f"`with_unit_tests = true` (`:{ut_name}` wired into tests at "
                    f"`{change_base}`), but '{bazel_rel}' defines no unit test "
                    "target for it."
                )
                rem_text = (
                    f"Migrate `:{ut_name}` to '{bazel_rel}' (`rustc_test(name = "
                    f"\"{ut_name}\", crate = \":{lib_name}\")`, plus "
                    "`fx_unittest_package` for target tests and `wrap_host_rust_test` "
                    "when GN ran it on `$host_toolchain`) and export the Bazel test "
                    f"targets via `bazel_test_suite(\"bazel_tests\")` in '{gn_rel}'."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_migrated_tests",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": 1,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            elif ran_on_host_in_base_gn(ut_name) and not any(
                r in HOST_TEST_RULES for _, r, _, _ in test_targets
            ) and not re.search(r"\b(?:wrap_host_rust_test|host_rustc_test)\s*\(", bazel_src):
                msg_text = (
                    f"'{gn_rel}' ran `:{ut_name}($host_toolchain)` on host in CI "
                    f"at `{change_base}`, but '{bazel_rel}' does not declare "
                    f"`wrap_host_rust_test(name = \"{lib_name}_host_test\", "
                    f"binary_name = \"{ut_name}\")` (or `host_rustc_test`)."
                )
                rem_text = (
                    f"Add `wrap_host_rust_test(name = \"{lib_name}_host_test\", "
                    f"binary_name = \"{ut_name}\")` in '{bazel_rel}', export "
                    f"`\"//{td}:{lib_name}_host_test\"` in "
                    f"`bazel_test_suite(\"bazel_tests\") {{ host_tests = [...] }}` "
                    f"in '{gn_rel}', and keep '\":{ut_name}($host_toolchain)\"' in "
                    "`group(\"tests\")`."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_bazel_host_test_wrapper",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": 1,
                    "message": msg_text,
                    "remediation": rem_text,
                })

    check_base_gn_test_preservation()

    for t_name, rule_name, lineno, check_linenos in test_targets:
        # Check if this test target (or test attribute) has `# @bazel2gn:skip` in BUILD.bazel
        # (in which case bazel2gn does not generate a GN copy; migration_sanity.sh validates the skip).
        is_skipped = any(has_skip_above(bazel_lines, ln) for ln in check_linenos)
        m_sent = SENTINEL_RE.search(gn_src)
        if m_sent and not is_skipped:
            gn_pre_no_suites = strip_bazel_test_suites(gn_code(gn_src[: m_sent.start()]))
            gn_post_code = gn_code(gn_src[m_sent.end() :])
            ref_in_gn_pre = bool(re.search(r'":' + re.escape(t_name) + r'(?:\(|")', gn_pre_no_suites))
            needs_host_gn_ref = ran_on_host_in_base_gn(t_name) and not bool(
                re.search(r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"', gn_pre_no_suites)
            )
            ref_in_gn_post_pkg = (
                not needs_host_gn_ref
                and (rule_name in ("rustc_test", "cc_test") or rule_name.startswith("rustc_"))
                and bool(re.search(r'":' + re.escape(t_name) + r'"', gn_post_code))
            )
            ref_in_ancestor_gn = False
            anc = os.path.dirname(td)
            while True:
                anc_rel = os.path.join(anc, "BUILD.gn") if anc else "BUILD.gn"
                anc_code = strip_bazel_test_suites(read_gn(anc_rel))
                if anc_code:
                    rel_from_anc = os.path.relpath(td, anc) if anc else td
                    label_re = r'"(?://' + re.escape(td) + r"|" + re.escape(rel_from_anc) + r"):" + re.escape(t_name) + r'(?:\(|")'
                    if t_name == os.path.basename(td):
                        # `gn format` shortens "dir/name:name" to "dir/name".
                        label_re += r'|"(?://' + re.escape(td) + r"|" + re.escape(rel_from_anc) + r')(?:\(|")'
                    if re.search(label_re, anc_code):
                        ref_in_ancestor_gn = True
                        break
                if not anc:
                    break
                anc = os.path.dirname(anc)
            if not (ref_in_gn_pre or ref_in_gn_post_pkg or ref_in_ancestor_gn):
                findings.append({
                    "source": "cq_reachability",
                    "category": "orphaned_test_target",
                    "severity": "error",
                    "file": gn_rel,
                    "line": 1,
                    "message": (
                        f"Generated GN test copy ':{t_name}' (from `{rule_name}` in '{bazel_rel}') below "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' is not wired into `group(\"tests\")` above the "
                        "sentinel (or an ancestor `BUILD.gn` `tests` group outside `bazel_test_suite`), so the "
                        "GN build of this test is dead code and never runs in CQ."
                    ),
                    "remediation": (
                        f"Add '\":{t_name}\"' (or '\":{t_name}($host_toolchain)\"' for a host test) to "
                        f"`group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' alongside `:bazel_tests`."
                    ),
                })
                continue
        if m_sent or rule_name in ("fx_unittest_package", "host_go_test", "host_rustc_test"):
            continue
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
    # Check whether a Rust device test binary is packaged in BUILD.bazel or a GN-kept test pkg.
    def is_rust_device_test_packaged(device_bin, call_node):
        call_seg = ast.get_source_segment(bazel_src, call_node) or ""
        if "HOST_OS_CONSTRAINTS" in call_seg or "HOST_CONSTRAINTS" in call_seg:
            return True
        bin_pat = (
            r'\bbinary\s*=\s*"(?://'
            + re.escape(td)
            + r')?:'
            + re.escape(device_bin)
            + r'"'
        )
        ut_pat = (
            r'\bunit_tests\s*=\s*\[[^\]]*"(?://'
            + re.escape(td)
            + r')?:'
            + re.escape(device_bin)
            + r'"'
        )
        if re.search(bin_pat, bazel_src) or re.search(ut_pat, bazel_src):
            return True
        m_s = SENTINEL_RE.search(gn_src)
        gn_pre_clean = gn_code(gn_src[: m_s.start()]) if m_s else gn_code(gn_src)
        gn_pkg_pat = (
            r"\b(?:fuchsia_unittest_package|fuchsia_test_package|"
            r"fuchsia_test_component|fuchsia_unittest_component)\s*\("
        )
        gn_ref_pat = (
            r'"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"'
        )
        return bool(
            re.search(gn_pkg_pat, gn_pre_clean)
            and re.search(gn_ref_pat, gn_pre_clean)
        )

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
            packaged_in_bin = re.search(
                r'\bbinary\s*=\s*"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"', bazel_src
            )
            packaged_in_unit_tests = re.search(
                r'\bunit_tests\s*=\s*\[[^\]]*"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"', bazel_src
            )
            packaged_in_gn_pkg = is_packaged_by_kept_gn_test_package(
                gn_src, td, device_bin
            )
            if FX_TEST_AVAILABLE and not (
                packaged_in_bin
                or packaged_in_unit_tests
                or packaged_in_gn_pkg
                or is_rust_device_test_packaged(device_bin, node)
            ):
                findings.append({
                    "source": "cq_reachability",
                    "category": "unpackaged_rust_device_test",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": (
                        f"Rust device test binary ':{device_bin}' ({rule_name}) in '{bazel_rel}' is not packaged by any "
                        "`fx_packaged_binary` or `fx_unittest_package`, so no `fx_test` runs it and it is missing from tests.json."
                    ),
                    "remediation": (
                        f"Add `fx_unittest_package(name = ..., unit_tests = [\":{device_bin}\"])` or "
                        f"`fx_packaged_binary(testonly = True, binary = \":{device_bin}\", binary_name = ...)` "
                        "(GN's output name, e.g. `<crate>_lib_test`), a `meta/<component>.cml` including "
                        "`//src/sys/test_runners/rust/default.shard.cml` and `syslog/client.shard.cml` with "
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

    had_base_local_suite = bool(base_gn_src and SUITE_RE.search(base_gn_src))
    gn_pre = gn_src[: SENTINEL_RE.search(gn_src).start()] if SENTINEL_RE.search(gn_src) else gn_src
    for t_name, kind, rule_name, lineno, twin in exportable:
        param = "target_tests" if kind == "target" else "host_tests"
        entries = listed_in.get(t_name, [])
        anc_suite_info = nearest_ancestor_test_suite(td, param)
        missing_anc_area = (
            preferred_ancestor_area_suite(td, param)
            if anc_suite_info is None and not had_base_local_suite
            else None
        )
        compat_expr = (
            '["@platforms//os:fuchsia"]'
            if param == "target_tests"
            else "HOST_OS_CONSTRAINTS"
        )
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
        gn_test_callers = direct_gn_test_callers(td, t_name)
        local_gn_tests_referenced = gn_tests_group_is_referenced(td)
        if not entries:
            if anc_suite_info is not None:
                anc_pkg, anc_suite = anc_suite_info
                req_vis = ancestor_suite_visibility(anc_pkg)
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not wired "
                    f"into the existing ancestor `test_suite` `//{anc_pkg}:{anc_suite}` in "
                    f"'{anc_pkg}/BUILD.bazel' (exported via `bazel_test_suite` in "
                    "`BUILD.gn`), so it is missing from tests.json and neither `fx test` "
                    "nor infra runs it."
                )
                rem_text = (
                    f"Add `\"//{td}:{t_name}\"` (in sorted order) to "
                    f"`test_suite(name = \"{anc_suite}\", tests = [...])` in "
                    f"'{anc_pkg}/BUILD.bazel' and set "
                    f"`visibility = {json.dumps(req_vis)}` on `:{t_name}` in "
                    f"'{bazel_rel}' (do not add a per-package `bazel_test_suite` in "
                    f"'{gn_rel}')."
                )
            elif missing_anc_area is not None:
                anc_pkg, anc_suite = missing_anc_area
                req_vis = ancestor_suite_visibility(anc_pkg)
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not exported "
                    f"through an ancestor `test_suite` in '{anc_pkg}/BUILD.bazel' and "
                    f"`bazel_test_suite` in '{anc_pkg}/BUILD.gn', so it is missing from "
                    "tests.json and neither `fx test` nor infra runs it."
                )
                rem_text = (
                    f"Create or update a flat `test_suite(name = \"{anc_suite}\", "
                    f"target_compatible_with = {compat_expr}, tests = [\"//{td}:{t_name}\"])` "
                    f"in '{anc_pkg}/BUILD.bazel', wire `\"//{anc_pkg}:{anc_suite}\"` into "
                    f"`bazel_test_suite(\"bazel_tests\") {{ {param} = [ \"//{anc_pkg}:{anc_suite}\" ] }}` "
                    f"(and `\":bazel_tests\"` into `group(\"tests\")`) in '{anc_pkg}/BUILD.gn', "
                    f"and set `visibility = {json.dumps(req_vis)}` on `:{t_name}` in "
                    f"'{bazel_rel}' (do not add a per-package `bazel_test_suite` in '{gn_rel}')."
                )
            else:
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not listed in any GN "
                    f"`bazel_test_suite({param} = [...])`, so it is missing from tests.json: neither "
                    "`fx test` nor infra runs it, and defining it (or running it with `fx bazel test`) "
                    "does not change that."
                )
                rem_text = (
                    f"In '{gn_rel}' (above any BAZEL2GN SENTINEL), add "
                    "`import(\"//build/bazel/bazel_test_suite.gni\")` and "
                    f"`bazel_test_suite(\"<name>\") {{ {param} = [ \"//{td}:{t_name}\" ] }}` (absolute Bazel "
                    "labels; a Bazel `test_suite` label also works), named after the GN test package it "
                    "replaces, and list `\":<name>\"` in `group(\"tests\")`."
                )
            findings.append({
                "source": "cq_reachability",
                "category": "unexported_bazel_test",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg_text,
                "remediation": rem_text,
            })
            continue
        # Enforce wiring into an existing or newly created ancestor BUILD.bazel test_suite
        # instead of a new per-package bazel_test_suite in BUILD.gn.
        handled_local_ancestor_suite = False
        if anc_suite_info is not None and not had_base_local_suite:
            anc_pkg, anc_suite = anc_suite_info
            req_vis = ancestor_suite_visibility(anc_pkg)
            rm_local_desc = (
                "remove the per-package `bazel_test_suite`, unreachable `group(\"tests\")`, "
                "and unused `//build/bazel/bazel_test_suite.gni` import"
                if (gn_test_callers and not local_gn_tests_referenced)
                else "remove the per-package `bazel_test_suite` "
                "(and unused `//build/bazel/bazel_test_suite.gni` import)"
            )
            in_anc_suite = t_name in foreign_suite_tests(
                f"//{anc_pkg}:{anc_suite}", anc_pkg, td, set()
            )
            local_entries = [e for e in entries if e[0] == gn_rel]
            if not in_anc_suite:
                handled_local_ancestor_suite = True
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is exported "
                    f"via a per-package `bazel_test_suite` in '{gn_rel}' instead of "
                    f"being wired into the existing ancestor `test_suite` "
                    f"`//{anc_pkg}:{anc_suite}` in '{anc_pkg}/BUILD.bazel'."
                )
                rem_text = (
                    f"Wire `\"//{td}:{t_name}\"` (in sorted order) into "
                    f"`test_suite(name = \"{anc_suite}\", tests = [...])` in "
                    f"'{anc_pkg}/BUILD.bazel', set "
                    f"`visibility = {json.dumps(req_vis)}` on `:{t_name}` in "
                    f"'{bazel_rel}', and {rm_local_desc} from '{gn_rel}'."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "bypassed_ancestor_test_suite",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            elif local_entries:
                handled_local_ancestor_suite = True
                local_rel, local_name_s, _, local_line = local_entries[0]
                msg_text = (
                    f"`bazel_test_suite(\"{local_name_s}\")` in '{local_rel}' "
                    f"redundantly exports ':{t_name}', which is already wired into the "
                    f"ancestor `test_suite` `//{anc_pkg}:{anc_suite}` in "
                    f"'{anc_pkg}/BUILD.bazel'."
                )
                rem_text = (
                    f"Remove `bazel_test_suite(\"{local_name_s}\")` (and "
                    f"`\":{local_name_s}\"` from `group(\"tests\")`, plus any unused "
                    f"`//build/bazel/bazel_test_suite.gni` import) from '{local_rel}'."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "redundant_local_bazel_test_suite",
                    "severity": "error",
                    "file": local_rel,
                    "line": local_line,
                    "message": msg_text,
                    "remediation": rem_text,
                })
        elif missing_anc_area is not None and any(e[0] == gn_rel for e in entries):
            handled_local_ancestor_suite = True
            anc_pkg, anc_suite = missing_anc_area
            req_vis = ancestor_suite_visibility(anc_pkg)
            msg_text = (
                f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is exported "
                f"via a per-package `bazel_test_suite` in '{gn_rel}' instead of "
                f"a flat `test_suite(name = \"{anc_suite}\")` in '{anc_pkg}/BUILD.bazel' "
                f"wired through `bazel_test_suite(\"bazel_tests\")` in '{anc_pkg}/BUILD.gn'."
            )
            rem_text = (
                f"Create or update a flat `test_suite(name = \"{anc_suite}\", "
                f"target_compatible_with = {compat_expr}, tests = [\"//{td}:{t_name}\"])` "
                f"in '{anc_pkg}/BUILD.bazel', wire `\"//{anc_pkg}:{anc_suite}\"` into "
                f"`bazel_test_suite(\"bazel_tests\") {{ {param} = [ \"//{anc_pkg}:{anc_suite}\" ] }}` "
                f"(and `\":bazel_tests\"` into `group(\"tests\")`) in '{anc_pkg}/BUILD.gn', "
                f"set `visibility = {json.dumps(req_vis)}` on `:{t_name}` in "
                f"'{bazel_rel}', and remove the per-package `bazel_test_suite` "
                f"(and unused `//build/bazel/bazel_test_suite.gni` import) from '{gn_rel}'."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "bypassed_ancestor_test_suite",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg_text,
                "remediation": rem_text,
            })
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
        local_suite_unwired_via_tests = bool(
            not handled_local_ancestor_suite
            and gn_test_callers
            and not local_gn_tests_referenced
            and all(
                s_rel == gn_rel and not refs_from_parent(parent_gn_src, td, s_name)
                for s_rel, s_name, _, _ in entries
            )
        )
        if local_suite_unwired_via_tests or not any(
            suite_is_wired(s_rel, s_name) for s_rel, s_name, _, _ in entries
        ):
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
                    f"'{s_rel}', but no reachable `group(\"tests\")` in the GN build graph references that "
                    "suite, so it is not reachable from the test roots and stays out of tests.json."
                ),
                "remediation": (
                    f"Wire `//{td}:{t_name}` into an existing ancestor `test_suite` exported to GN "
                    f"(or reference `//{os.path.dirname(s_rel)}:tests` / `//{os.path.dirname(s_rel)}:{s_name}` "
                    "from the parent directory's `tests` group)."
                    if local_suite_unwired_via_tests
                    else (
                        f"Add `\":{s_name}\"` to `group(\"tests\")` in '{s_rel}' (or reference "
                        f"`//{os.path.dirname(s_rel)}:{s_name}` from the parent directory's `tests` group)."
                    )
                ),
            })

    # Flag an unreachable hand-written group("tests") above BAZEL2GN SENTINEL when an ancestor
    # BUILD.gn depends directly on the package's generated test target(s) instead of //<td >:tests.
    m_tests_grp = re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_code(gn_pre))
    if (
        m_tests_grp
        and SENTINEL_RE.search(gn_src)
        and not SUITE_RE.search(gn_code(gn_pre))
        and not gn_tests_group_is_referenced(td)
        and any(direct_gn_test_callers(td, tn) for tn, _, _, _ in test_targets)
    ):
        grp_line = gn_pre.count("\n", 0, m_tests_grp.start()) + 1
        direct_callers = sorted(
            {c for tn, _, _, _ in test_targets for c in direct_gn_test_callers(td, tn)}
        )
        caller_desc = ", ".join(f"'{c}/BUILD.gn'" for c in direct_callers)
        findings.append({
            "source": "cq_reachability",
            "category": "unreachable_local_gn_tests_group",
            "severity": "error",
            "file": gn_rel,
            "line": grp_line,
            "message": (
                f"`group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' is unreachable in "
                f"the GN build graph because nothing depends on `//{td}:tests` ({caller_desc} "
                "depends directly on the generated test target below the sentinel)."
            ),
            "remediation": (
                f"Remove the unreachable `group(\"tests\")` target above `## BAZEL2GN SENTINEL` "
                f"in '{gn_rel}'."
            ),
        })

print(json.dumps(findings, indent=2))
PYEOF
