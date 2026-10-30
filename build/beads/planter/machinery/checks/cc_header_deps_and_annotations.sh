#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Two review-driven checks on the BUILD.bazel files of the target directories:
#
# 1. undocumented_bazel2gn_directive: every `# @bazel2gn:` directive (skip,
#    raw_attribute, raw_overwrite, value_overwrite, path_overwrite) that the
#    change adds needs a short human comment explaining why Bazel and GN differ.
#    One comment may cover a stacked group of directives, and one documented
#    directive covers identical directives elsewhere in the same file.
#
# 2. public_header_dep_in_implementation_deps: a C/C++ library whose public
#    headers (hdrs) #include a dependency's headers must list that dependency in
#    `deps` (GN public_deps), not `implementation_deps` (GN deps).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-}"

python3 - "$WORKDIR" "$TARGET_DIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in re.split(r"[,\s]+", sys.argv[2]) if d.strip()]
change_base = sys.argv[3]

findings = []

DIRECTIVE_RE = re.compile(r"#\s*@bazel2gn:([a-z_]+)(:.*)?$")
CC_RULE_RE = re.compile(r"^(fx_cc_library|cc_library|idk_cc_source_library\w*)\(\s*$", re.M)

DIRECTIVE_REMEDIATION = """\
Add a brief comment right above the target, attribute or list entry that carries
the directive, saying why Bazel and GN differ there (e.g. "fx_cc_binary names the
binary after the target; GN sets output_name", or "GN depends on the default
:rust group alias; Bazel depends on the library directly"). One comment may cover
a stacked group of directives. If the comment describes a workaround or a
bazel2gn limitation, include TODO(https://fxbug.dev/<id>)."""

HEADER_REMEDIATION = """\
Move the label from implementation_deps to deps (bazel2gn emits deps as GN
public_deps), then re-run `fx bazel2gn -d <dir>` so BUILD.gn lists it in
public_deps. implementation_deps is only for libraries used by srcs, never for
ones whose headers appear in hdrs."""


def base_text(rel):
    """Returns the file at $PLANTER_CHANGE_BASE, or "" when it did not exist."""
    if not change_base:
        return ""
    try:
        return subprocess.run(
            ["git", "-C", workdir, "show", f"{change_base}:{rel}"],
            capture_output=True, text=True, check=True).stdout
    except subprocess.CalledProcessError:
        return ""


def block_at(text, start):
    depth = 0
    for i in range(start, len(text)):
        c = text[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[start:i + 1]
    return text[start:]


def list_attr(block, attr):
    m = re.search(r"^\s+" + attr + r"\s*=\s*\[(.*?)\]", block, re.M | re.S)
    if not m:
        return []
    return re.findall(r'"([^"]+)"', m.group(1))


# Check undocumented # @bazel2gn: directives
def is_plain_comment(line):
    s = line.strip()
    return s.startswith("#") and not DIRECTIVE_RE.search(s) and len(s.lstrip("#").strip()) > 0


def is_directive_only(line):
    s = line.strip()
    return s.startswith("#") and DIRECTIVE_RE.search(s) is not None


def opening_line(lines, idx):
    """For a closing `]`/`)` line, returns the index of the line that opened it."""
    depth = 0
    for j in range(idx, -1, -1):
        code = lines[j].split("#", 1)[0]
        depth += code.count("]") + code.count(")") - code.count("[") - code.count("(")
        if depth <= 0 and j != idx:
            return j
    return None


def documented(lines, idx):
    anchors = [idx]
    code = lines[idx].split("#", 1)[0].strip()
    if code.startswith("]") or code.startswith(")"):
        op = opening_line(lines, idx)
        if op is not None:
            anchors.append(op)
    for a in anchors:
        j = a - 1
        while j >= 0 and is_directive_only(lines[j]):
            j -= 1
        if j >= 0 and is_plain_comment(lines[j]):
            return True
    return False


def check_directives(d, rel, text):
    base_lines = set(l.strip() for l in base_text(rel).splitlines())
    lines = text.splitlines()
    documented_keys = set()
    pending = []
    for i, line in enumerate(lines):
        m = DIRECTIVE_RE.search(line)
        if not m:
            continue
        key = m.group(0).strip()
        if documented(lines, i):
            documented_keys.add(key)
        elif line.strip() not in base_lines:
            pending.append((i, m.group(1), key))
    for i, kind, key in pending:
        if key in documented_keys:
            continue
        findings.append({
            "severity": "WARNING",
            "file": rel,
            "line": i + 1,
            "rule": "undocumented_bazel2gn_directive",
            "message": (f"New `# @bazel2gn:{kind}` directive has no comment explaining "
                        "why the generated GN differs from Bazel here."),
            "remediation": DIRECTIVE_REMEDIATION,
        })


# Check public headers whose includes come from implementation_deps
def include_matches_label(includes, label):
    lab = label.lstrip("@")
    pkg, _, name = lab.partition(":")
    pkg = pkg.strip("/")
    last = pkg.rsplit("/", 1)[-1]
    name = name or last
    # FIDL bindings: HLCPP <fuchsia/io/cpp/fidl.h>, new C++ <fidl/fuchsia.io/cpp/...>.
    fm = re.match(r"(.+)_(hlcpp|cpp)$", name)
    if fm and (pkg.startswith("sdk/fidl/") or last == fm.group(1)):
        lib, flavor = fm.group(1), fm.group(2)
        for inc in includes:
            if flavor == "hlcpp" and inc.startswith(lib.replace(".", "/") + "/cpp/"):
                return inc, "ERROR"
            if flavor == "cpp" and inc.startswith(f"fidl/{lib}/cpp/"):
                return inc, "ERROR"
        return None
    # Common Fuchsia layouts: //<...>/<x> exports <x/...> or <lib/<x>/...>.
    for inc in includes:
        if last and (inc.startswith(f"{last}/") or inc.startswith(f"lib/{last}/")):
            return inc, "WARNING"
    return None


def check_header_deps(d, rel, text):
    for m in CC_RULE_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        impl = list_attr(block, "implementation_deps")
        if not impl:
            continue
        deps = set(list_attr(block, "deps"))
        includes = []
        for h in list_attr(block, "hdrs"):
            if h.startswith(":") or h.startswith("//") or h.startswith("@"):
                continue
            path = os.path.join(workdir, d, h)
            if not os.path.isfile(path):
                continue
            with open(path, encoding="utf-8", errors="replace") as f:
                includes += re.findall(r'^\s*#\s*include\s*<([^>]+)>', f.read(), re.M)
        if not includes:
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        line = text[:m.start()].count("\n") + 1
        for label in impl:
            if label in deps:
                continue
            hit = include_matches_label(includes, label)
            if not hit:
                continue
            inc, severity = hit
            findings.append({
                "severity": severity,
                "file": rel,
                "line": line,
                "rule": "public_header_dep_in_implementation_deps",
                "message": (f"{m.group(1)}(name = \"{name}\") lists \"{label}\" in "
                            f"implementation_deps, but a header in hdrs includes <{inc}>, "
                            "so dependents need it (GN public_deps)."),
                "remediation": HEADER_REMEDIATION,
            })


for d in target_dirs:
    rel = os.path.join(d, "BUILD.bazel")
    path = os.path.join(workdir, rel)
    if not os.path.isfile(path):
        continue
    with open(path, encoding="utf-8") as f:
        text = f.read()
    check_directives(d, rel, text)
    check_header_deps(d, rel, text)

print(json.dumps(findings, indent=2))
PYEOF
