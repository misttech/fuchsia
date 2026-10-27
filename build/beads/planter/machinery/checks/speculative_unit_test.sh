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
def base_gn_had_tests(base_gn, name):
    if base_gn is None:
        return True  # No base BUILD.gn: nothing to compare against.
    if f'"{name}_test"' in base_gn:
        return True
    m = re.search(r'\w+\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', base_gn)
    if not m:
        return True
    body = block_at(base_gn, m.end() - 1, "{", "}")
    return "with_unit_tests" in body or "with_host_unit_tests" in body


def git_show(path):
    r = subprocess.run(["git", "-C", workdir, "show", f"{base}:{path}"],
                       capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def report(d, text, pos, name, lib):
    findings.append({
        "severity": "ERROR",
        "file": os.path.join(d, "BUILD.bazel"),
        "line": text[:pos].count("\n") + 1,
        "rule": "speculative_empty_unit_test",
        "message": (f"{name} tests crate :{lib}, whose sources contain no tests, and "
                    f"the base GN target :{lib} had no with_unit_tests."),
        "remediation": REMEDIATION,
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
            if not base_gn_had_tests(base_gn, name) and \
                    not sources_have_tests(crate_sources(d, block)):
                report(d, text, pos, f"with_unit_tests on :{name}", name)

    # Check standalone rustc_test(crate = ":<lib>") without own srcs.
    for m in TEST_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        crate = attr_str(block, "crate")
        tname = attr_str(block, "name") or "?"
        if not crate or not crate.startswith(":") or re.search(r"^\s+srcs\s*=", block, re.M):
            continue
        lib = crate[1:]
        if lib not in libs or base_gn_had_tests(base_gn, lib):
            continue
        if not sources_have_tests(crate_sources(d, libs[lib][1])):
            report(d, text, m.start(), f"rustc_test :{tname}", lib)

print(json.dumps(findings, indent=2))
PYEOF
