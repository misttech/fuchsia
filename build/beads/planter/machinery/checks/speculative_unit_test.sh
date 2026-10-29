#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags Rust unit tests that a migration invents: a Bazel rustc_test with
# crate = ":<lib>" (or with_unit_tests on the library) whose crate sources
# contain no test functions, when the library's GN target at
# $PLANTER_CHANGE_BASE had no with_unit_tests. Such a test builds an empty
# test binary and usually an fx_unittest_package that no test suite runs,
# and bazel2gn copies it into BUILD.gn as a new empty GN test package.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
BASE="${PLANTER_CHANGE_BASE:-HEAD~1}"

python3 - "$WORKDIR" "$TARGET_DIR" "$BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in re.split(r"[,\s]+", sys.argv[2]) if d.strip()]
base = sys.argv[3]

LIB_RE = re.compile(r"^(rustc_library|rustc_binary)\(\s*$", re.M)
TEST_RE = re.compile(r"^rustc_test\(\s*$", re.M)
# Any #[test], #[fuchsia::test], #[fasync::run_singlethreaded(test)], #[cfg(test)] ...
HAS_TEST = re.compile(r"#\[\s*(?:[\w:]+::)?test\b|#\[cfg\(test\)\]|\(test\)\s*\]")

REMEDIATION = """\
The crate has no unit tests and its base GN target had no with_unit_tests, so this
adds an empty test binary (and package) that nothing runs. Remove the rustc_test /
with_unit_tests, the fx_unittest_package that wraps it, and loads left unused, then
rerun `fx bazel2gn -d <dir>`. Migrate only tests that existed in GN."""

findings = []


# Return the parenthesized block that starts at `start`.
def block_at(text, start, open_c="(", close_c=")"):
    depth = 0
    for i in range(start, len(text)):
        if text[i] == open_c:
            depth += 1
        elif text[i] == close_c:
            depth -= 1
            if depth == 0:
                return text[start:i + 1]
    return text[start:]


def attr_str(block, attr):
    m = re.search(r"^\s+" + attr + r'\s*=\s*"([^"]+)"', block, re.M)
    return m.group(1) if m else None


# Collect the .rs files of a crate: literal srcs, else every .rs under the package.
def crate_sources(d, block):
    pkg = os.path.join(workdir, d)
    m = re.search(r"^\s+srcs\s*=\s*\[(.*?)\]", block, re.M | re.S)
    files = re.findall(r'"([^":]+\.rs)"', m.group(1)) if m else []
    if files:
        return [os.path.join(pkg, f) for f in files]
    out = []
    for root, dirs, names in os.walk(pkg):
        dirs[:] = [x for x in dirs if not os.path.isfile(os.path.join(root, x, "BUILD.bazel"))]
        out += [os.path.join(root, n) for n in names if n.endswith(".rs")]
    return out


def sources_have_tests(paths):
    for p in paths:
        try:
            if HAS_TEST.search(open(p, encoding="utf-8", errors="ignore").read()):
                return True
        except OSError:
            pass
    return False


# Did the base GN target of the same name have unit tests (or a GN <name>_test)?
# Returns (had_tests: bool, explicit_false: bool).
def base_gn_test_status(base_gn, name):
    if base_gn is None:
        return True, False  # No base BUILD.gn: nothing to compare against.
    no_comments = re.sub(r"#[^\n]*", "", base_gn)
    m = re.search(r'\b(\w+)\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', no_comments)
    if m:
        tmpl_name = m.group(1)
        body = block_at(no_comments, m.end() - 1, "{", "}")
        if re.search(r"\bwith_unit_tests\s*=\s*false\b", body):
            return False, True
        if re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", body):
            return True, False
        # Check if invoked template in the same BUILD.gn sets with_unit_tests = true
        tm = re.search(r'\btemplate\(\s*"' + re.escape(tmpl_name) + r'"\s*\)\s*\{', no_comments)
        if tm:
            t_body = block_at(no_comments, tm.end() - 1, "{", "}")
            if re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*(?:true|invoker\.with_unit_tests)\b", t_body):
                return True, False
    if f'"{name}_test"' in no_comments or f'":{name}_test"' in no_comments:
        return True, False
    if not m:
        return True, False
    return False, False


# Did base_gn run unit tests on host (via ($host_toolchain), is_host, with_host_unit_tests,
# or wiring :<lib>_test directly into group("tests") outside a fuchsia_*test_package)?
def base_gn_had_host_unit_tests(base_gn, lib_name, test_name):
    if base_gn is None:
        return True
    no_comments = re.sub(r"#[^\n]*", "", base_gn)
    if (
        "host_toolchain" in no_comments
        or "with_host_unit_tests" in no_comments
        or re.search(r"\bis_host\b", no_comments)
    ):
        return True
    # Check if :<test_name> or :<lib_name>_test is referenced in group("tests") directly
    for gm in re.finditer(r'\bgroup\(\s*"tests"\s*\)\s*\{', no_comments):
        gbody = block_at(no_comments, gm.end() - 1, "{", "}")
        for cand in (test_name, f"{lib_name}_test"):
            if cand and re.search(r'":' + re.escape(cand) + r'"', gbody):
                return True
    return False


def _normalize_git_rev(rev):
    rev = (rev or "").strip()
    if rev.startswith("@"):
        minus_cnt = len(rev) - 1
        if all(c == "-" for c in rev[1:]):
            return f"HEAD~{minus_cnt}" if minus_cnt > 0 else "HEAD"
    return rev


def git_show(path):
    eff_base = _normalize_git_rev(base) or "HEAD"
    try:
        head_sha = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        base_sha = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", f"{eff_base}^{{commit}}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        head_sha, base_sha = "", ""
    if head_sha and head_sha == base_sha:
        try:
            head_subj = subprocess.check_output(
                ["git", "-C", workdir, "log", "-1", "--format=%s", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
            head_files = subprocess.check_output(
                ["git", "-C", workdir, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).splitlines()
            if (
                head_subj.startswith("[bazel_migration]")
                and target_dirs
                and any(
                    any(f == td or f.startswith(td + "/") for td in target_dirs)
                    for f in head_files
                )
            ):
                eff_base = "HEAD~1"
        except Exception:
            pass
    for ref in dict.fromkeys([eff_base, "HEAD~1", "origin/main"]):
        if not ref:
            continue
        r = subprocess.run(
            ["git", "-C", workdir, "show", f"{ref}:{path}"],
            capture_output=True,
            text=True,
        )
        if r.returncode == 0:
            return r.stdout
    return None


def report(d, text, pos, name, lib, explicit_false=False):
    reason = (
        f"the base GN target :{lib} explicitly set `with_unit_tests = false` (no existing unit tests in GN)."
        if explicit_false
        else f"its sources contain no tests, and the base GN target :{lib} had no `with_unit_tests = true`."
    )
    findings.append({
        "source": "speculative_unit_test",
        "category": "speculative_empty_unit_test",
        "severity": "ERROR",
        "file": os.path.join(d, "BUILD.bazel"),
        "line": text[:pos].count("\n") + 1,
        "rule": "speculative_empty_unit_test",
        "message": f"{name} tests crate :{lib}, but {reason}",
        "remediation": REMEDIATION,
    })


def report_host_test(d, text, pos, wrapper_name, test_name):
    findings.append({
        "source": "speculative_unit_test",
        "category": "speculative_host_unit_test",
        "severity": "ERROR",
        "file": os.path.join(d, "BUILD.bazel"),
        "line": text[:pos].count("\n") + 1,
        "rule": "speculative_host_unit_test",
        "message": (
            f"`{wrapper_name}` wraps `{test_name}` as a host test, but in `{d}/BUILD.gn` the unit "
            "tests were only built/run on Fuchsia (inside a Fuchsia test package) and were not run on host."
        ),
        "remediation": (
            f"Remove `{wrapper_name}` from `{d}/BUILD.bazel` and remove any `:{test_name}($host_toolchain)` "
            f"entry added to `group(\"tests\")` unless host unit tests existed in `{d}/BUILD.gn`."
        ),
    })


for d in target_dirs:
    path = os.path.join(workdir, d, "BUILD.bazel")
    if not os.path.isfile(path):
        continue
    text = open(path, encoding="utf-8").read()
    base_gn = git_show(f"{d}/BUILD.gn")
    libs = {}
    for m in LIB_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        name = attr_str(block, "name")
        if name:
            libs[name] = (m.start(), block)

    # Check with_unit_tests on the library itself.
    for name, (pos, block) in libs.items():
        if re.search(r"^\s+with_(host_)?unit_tests\s*=", block, re.M):
            had_tests, explicit_false = base_gn_test_status(base_gn, name)
            if explicit_false or (not had_tests and not sources_have_tests(crate_sources(d, block))):
                report(d, text, pos, f"with_unit_tests on :{name}", name, explicit_false=explicit_false)

    # Check standalone rustc_test(crate = ":<lib>") without own srcs.
    test_to_lib = {}
    for m in TEST_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        crate = attr_str(block, "crate")
        tname = attr_str(block, "name") or "?"
        if crate and crate.startswith(":"):
            test_to_lib[tname] = crate[1:]
        if not crate or not crate.startswith(":") or re.search(r"^\s+srcs\s*=", block, re.M):
            continue
        lib = crate[1:]
        if lib not in libs:
            continue
        had_tests, explicit_false = base_gn_test_status(base_gn, lib)
        if explicit_false or (not had_tests and not sources_have_tests(crate_sources(d, libs[lib][1]))):
            report(d, text, m.start(), f"rustc_test :{tname}", lib, explicit_false=explicit_false)

    # Check wrap_host_rust_test(binary = ":<test>") when base BUILD.gn only ran tests on Fuchsia.
    for m in re.finditer(r"^wrap_host_rust_test\(\s*$", text, re.M):
        block = block_at(text, m.end() - 1)
        wname = attr_str(block, "name") or "wrap_host_rust_test"
        bin_attr = attr_str(block, "binary") or ""
        tname = bin_attr[1:] if bin_attr.startswith(":") else bin_attr
        lib = test_to_lib.get(tname, tname[:-5] if tname.endswith("_test") else tname)
        if not base_gn_had_host_unit_tests(base_gn, lib, tname):
            report_host_test(d, text, m.start(), wname, tname)

print(json.dumps(findings, indent=2))
PYEOF
