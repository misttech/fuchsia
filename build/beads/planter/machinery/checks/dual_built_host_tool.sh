#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check for host tool binaries in dual-build BUILD.gn files
# (also consolidated into checks/migration_sanity.sh so pre-existing task pins
# always run it): rejects keeping a hand-written install_host_tools() (or
# bazel_host_tool()) above the BAZEL2GN SENTINEL while bazel2gn still generates
# the underlying host binary (rustc_binary, go_binary, executable) below the
# sentinel.

WORKDIR="${PLANTER_WORKDIR:-$(pwd)}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_target_dir = sys.argv[2].strip()
raw_target_dirs = sys.argv[3].strip()
raw_change_base = sys.argv[4].strip() or "HEAD"

SENTINEL = "## BAZEL2GN SENTINEL"
HOST_BIN_RULES = ("rustc_binary", "go_binary", "executable", "cc_binary")


# Normalize a directory argument to a checkout-relative package path.
def normalize_dir(path_str):
    d = path_str.strip()
    if os.path.isabs(d):
        rel = os.path.relpath(d, workdir)
        if rel != ".." and not rel.startswith("../"):
            d = rel
    if d.startswith("//"):
        d = d[2:]
    while d.startswith("./"):
        d = d[2:]
    d = d.rstrip("/")
    return "" if d == "." else d


# Resolve the effective git base commit when HEAD is already the task commit.
def resolve_change_base(wdir, base_ref):
    try:
        head = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        parent = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        subj = subprocess.check_output(
            ["git", "-C", wdir, "log", "-1", "--format=%s", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        base_sha = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--verify", "--quiet", f"{base_ref}^{{commit}}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        if head and parent and subj.startswith("[bazel_migration]"):
            if base_ref == "HEAD" or base_sha == head:
                return "HEAD~1"
    except Exception:
        pass
    return base_ref


# Collect candidate package directories from target dirs and git diff.
def collect_candidate_packages(wdir, tdir, tdirs, base_ref):
    pkgs = []
    seen = set()

    def add_pkg(pkg):
        norm = normalize_dir(pkg)
        if norm and norm not in seen:
            seen.add(norm)
            pkgs.append(norm)

    for token in tdirs.split():
        add_pkg(token)
    if tdir:
        add_pkg(tdir)

    eff_base = resolve_change_base(wdir, base_ref)
    for cmd in (
        ["git", "-C", wdir, "diff", "--name-only", eff_base, "HEAD"],
        ["git", "-C", wdir, "diff", "--name-only"],
        ["git", "-C", wdir, "ls-files", "--others", "--exclude-standard"],
    ):
        try:
            out = subprocess.check_output(cmd, stderr=subprocess.DEVNULL, text=True)
        except Exception:
            continue
        for line in out.splitlines():
            f = line.strip()
            if os.path.basename(f) in ("BUILD.gn", "BUILD.bazel", "BUILD"):
                add_pkg(os.path.dirname(f))
    return pkgs


# Extract balanced top-level or nested template invocations of the given rule names.
def find_gn_blocks(text, rule_names, base_line_offset=0):
    pattern = re.compile(
        r"^\s*(" + "|".join(re.escape(r) for r in rule_names) + r')\(\s*"([^"]+)"\s*\)\s*\{',
        re.MULTILINE,
    )
    blocks = []
    for match in pattern.finditer(text):
        rule = match.group(1)
        name = match.group(2)
        start_idx = match.start()
        line_no = base_line_offset + text.count("\n", 0, start_idx) + 1
        brace_idx = match.end() - 1
        depth = 0
        end_idx = len(text)
        i = brace_idx
        in_str = False
        while i < len(text):
            ch = text[i]
            if in_str:
                if ch == "\\":
                    i += 2
                    continue
                if ch == '"':
                    in_str = False
            else:
                if ch == "#":
                    nl = text.find("\n", i)
                    if nl == -1:
                        break
                    i = nl + 1
                    continue
                if ch == '"':
                    in_str = True
                elif ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                    if depth == 0:
                        end_idx = i + 1
                        break
            i += 1
        body = text[brace_idx + 1 : end_idx - 1]
        blocks.append(
            {
                "rule": rule,
                "name": name,
                "line": line_no,
                "body": body,
            }
        )
    return blocks


# Parse same-package target names referenced in deps or public_deps of a GN block.
def extract_same_pkg_deps(body, pkg):
    targets = []
    for quoted in re.findall(r'"([^"]+)"', body):
        label = quoted.split("(", 1)[0].strip()
        if label.startswith(":"):
            targets.append(label[1:])
        elif label.startswith(f"//{pkg}:"):
            targets.append(label[len(f"//{pkg}:") :])
    return targets


# Parse the first string literal assigned to an attribute in a GN block body.
def extract_string_attr(body, attr_name):
    m = re.search(rf'\b{re.escape(attr_name)}\s*=\s*"([^"]+)"', body)
    return m.group(1) if m else None


# Parse the first string element in a list attribute such as outputs = [ "..." ].
def extract_first_list_string(body, attr_name):
    m = re.search(rf"\b{re.escape(attr_name)}\s*=\s*\[\s*\"([^\"]+)\"", body, re.DOTALL)
    return m.group(1) if m else None


# Check a single package for host binaries generated below the BAZEL2GN SENTINEL.
def check_package(pkg):
    findings = []
    gn_rel = os.path.join(pkg, "BUILD.gn")
    bz_rel = os.path.join(pkg, "BUILD.bazel")
    gn_path = os.path.join(workdir, gn_rel)
    bz_path = os.path.join(workdir, bz_rel)
    if not (os.path.isfile(gn_path) and os.path.isfile(bz_path)):
        return findings

    try:
        gn_text = open(gn_path, encoding="utf-8", errors="replace").read()
    except OSError:
        return findings

    if SENTINEL not in gn_text:
        return findings

    pre_text, post_text = gn_text.split(SENTINEL, 1)
    sentinel_line = pre_text.count("\n")

    generated_bins = {
        b["name"]: b for b in find_gn_blocks(post_text, HOST_BIN_RULES, sentinel_line)
    }
    if not generated_bins:
        return findings

    # Check install_host_tools() above the sentinel referencing a generated binary.
    reported_bins = set()
    for inst in find_gn_blocks(pre_text, ("install_host_tools",), 0):
        for dep_name in extract_same_pkg_deps(inst["body"], pkg):
            if dep_name not in generated_bins:
                continue
            gen_bin = generated_bins[dep_name]
            reported_bins.add(dep_name)
            out_name = (
                extract_string_attr(gen_bin["body"], "output_name")
                or extract_first_list_string(inst["body"], "outputs")
                or dep_name
            )
            if gen_bin["rule"] == "go_binary":
                out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}_/{dep_name}"
            else:
                out_path = f"{{{{BAZEL_TARGET_OUT_DIR}}}}/{dep_name}"
            out_attr = (
                f'output_name = "{out_name}", ' if out_name != dep_name else ""
            )
            msg = (
                f"`{gn_rel}` keeps `install_host_tools(\"{inst['name']}\")` above "
                f"`{SENTINEL}` while `bazel2gn` generates `{gen_bin['rule']}(\"{dep_name}\")` "
                f"below the sentinel (line {gen_bin['line']}). Host tools must be built in "
                "Bazel only and brought into GN via `bazel_host_tool` instead of syncing "
                "the binary with `bazel2gn`."
            )
            remediation = (
                f"In `{gn_rel}`, replace `install_host_tools(\"{inst['name']}\")` with "
                f"`import(\"//build/bazel/bazel_host_tool.gni\")` and "
                f"`if (is_host) {{ bazel_host_tool(\"{dep_name}\") {{ "
                f'bazel_target = ":{dep_name}" '
                f'bazel_output_path = "{out_path}" '
                f"{out_attr}"
                f"install_host_tool = true }} }}` above `{SENTINEL}`, and update any "
                f'same-package group depending on `:{inst["name"]}` to depend on '
                f'`:{dep_name}($host_toolchain)` (or keep a forwarding `group("{inst["name"]}")` '
                f"if external callers reference `:{inst['name']}`). In `{bz_rel}`, place "
                f"`# @bazel2gn:skip` on the line immediately above `{dep_name}`, remove any "
                f"inner `# @bazel2gn:*` attribute comments inside `{dep_name}`, and re-run "
                f"`fx bazel2gn -d {pkg}` so `{dep_name}` is no longer generated below the "
                "sentinel."
            )
            findings.append(
                {
                    "source": "dual_built_host_tool",
                    "category": "dual_built_host_tool",
                    "severity": "ERROR",
                    "file": gn_rel,
                    "line": inst["line"],
                    "message": msg,
                    "remediation": remediation,
                }
            )

    # Check bazel_host_tool() above the sentinel that duplicates a generated binary below.
    for htool in find_gn_blocks(pre_text, ("bazel_host_tool",), 0):
        candidates = [htool["name"]]
        bz_t = extract_string_attr(htool["body"], "bazel_target")
        if bz_t:
            if bz_t.startswith(":"):
                candidates.append(bz_t[1:])
            elif bz_t.startswith(f"//{pkg}:"):
                candidates.append(bz_t[len(f"//{pkg}:") :])
        for dep_name in candidates:
            if dep_name in generated_bins and dep_name not in reported_bins:
                gen_bin = generated_bins[dep_name]
                reported_bins.add(dep_name)
                msg = (
                    f"`{gn_rel}` defines `bazel_host_tool(\"{htool['name']}\")` above "
                    f"`{SENTINEL}` (line {htool['line']}) while `{gen_bin['rule']}(\"{dep_name}\")` "
                    f"is still generated below `{SENTINEL}` (line {gen_bin['line']})."
                )
                remediation = (
                    f"In `{bz_rel}`, place `# @bazel2gn:skip` on the line immediately above "
                    f"`{dep_name}`, remove any inner `# @bazel2gn:*` attribute comments inside "
                    f"`{dep_name}`, and re-run `fx bazel2gn -d {pkg}` so `{dep_name}` is not "
                    f"generated below `{SENTINEL}` in `{gn_rel}`."
                )
                findings.append(
                    {
                        "source": "dual_built_host_tool",
                        "category": "dual_built_host_tool",
                        "severity": "ERROR",
                        "file": gn_rel,
                        "line": gen_bin["line"],
                        "message": msg,
                        "remediation": remediation,
                    }
                )

    return findings


all_findings = []
for pkg_dir in collect_candidate_packages(workdir, raw_target_dir, raw_target_dirs, raw_change_base):
    all_findings.extend(check_package(pkg_dir))

print(json.dumps(all_findings))
PYEOF
