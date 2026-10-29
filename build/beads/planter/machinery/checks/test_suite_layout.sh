#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check of how the change hooks Bazel tests into the GN test graph,
# following docs/development/build/bazel_concepts/tests.md:
# - When an ancestor directory already defines a wired `bazel_test_suite()` /
#   `BUILD.bazel` `test_suite()`, leaf packages must wire into that ancestor
#   `test_suite()` rather than adding duplicate per-package `bazel_test_suite()`
#   targets.
# - When NO ancestor `bazel_test_suite()` exists yet above the package, placing
#   `bazel_test_suite()` at the nearest `group("tests")` boundary (moving a
#   shim-only leaf `BUILD.gn` up to the non-umbrella ancestor `BUILD.gn` that
#   lists `<pkg>:tests`, or keeping `bazel_test_suite()` in `<pkg>/BUILD.gn`
#   when `<pkg>` keeps `BUILD.gn` for other targets or itself owns the
#   `group("tests")` boundary) is permitted and required.
# Only additions since $PLANTER_CHANGE_BASE are checked, so existing per-package
# suites stay as they are.
#
# Categories:
# 1. per_package_bazel_test_suite (ERROR): a BUILD.gn gets a new
#    `bazel_test_suite()` listing individual tests while an ancestor area
#    `test_suite()` of the same kind already covers the directory.
# 2. bazel_test_suite_only_build_gn (ERROR): after the change a BUILD.gn holds
#    nothing but imports, `bazel_test_suite()`s and a `group("tests")` of them
#    (and didn't before), AND either an ancestor area `test_suite()` exists or a
#    non-umbrella ancestor `BUILD.gn` wraps `<pkg>:tests` (so that ancestor owns
#    the `group("tests")` boundary; when `<pkg>` itself owns the `group("tests")`
#    boundary with no ancestor suite or referrer, keeping `<pkg>/BUILD.gn` is
#    permitted and required).
# 3. nested_test_suite (ERROR): a `test_suite(tests = ...)` or GN
#    `bazel_test_suite()` gains a cross-package `test_suite()` entry instead of
#    listing tests directly (same-file sub-suites are allowed).
# 4. implicit_test_suite (ERROR): a newly added Bazel `test_suite()` omits an
#    explicit non-empty `tests = [...]` list.

WORKDIR="${PLANTER_WORKDIR:-.}"

python3 -B - "$WORKDIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
SOURCE = "test_suite_layout"
DOC = "docs/development/build/bazel_concepts/tests.md"
UMBRELLA_ROOTS = frozenset(
    {"src", "zircon", "zircon/system", "sdk", "third_party", "bundles"}
)

findings = []


def add(category, file, line, message, remediation):
    findings.append(
        {
            "source": SOURCE,
            "category": category,
            "severity": "ERROR",
            "file": file,
            "line": line,
            "message": message,
            "remediation": remediation,
        }
    )


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return ""


def read(rel):
    try:
        with open(
            os.path.join(workdir, rel), encoding="utf-8", errors="replace"
        ) as f:
            return f.read()
    except OSError:
        return None


_base_cache = {}


def base_text(rel):
    if rel not in _base_cache:
        try:
            _base_cache[rel] = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError):
            _base_cache[rel] = None
    return _base_cache[rel]


def line_of(text, offset):
    return text.count("\n", 0, offset) + 1


def gn_code(text):
    """Blanks out GN comments without moving byte offsets."""
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "#":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def closing(code, i):
    """Index of the bracket closing `{`, `[` or `(` at code[i]."""
    open_c = code[i]
    close_c = {"{": "}", "[": "]", "(": ")"}[open_c]
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
        elif c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')


def gn_targets(text):
    code = gn_code(text)
    out = []
    for m in GN_TARGET.finditer(code):
        end = closing(code, m.end() - 1)
        out.append(
            (m.group(1), m.group(2), code[m.end() : end], m.start(), end + 1)
        )
    return out


def gn_list(body, attr):
    out = []
    for m in re.finditer(r"\b" + attr + r"\s*\+?=\s*\[", body):
        end = closing(body, m.end() - 1)
        out += re.findall(r'"([^"]*)"', body[m.end() : end])
    return out


def bazel_test_suites_gn(text):
    suites = {}
    for tmpl, name, body, start, _ in gn_targets(text or ""):
        if tmpl == "bazel_test_suite":
            suites[name] = {
                p: gn_list(body, p) for p in ("host_tests", "target_tests")
            }
            suites[name]["line"] = line_of(text, start)
    return suites


def bazel_test_suites(text):
    suites = {}
    try:
        tree = ast.parse(text or "")
    except SyntaxError:
        return suites
    for node in ast.walk(tree):
        if not (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "test_suite"
        ):
            continue
        kw = {k.arg: k.value for k in node.keywords if k.arg}
        if not isinstance(kw.get("name"), ast.Constant):
            continue
        tests = None
        if "tests" in kw:
            v = kw["tests"]
            if isinstance(v, ast.List) and all(
                isinstance(e, ast.Constant) and isinstance(e.value, str)
                for e in v.elts
            ):
                tests = [e.value for e in v.elts]
            else:
                tests = "unknown"
        suites[kw["name"].value] = {"tests": tests, "line": node.lineno}
    return suites


def resolve(label, pkg, gn=False):
    label = re.sub(r"^@@?(?=//)", "", label.strip())
    if label.startswith(":"):
        return None if gn else (pkg, label[1:])
    if not label.startswith("//"):
        return None
    path, _, name = label[2:].partition(":")
    return path.strip("/"), name or os.path.basename(path)


def is_test_suite(pkg, name):
    return name in bazel_test_suites(read(f"{pkg}/BUILD.bazel"))


def label_line(text, start_line, label):
    lines = text.splitlines()
    for i in range(max(start_line - 1, 0), len(lines)):
        if f'"{label}"' in lines[i]:
            return i + 1
    return start_line


changed = set(
    p
    for p in git(
        ["diff", "--name-only", "--no-renames", change_base, "--"]
    ).splitlines()
    if p
)
changed |= set(
    p
    for p in git(["ls-files", "--others", "--exclude-standard"]).splitlines()
    if p
)
gn_files = sorted(
    p for p in changed if os.path.basename(p) == "BUILD.gn" and read(p) is not None
)
bazel_files = sorted(
    p
    for p in changed
    if os.path.basename(p) == "BUILD.bazel" and read(p) is not None
)


def area_suite(pkg, param):
    """Nearest strict ancestor `test_suite()` exported via GN `bazel_test_suite(param)`."""
    ancestors = []
    a = os.path.dirname(pkg)
    while a:
        ancestors.append(a)
        a = os.path.dirname(a)
    queue = []
    for a in ancestors:
        for s in bazel_test_suites_gn(read(f"{a}/BUILD.gn")).values():
            queue += [
                r
                for r in (resolve(l, a, gn=True) for l in s[param])
                if r and is_test_suite(*r)
            ]
    best, seen = None, set()
    while queue and len(seen) < 1000:
        r = queue.pop()
        if r in seen:
            continue
        seen.add(r)
        if r[0] in ancestors and (best is None or len(r[0]) > len(best[0])):
            best = r
        tests = (
            bazel_test_suites(read(f"{r[0]}/BUILD.bazel"))
            .get(r[1], {})
            .get("tests")
        )
        if isinstance(tests, list):
            queue += [
                t
                for t in (resolve(l, r[0]) for l in tests)
                if t and is_test_suite(*t)
            ]
    return best


def gn_referrers(pkg):
    """Non-umbrella ancestor BUILD.gn files that list `pkg`'s `tests` group."""
    out = []
    a = os.path.dirname(pkg)
    while a:
        if a not in UMBRELLA_ROOTS:
            text = read(f"{a}/BUILD.gn")
            if text:
                rel = os.path.relpath(pkg, a)
                pat = (
                    r'"(//'
                    + re.escape(pkg)
                    + "|"
                    + re.escape(rel)
                    + r'):tests(?:\([^"]*\))?"'
                )
                if re.search(pat, gn_code(text)):
                    out.append(f"{a}/BUILD.gn")
        a = os.path.dirname(a)
    return out


IMPORT = re.compile(r'^\s*import\(\s*"[^"]+"\s*\)\s*$', re.M)


def only_test_suite_shim(text, pkg):
    """True when BUILD.gn holds only imports, bazel_test_suite()s and group("tests") of them."""
    if text is None:
        return False
    targets = gn_targets(text)
    suites = [
        name for tmpl, name, _, _, _ in targets if tmpl == "bazel_test_suite"
    ]
    if not suites:
        return False
    code = gn_code(text)
    for tmpl, name, body, start, end in sorted(targets, key=lambda t: -t[3]):
        if tmpl == "bazel_test_suite":
            pass
        elif tmpl == "group" and name == "tests":
            deps = (
                gn_list(body, "deps")
                + gn_list(body, "public_deps")
                + gn_list(body, "data_deps")
            )
            own = {f":{n}" for n in suites} | {f"//{pkg}:{n}" for n in suites}
            if any(d not in own for d in deps):
                return False
        else:
            return False
        code = code[:start] + code[end:]
    return not IMPORT.sub("", code).strip()


for gn in gn_files:
    pkg = os.path.dirname(gn)
    text = read(gn)
    old = base_text(gn)
    suites, old_suites = bazel_test_suites_gn(text), bazel_test_suites_gn(old)

    old_labels_all = {
        l
        for o in old_suites.values()
        for p in ("host_tests", "target_tests")
        for l in o[p]
    }
    for name, s in suites.items():
        added = name not in old_suites and not (
            old_labels_all & set(s["host_tests"] + s["target_tests"])
        )
        for param in ("host_tests", "target_tests"):
            old_labels = (
                set(old_suites[name][param])
                if name in old_suites
                else (set() if added else old_labels_all)
            )
            new_labels = [l for l in s[param] if l not in old_labels]

            # Rule 1: new suite of individual tests when an ancestor area suite
            # already covers this directory.
            raw = [
                l
                for l in s[param]
                if (r := resolve(l, pkg, gn=True)) and not is_test_suite(*r)
            ]
            area = area_suite(pkg, param) if added and raw else None
            if area:
                area_pkg, area_name = area
                add(
                    "per_package_bazel_test_suite",
                    gn,
                    s["line"],
                    f'`bazel_test_suite("{name}")` adds a new `{param}` entry point for individual tests, '
                    f"but they belong in the existing ancestor area suite `//{area_pkg}:{area_name}`, "
                    "which GN already exports.",
                    f"Add {', '.join(f'`{l}`' for l in raw)} as individual entries to "
                    f'`test_suite(name = "{area_name}")` in `{area_pkg}/BUILD.bazel`, add '
                    f'`"//{area_pkg}:__pkg__"` to each test\'s `visibility`, and remove '
                    f'`bazel_test_suite("{name}")` (and its `group("tests")` entry). See {DOC}.',
                )

            # Rule 3: cross-package test_suite() entry.
            for lbl in new_labels:
                r = resolve(lbl, pkg, gn=True)
                if r and r[0] != pkg and is_test_suite(*r):
                    add(
                        "nested_test_suite",
                        gn,
                        label_line(text, s["line"], lbl),
                        f'`bazel_test_suite("{name}")` now exports `{lbl}`, a `test_suite()` of another '
                        "package. That adds an extra level of suites across directories.",
                        f"List the tests of `{lbl}` themselves (absolute labels) in the area's flat "
                        "`test_suite()`, add the area's package to each test's `visibility`, and don't add a "
                        f"per-package `test_suite()`. See {DOC}.",
                    )

    # Rule 2: a BUILD.gn that only exists to hold bazel_test_suite() + group("tests")
    # when either an ancestor area suite exists OR a non-umbrella ancestor BUILD.gn
    # owns the enclosing group("tests") boundary. When neither exists, `pkg` itself
    # owns the group("tests") boundary and keeping `pkg/BUILD.gn` is required.
    if only_test_suite_shim(text, pkg) and not only_test_suite_shim(old, pkg):
        areas = sorted(
            {
                f"//{a[0]}:{a[1]}"
                for p in ("host_tests", "target_tests")
                if (a := area_suite(pkg, p))
            }
        )
        referrers = gn_referrers(pkg)
        if areas or referrers:
            if areas:
                where = (
                    "Add the tests to the ancestor area suite "
                    + " / ".join(f"`{a}`" for a in areas)
                    + " instead"
                )
            else:
                ref = f"`{referrers[0]}`"
                where = (
                    f"Move `bazel_test_suite()` to {ref} (the nearest enclosing "
                    "`group(\"tests\")` boundary): add the tests to its existing "
                    "`bazel_test_suite()`, or add one there in place of the "
                    f"`{pkg}:tests` entry"
                )
            add(
                "bazel_test_suite_only_build_gn",
                gn,
                1,
                f'After the change `{gn}` holds nothing but `bazel_test_suite()` and `group("tests")`. '
                "Don't keep a leaf BUILD.gn solely as a shim when an ancestor area suite or enclosing "
                "`group(\"tests\")` boundary exists.",
                f"{where}, remove the `{pkg}:tests` entry from the GN group that lists it, and delete `{gn}`. "
                f"See {DOC}.",
            )

for bz in bazel_files:
    pkg = os.path.dirname(bz)
    text = read(bz)
    suites, old_suites = bazel_test_suites(text), bazel_test_suites(
        base_text(bz)
    )
    for name, s in suites.items():
        added = name not in old_suites

        # Rule 4: newly added test_suite() without an explicit non-empty tests list.
        if added and (s["tests"] is None or s["tests"] == []):
            add(
                "implicit_test_suite",
                bz,
                s["line"],
                f'`test_suite(name = "{name}")` has no explicit non-empty `tests` list, so Bazel collects '
                f"every test of `//{pkg}`, including raw `rust_test`s and tests of incompatible platforms.",
                f"List each test explicitly in `tests = [...]`. See {DOC}.",
            )
        if not isinstance(s["tests"], list):
            continue

        # Rule 3: cross-package test_suite() entry.
        old_tests = old_suites.get(name, {}).get("tests")
        old_set = set(old_tests) if isinstance(old_tests, list) else set()
        for lbl in s["tests"]:
            if lbl in old_set:
                continue
            r = resolve(lbl, pkg)
            if r and r[0] != pkg and is_test_suite(*r):
                add(
                    "nested_test_suite",
                    bz,
                    label_line(text, s["line"], lbl),
                    f'`test_suite(name = "{name}")` now lists `{lbl}`, a `test_suite()` of another package. '
                    "That adds an extra level of suites across directories.",
                    f'List the tests of `{lbl}` themselves (absolute labels, adding `"//{pkg}:__pkg__"` to '
                    "each test's `visibility`) and delete that `test_suite()` if nothing else uses it. "
                    f"Same-file sub-suites rolled up in this BUILD.bazel are fine. See {DOC}.",
                )

print(json.dumps(findings, indent=2))
PYEOF
