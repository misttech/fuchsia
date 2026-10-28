#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Resolves Fuchsia review skills from .agents/code-review/agent_configs.textproto.

This mirrors how the Gerrit CL Review Agent (via CRACS) picks skills, so local
reviews load the same Fuchsia skills as the hosted agent:

  * An entry applies to a file when its `include_filters` match: the project is
    listed (or no project is listed) and the repo-relative path fully matches
    one of the `path_regex` values (RE2 full-match semantics).
  * A skill without "/" resolves to `.agents/skills/<name>/SKILL.md`; a skill
    with "/" resolves to `<repo root>/<skill>/SKILL.md`.
  * Entries without skills (e.g. the default `use_default_subagents` entry) add
    nothing here; the review skill's `general` task covers that.

Output is a JSON array of
`{"skill_path": "<absolute SKILL.md path>", "files": ["<repo-relative path>", ...]}`.

Usage:
  resolve_skills.py --files=src/foo.rs,docs/bar.md [--repo_root=DIR] [--project=fuchsia]
"""

import argparse
import json
import os
import re
import sys

CONFIG = os.path.join(".agents", "code-review", "agent_configs.textproto")

_TOKEN_RE = re.compile(
    r"\s*(?:(#[^\n]*)|([A-Za-z_][A-Za-z0-9_]*)\s*:?\s*(\{)|"
    r'([A-Za-z_][A-Za-z0-9_]*)\s*:\s*("(?:[^"\\]|\\.)*"|[^\s{}]+)|(\}))'
)


def _unquote(value):
    if value.startswith('"'):
        # Text proto strings use C-style escapes; this is enough for the
        # characters used in agent_configs.textproto (\\ and \").
        return re.sub(r"\\(.)", r"\1", value[1:-1])
    return value


def parse_textproto(text):
    """Parses the subset of text proto used by agent_configs.textproto.

    Returns a dict mapping field name -> list of values, where a value is
    either a string or a nested dict of the same shape.
    """
    root = {}
    stack = [root]
    pos = 0
    while pos < len(text):
        m = _TOKEN_RE.match(text, pos)
        if not m:
            if text[pos:].strip():
                raise ValueError(
                    f"cannot parse text proto near: {text[pos:pos + 40]!r}"
                )
            break
        pos = m.end()
        comment, msg_name, _, field, value, close = m.groups()
        if comment:
            continue
        if msg_name:
            child = {}
            stack[-1].setdefault(msg_name, []).append(child)
            stack.append(child)
        elif field:
            stack[-1].setdefault(field, []).append(_unquote(value))
        elif close:
            stack.pop()
    if len(stack) != 1:
        raise ValueError("unbalanced braces in text proto")
    return root


def entry_matches(entry, path, project):
    """True if any include_filters block of `entry` matches `path`."""
    for flt in entry.get("include_filters", []):
        projects = flt.get("project", [])
        if projects and project not in projects:
            continue
        regexes = flt.get("path_regex", [])
        if not regexes:
            return True
        if any(re.fullmatch(rx, path) for rx in regexes):
            return True
    return False


def skill_file(skill, repo_root):
    if "/" in skill:
        return os.path.join(repo_root, skill, "SKILL.md")
    return os.path.join(repo_root, ".agents", "skills", skill, "SKILL.md")


def resolve(config_text, files, repo_root, project="fuchsia"):
    """Returns `{"skill_path", "files"}` review tasks for `files`."""
    configs = parse_textproto(config_text).get("configs", [])
    by_skill = {}
    for entry in configs:
        skills = entry.get("skills", [])
        if not skills:
            continue
        matched = [f for f in files if entry_matches(entry, f, project)]
        if not matched:
            continue
        for skill in skills:
            dest = by_skill.setdefault(skill_file(skill, repo_root), [])
            for f in matched:
                if f not in dest:
                    dest.append(f)
    return [
        {"skill_path": path, "files": sorted(paths)}
        for path, paths in sorted(by_skill.items())
    ]


def _repo_root(start):
    cur = os.path.abspath(start)
    while True:
        if os.path.isfile(os.path.join(cur, CONFIG)):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            raise SystemExit(f"could not find {CONFIG} above {start}")
        cur = parent


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--files", required=True, help="comma-separated repo-relative paths"
    )
    ap.add_argument(
        "--repo_root",
        default=None,
        help="fuchsia.git root (default: found from the current dir)",
    )
    ap.add_argument(
        "--project",
        default="fuchsia",
        help="Gerrit project the files belong to",
    )
    args = ap.parse_args(argv)
    root = (
        os.path.abspath(args.repo_root)
        if args.repo_root
        else _repo_root(os.getcwd())
    )
    with open(os.path.join(root, CONFIG), encoding="utf-8") as f:
        text = f.read()
    files = [p.strip().lstrip("/") for p in args.files.split(",") if p.strip()]
    results = resolve(text, files, root, project=args.project)
    missing = [
        r["skill_path"] for r in results if not os.path.isfile(r["skill_path"])
    ]
    for path in missing:
        print(f"warning: skill file not found: {path}", file=sys.stderr)
    json.dump(results, sys.stdout, indent=2)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
