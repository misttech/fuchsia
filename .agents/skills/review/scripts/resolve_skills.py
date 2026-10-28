#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Resolves Fuchsia review skills from .agents/code-review/agent_configs.textproto.

This mirrors how the Gerrit CL Review Agent (via CRACS) picks skills, so local
reviews load the same Fuchsia skills as the hosted agent:

  * An entry applies to a file when its `include_filters` match: the project is
    listed (or no project is listed) and the repo-relative path fully matches
    one of the `path_regex` values (RE2 full-match semantics), and no
    `exclude_filters` block matches it the same way.
  * A skill without "/" resolves to `.agents/skills/<name>/SKILL.md`; a skill
    with "/" resolves to `<repo root>/<skill>/SKILL.md`, or to
    `<repo root>/<skill>` if it already ends in `/SKILL.md`.
  * Entries without skills (e.g. the default `use_default_subagents` entry) add
    nothing here; the review skill's `general` task covers that.
  * With `--project=vendor/google`, entries from
    `vendor/google/.agents/code-review/agent_configs.textproto` are used too
    (when that file exists), with their skills resolved relative to
    `vendor/google` instead of the fuchsia.git root.

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
VENDOR_PROJECT = "vendor/google"
VENDOR_DIR = os.path.join("vendor", "google")

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


def _filter_matches(flt, path, project):
    """True if one include/exclude filter block matches `path`."""
    projects = flt.get("project", [])
    if projects and project not in projects:
        return False
    regexes = flt.get("path_regex", [])
    return not regexes or any(re.fullmatch(rx, path) for rx in regexes)


def entry_matches(entry, path, project):
    """True if `entry` applies to `path`.

    Like CRACS: any include_filters block must match and no exclude_filters
    block may match.
    """
    if not any(
        _filter_matches(flt, path, project)
        for flt in entry.get("include_filters", [])
    ):
        return False
    return not any(
        _filter_matches(flt, path, project)
        for flt in entry.get("exclude_filters", [])
    )


def skill_file(skill, repo_root):
    if skill.endswith("/SKILL.md"):
        # Already names the file; don't append another SKILL.md.
        return os.path.join(repo_root, skill)
    if "/" in skill:
        return os.path.join(repo_root, skill, "SKILL.md")
    return os.path.join(repo_root, ".agents", "skills", skill, "SKILL.md")


def resolve(config_text, files, repo_root, project="fuchsia", extra_configs=()):
    """Returns `{"skill_path", "files"}` review tasks for `files`.

    `config_text` is the fuchsia.git config, whose skills resolve relative to
    `repo_root`. `extra_configs` is a sequence of `(config_text, skill_root)`
    pairs for other repositories' configs, whose skills resolve relative to
    `skill_root`.
    """
    entries = []
    for text, skill_root in [(config_text, repo_root), *extra_configs]:
        for entry in parse_textproto(text).get("configs", []):
            entries.append((entry, skill_root))
    by_skill = {}
    for entry, skill_root in entries:
        skills = entry.get("skills", [])
        if not skills:
            continue
        matched = [f for f in files if entry_matches(entry, f, project)]
        if not matched:
            continue
        for skill in skills:
            dest = by_skill.setdefault(skill_file(skill, skill_root), [])
            for f in matched:
                if f not in dest:
                    dest.append(f)
    return [
        {"skill_path": path, "files": sorted(paths)}
        for path, paths in sorted(by_skill.items())
    ]


def extra_configs(root, project):
    """Returns `(config_text, skill_root)` pairs to use besides fuchsia.git's.

    For vendor/google changes this is vendor/google's own config, if present.
    """
    if project != VENDOR_PROJECT:
        return []
    vendor_root = os.path.join(root, VENDOR_DIR)
    vendor_config = os.path.join(vendor_root, CONFIG)
    if not os.path.isfile(vendor_config):
        return []
    with open(vendor_config, encoding="utf-8") as f:
        return [(f.read(), vendor_root)]


def _repo_root(start):
    cur = os.path.abspath(start)
    while True:
        # vendor/google has its own config; keep going up to fuchsia.git.
        is_vendor = cur.endswith(os.sep + VENDOR_DIR) and os.path.isfile(
            os.path.join(os.path.dirname(os.path.dirname(cur)), CONFIG)
        )
        if os.path.isfile(os.path.join(cur, CONFIG)) and not is_vendor:
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
    results = resolve(
        text,
        files,
        root,
        project=args.project,
        extra_configs=extra_configs(root, args.project),
    )
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
