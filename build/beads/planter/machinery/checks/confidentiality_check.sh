#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic confidentiality and commit-hygiene check for open-source
# Fuchsia CLs and shared machinery files:
# 1. Rejects internal repository/vendor subpaths and internal domain names.
# 2. Rejects Fuchsia Gerrit banned words (internal product/board codenames).
# 3. Rejects personal email addresses, user handle mentions (except Bazel repo
#    names like @bazel2gn, @platforms, @fuchsia_sdk), and individual names.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"

# Construct patterns dynamically so this file itself contains no banned tokens.
v_prefix = "//" + "vendor/"
v_goog = "vendor/" + "google"
g_three = "google" + "3"
g_plex = "google" + "plex"
c_dom = "corp." + "google.com"

INTERNAL_PATH_RE = re.compile(
    rf"(?i)({v_prefix}[a-z0-9_-]+/[a-z0-9_.-]+|\b{v_goog}\b|\b{g_three}\b|\b{g_plex}\b|\.{c_dom}\b|\b{c_dom}\b)"
)
EMAIL_RE = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
HANDLE_RE = re.compile(r"(?:^|[\s(])@([A-Za-z][A-Za-z0-9_-]{2,})\b(?!//|:)")
ALLOWED_HANDLES = {
    "bazel2gn",
    "platforms",
    "fuchsia_sdk",
    "fuchsia_build_info",
    "fuchsia_clang",
    "fuchsia_icu",
    "rules_rust",
    "rules_cc",
    "rules_python",
    "rules_go",
    "rules_license",
    "rules_fuchsia",
    "io_bazel_rules_go",
    "com_google_protobuf",
    "com_google_googletest",
    "crate_index",
    "vendor",
    "handle",
    "handles",
}
ALLOWED_HANDLE_PREFIXES = (
    "fuchsia_",
    "rules_",
    "bazel_",
    "io_bazel_",
    "com_google_",
    "internal_sdk",
)
PLACEHOLDER_NAME_RE = re.compile(r"\b(" + "Al" + "ice" + r")\b")


def git_output(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


changed = set()
for cmd in (
    ["diff", "--name-only", change_base],
    ["ls-files", "--others", "--exclude-standard"],
):
    for line in git_output(cmd).splitlines():
        if line.strip():
            changed.add(line.strip())

findings = []


def scan_text(rel_path, text):
    whole_vg_bazel = "//" + "vendor/" + "google:__subpackages__"
    whole_vg_gn = "//" + "vendor/" + "google/*"
    for idx, line in enumerate(text.splitlines(), start=1):
        sanitized = line.replace(whole_vg_bazel, "").replace(whole_vg_gn, "")
        m_path = INTERNAL_PATH_RE.search(sanitized)
        if m_path:
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": f"Confidential/internal path reference '{m_path.group(0)}' found in '{rel_path}'.",
                "remediation": (
                    "Remove internal/vendor subpath references. For vendor visibility, "
                    "always expose to the entire tree using '\"//vendor/google:__subpackages__\"'."
                ),
            })
        m_email = EMAIL_RE.search(line)
        if m_email:
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": f"Email address '{m_email.group(0)}' found in '{rel_path}'.",
                "remediation": "Remove personal email addresses and state the rule or comment generically.",
            })
        for m_h in HANDLE_RE.finditer(line):
            handle = m_h.group(1)
            h_low = handle.lower()
            if h_low not in ALLOWED_HANDLES and not h_low.startswith(ALLOWED_HANDLE_PREFIXES):
                findings.append({
                    "source": "confidentiality_check",
                    "category": "confidentiality_violation",
                    "severity": "error",
                    "file": rel_path,
                    "line": idx,
                    "message": f"User handle mention '@{handle}' found in '{rel_path}'.",
                    "remediation": "Remove user handle mentions and state the technical rule generically.",
                })
        m_name = PLACEHOLDER_NAME_RE.search(line)
        if m_name:
            findings.append({
                "source": "confidentiality_check",
                "category": "confidentiality_violation",
                "severity": "error",
                "file": rel_path,
                "line": idx,
                "message": f"Person/reviewer name '{m_name.group(0)}' found in '{rel_path}'.",
                "remediation": "Never name specific reviewers or individuals in comments, commit messages, or lessons.",
            })


for rel in sorted(changed):
    if rel.endswith("confidentiality_check.sh"):
        continue
    full = os.path.join(workdir, rel)
    if not os.path.isfile(full):
        continue
    diff_out = git_output(["diff", "-U0", change_base, "--", rel])
    added_lines = []
    for l in diff_out.splitlines():
        if l.startswith("+") and not l.startswith("+++"):
            added_lines.append(l[1:])
    if added_lines:
        scan_text(rel, "\n".join(added_lines))

head_rev = git_output(["rev-parse", "--verify", "HEAD"])
base_rev = git_output(["rev-parse", "--verify", change_base])
commit_msg = git_output(["log", "-1", "--format=%B", "HEAD"]).rstrip("\n")
if commit_msg and head_rev and base_rev and head_rev != base_rev:
    scan_text("COMMIT_MSG", commit_msg)
    msg_lines = commit_msg.splitlines()
    if msg_lines:
        subject = msg_lines[0].strip()
        if len(subject) > 65:
            findings.append({
                "source": "confidentiality_check",
                "category": "commit_msg_subject_length",
                "severity": "error",
                "file": "COMMIT_MSG",
                "line": 1,
                "message": (
                    f"commit_msg: Subject line exceeds 65 characters ({len(subject)} chars). "
                    "Limit first line to 50-65 characters."
                ),
                "remediation": (
                    "Amend the HEAD commit message so the subject line is at most 65 characters "
                    "(for example, omit trailing 'to Bazel' or move detailed crate lists to the body)."
                ),
            })
        trailer_re = re.compile(r"^[A-Za-z0-9-]+:\s")
        for idx, line in enumerate(msg_lines[1:], start=2):
            stripped = line.strip()
            if not stripped or trailer_re.match(stripped) or "://" in stripped:
                continue
            if len(line) > 72:
                findings.append({
                    "source": "confidentiality_check",
                    "category": "commit_msg_line_length",
                    "severity": "error",
                    "file": "COMMIT_MSG",
                    "line": idx,
                    "message": (
                        f"commit_msg: Body line {idx} exceeds 72 characters ({len(line)} chars). "
                        "Wrap commit message body lines at <= 72 characters."
                    ),
                    "remediation": "Wrap all body lines of the HEAD commit message at 72 characters or fewer.",
                })

print(json.dumps(findings, indent=2))
PYEOF
