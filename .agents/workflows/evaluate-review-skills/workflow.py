# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Jetski Dynamic Workflow: Evaluate Fuchsia in-tree skills for AI code review.

Discovers SKILL.md files across fuchsia.git and/or vendor/google, resolves
existing registrations from .agents/code-review/agent_configs.textproto
(fuchsia.git and, when present, vendor/google) using the same semantics as
.agents/skills/review/scripts/resolve_skills.py and Gerrit CRACS, and evaluates
candidate skills in parallel against three strict criteria:
  1. Rubric vs. Generator/Workflow (must define diff-checkable review rules, not
     scaffolding generators, CLI wrappers, or operational triage/merge runbooks).
  2. Config Placement & Full-Match Path Regex Accuracy (must target the right
     agent_configs.textproto file and use RE2 full-match path_regex rules that
     match real files in git ls-files without matching unintended paths).
  3. Commit Volume Threshold (target paths must meet min_commits since
     since_date to justify review overhead).

Then runs deterministic git ls-files regex validation, an adversarial
cross-check pass, and outputs ready-to-paste configs: { ... } textproto blocks
grouped by target config file.
"""

import calendar
import datetime
import json
import os
import re
import subprocess

try:
    from advanced import parallel as _adv_parallel
except ImportError:
    _adv_parallel = None

CONFIG_REL = os.path.join(".agents", "code-review", "agent_configs.textproto")
FUCHSIA_CONFIG_LABEL = ".agents/code-review/agent_configs.textproto"
VENDOR_PROJECT = "vendor/google"
VENDOR_DIR = os.path.join("vendor", "google")
VENDOR_CONFIG_LABEL = (
    "vendor/google/.agents/code-review/agent_configs.textproto"
)
REQUIRED_TAG = "code-review-agent"

_TOKEN_RE = re.compile(
    r"\s*(?:(#[^\n]*)|([A-Za-z_][A-Za-z0-9_]*)\s*:?\s*(\{)|"
    r'([A-Za-z_][A-Za-z0-9_]*)\s*:\s*("(?:[^"\\]|\\.)*"|[^\s{}]+)|(\}))'
)


def _run_parallel(items, fn, max_concurrency):
    if _adv_parallel is not None:
        return _adv_parallel(items, fn, max_concurrency=max_concurrency)
    return parallel(items, fn)


def _git_lines(repo_root, *git_args):
    cmd = ["git", "-C", repo_root] + list(git_args)
    out = subprocess.check_output(cmd, text=True, stderr=subprocess.DEVNULL)
    return [line.strip() for line in out.splitlines() if line.strip()]


def _discover_repo_files(repo_root, pattern=None):
    pattern_args = [pattern] if pattern else []
    tracked = _git_lines(repo_root, "ls-files", *pattern_args)
    untracked = _git_lines(
        repo_root,
        "ls-files",
        "--others",
        "--exclude-standard",
        *pattern_args,
    )
    return sorted(set(tracked + untracked))


def _read_repo_file(repo_root, rel_path):
    abs_path = os.path.join(repo_root, rel_path)
    if os.path.isfile(abs_path):
        with open(abs_path, encoding="utf-8") as f:
            return f.read()
    try:
        return subprocess.check_output(
            ["git", "-C", repo_root, "show", f"HEAD:{rel_path}"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return ""


def _default_since_date(today=None):
    today = today or datetime.date.today()
    year = today.year
    month = today.month - 2
    if month <= 0:
        month += 12
        year -= 1
    max_day = calendar.monthrange(year, month)[1]
    return datetime.date(year, month, min(today.day, max_day)).isoformat()


def _unquote(value):
    if value.startswith('"'):
        return re.sub(r"\\(.)", r"\1", value[1:-1])
    return value


def parse_textproto(text):
    """Parses the subset of text proto used by agent_configs.textproto.

    Mirrors .agents/skills/review/scripts/resolve_skills.py.
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
    """True if `entry` applies to `path` under CRACS / resolve_skills rules."""
    if not any(
        _filter_matches(flt, path, project)
        for flt in entry.get("include_filters", [])
    ):
        return False
    return not any(
        _filter_matches(flt, path, project)
        for flt in entry.get("exclude_filters", [])
    )


def skill_file(skill, skill_root):
    """Resolves a config `skills` value against its config repository root."""
    if skill.endswith("/SKILL.md"):
        return os.path.join(skill_root, skill)
    if "/" in skill:
        return os.path.join(skill_root, skill, "SKILL.md")
    return os.path.join(skill_root, ".agents", "skills", skill, "SKILL.md")


def canonical_skill_ref(skill_dir):
    """Formats a repo-relative skill directory for that repo's textproto."""
    norm_dir = skill_dir.strip("/")
    if os.path.dirname(norm_dir) == ".agents/skills":
        return os.path.basename(norm_dir)
    return norm_dir


def _normalize_parsed_entry(entry, config_file):
    return {
        "config_file": config_file,
        "id": (entry.get("id") or [""])[0],
        "display_name": (entry.get("display_name") or [""])[0],
        "description": (entry.get("description") or [""])[0],
        "tags": list(entry.get("tags") or []),
        "use_default_subagents": (
            (entry.get("use_default_subagents") or ["false"])[0] == "true"
        ),
        "skills": list(entry.get("skills") or []),
        "include_filters": [
            {
                "projects": list(flt.get("project") or []),
                "path_regexes": list(flt.get("path_regex") or []),
            }
            for flt in entry.get("include_filters") or []
        ],
        "exclude_filters": [
            {
                "projects": list(flt.get("project") or []),
                "path_regexes": list(flt.get("path_regex") or []),
            }
            for flt in entry.get("exclude_filters") or []
        ],
    }


def _entry_to_raw(normalized_entry):
    return {
        "id": [normalized_entry.get("id", "")],
        "tags": list(normalized_entry.get("tags") or []),
        "skills": list(normalized_entry.get("skills") or []),
        "include_filters": [
            {
                "project": list(flt.get("projects") or []),
                "path_regex": list(flt.get("path_regexes") or []),
            }
            for flt in normalized_entry.get("include_filters") or []
        ],
        "exclude_filters": [
            {
                "project": list(flt.get("projects") or []),
                "path_regex": list(flt.get("path_regexes") or []),
            }
            for flt in normalized_entry.get("exclude_filters") or []
        ],
    }


def load_agent_configs(workspace_root):
    """Loads fuchsia.git and (if present) vendor/google agent_configs.textproto."""
    vendor_root = os.path.join(workspace_root, VENDOR_DIR)
    has_vendor_google = os.path.isdir(vendor_root) and (
        os.path.exists(os.path.join(vendor_root, ".git"))
        or os.path.isfile(os.path.join(vendor_root, CONFIG_REL))
    )

    config_specs = [(FUCHSIA_CONFIG_LABEL, workspace_root, "fuchsia")]
    if has_vendor_google:
        config_specs.append((VENDOR_CONFIG_LABEL, vendor_root, VENDOR_PROJECT))

    parsed_by_config = {}
    entries_by_skill_abs = {}
    all_entry_ids = {}

    for config_label, skill_root, _ in config_specs:
        raw_text = _read_repo_file(skill_root, CONFIG_REL)
        parsed_entries = []
        if raw_text:
            for raw_entry in parse_textproto(raw_text).get("configs", []):
                norm = _normalize_parsed_entry(raw_entry, config_label)
                parsed_entries.append(norm)
                if norm["id"]:
                    all_entry_ids[norm["id"]] = config_label
                # Entries without skills (e.g. use_default_subagents: true)
                # carry no skills and are skipped during skill indexing.
                for skill_ref in norm["skills"]:
                    resolved_abs = os.path.normpath(
                        skill_file(skill_ref, skill_root)
                    )
                    entries_by_skill_abs.setdefault(resolved_abs, []).append(
                        norm
                    )
        parsed_by_config[config_label] = parsed_entries

    return {
        "has_vendor_google": has_vendor_google,
        "vendor_root": vendor_root,
        "parsed_by_config": parsed_by_config,
        "entries_by_skill_abs": entries_by_skill_abs,
        "all_entry_ids": all_entry_ids,
    }


def discover_candidates(
    workspace_root,
    repo_arg="all",
    path_prefix="",
    only_unregistered=True,
    config_state=None,
):
    """Discovers SKILL.md files and matches them against agent_configs.textproto."""
    config_state = config_state or load_agent_configs(workspace_root)
    has_vendor_google = config_state["has_vendor_google"]
    vendor_root = config_state["vendor_root"]
    entries_by_skill_abs = config_state["entries_by_skill_abs"]

    repos = []
    missing_repos = []
    if repo_arg in ("fuchsia", "all"):
        repos.append(("fuchsia", workspace_root))
    if repo_arg == "all":
        if has_vendor_google:
            repos.append((VENDOR_PROJECT, vendor_root))
    elif repo_arg == VENDOR_PROJECT:
        if has_vendor_google:
            repos.append((VENDOR_PROJECT, vendor_root))
        else:
            missing_repos.append(repo_arg)
    elif repo_arg != "fuchsia":
        missing_repos.append(repo_arg)

    candidates = []
    for repo_label, repo_root in repos:
        skill_rels = _discover_repo_files(repo_root, "*SKILL.md")
        if path_prefix:
            skill_rels = [
                p
                for p in skill_rels
                if p.startswith(path_prefix + "/") or p == path_prefix
            ]

        default_config = (
            FUCHSIA_CONFIG_LABEL
            if repo_label == "fuchsia"
            else VENDOR_CONFIG_LABEL
        )

        for skill_rel in skill_rels:
            skill_dir = os.path.dirname(skill_rel)
            skill_name = os.path.basename(skill_dir)
            skill_abs = os.path.normpath(os.path.join(repo_root, skill_rel))
            existing_entries = entries_by_skill_abs.get(skill_abs, [])
            if only_unregistered and existing_entries:
                continue
            skill_body = _read_repo_file(repo_root, skill_rel)
            candidates.append(
                {
                    "repo_label": repo_label,
                    "repo_root": repo_root,
                    "skill_rel": skill_rel,
                    "skill_dir": skill_dir,
                    "skill_name": skill_name,
                    "skill_abs": skill_abs,
                    "canonical_skill_ref": canonical_skill_ref(skill_dir),
                    "default_config_file": default_config,
                    "skill_body": skill_body,
                    "existing_entries": existing_entries,
                    "already_registered": bool(existing_entries),
                }
            )

    return candidates, missing_repos


def _sample_spread(paths, limit=8):
    if len(paths) <= limit:
        return list(paths)
    step = max(1, (len(paths) - 1) // (limit - 1))
    picked = [paths[i * step] for i in range(limit - 1)]
    picked.append(paths[-1])
    return picked


def validate_proposed_entry(
    entry, repo_files, workspace_root, has_vendor_google
):
    """Validates a proposed textproto entry against git ls-files of each repo."""
    errors = []
    warnings = []
    config_file = entry.get("config_file", "")
    valid_configs = [FUCHSIA_CONFIG_LABEL] + (
        [VENDOR_CONFIG_LABEL] if has_vendor_google else []
    )
    if config_file not in valid_configs:
        errors.append(
            f"Invalid config_file {config_file!r}; allowed: {valid_configs}."
        )

    if config_file == VENDOR_CONFIG_LABEL:
        owning_repo = VENDOR_PROJECT
        skill_root = os.path.join(workspace_root, VENDOR_DIR)
    else:
        owning_repo = "fuchsia"
        skill_root = workspace_root

    tags = entry.get("tags") or []
    if REQUIRED_TAG not in tags:
        errors.append(
            f"Missing required tag {REQUIRED_TAG!r} in tags={tags!r}."
        )

    skills = entry.get("skills") or []
    if not skills:
        errors.append("Entry must list at least one skill in `skills`.")
    owning_file_set = set(repo_files.get(owning_repo, []))
    for skill_ref in skills:
        if config_file == FUCHSIA_CONFIG_LABEL and skill_ref.startswith(
            "vendor/"
        ):
            errors.append(
                f"fuchsia.git config cannot reference vendor skill {skill_ref!r};"
                f" move entry to {VENDOR_CONFIG_LABEL}."
            )
            continue
        resolved_abs = os.path.normpath(skill_file(skill_ref, skill_root))
        rel_to_owner = os.path.relpath(resolved_abs, skill_root)
        if (
            not os.path.isfile(resolved_abs)
            and rel_to_owner not in owning_file_set
        ):
            errors.append(
                f"Skill {skill_ref!r} resolves to missing file"
                f" {rel_to_owner!r} in {owning_repo}."
            )

    include_filters = entry.get("include_filters") or []
    exclude_filters = entry.get("exclude_filters") or []
    if not include_filters:
        errors.append("Entry must define at least one include_filters block.")

    for kind, blocks in (
        ("include_filters", include_filters),
        ("exclude_filters", exclude_filters),
    ):
        for idx, flt in enumerate(blocks):
            regexes = flt.get("path_regexes") or []
            if kind == "include_filters" and not regexes:
                errors.append(
                    f"include_filters[{idx}] must specify at least one"
                    " path_regex."
                )
            for rx in regexes:
                try:
                    re.compile(rx)
                except re.error as exc:
                    errors.append(
                        f"Invalid regex {rx!r} in {kind}[{idx}]: {exc}"
                    )

    if errors:
        return {
            "valid": False,
            "errors": errors,
            "warnings": warnings,
            "matched_counts": {},
            "matched_top_dirs": {},
            "sample_matches": {},
            "per_regex_counts": [],
        }

    raw_entry = _entry_to_raw(entry)
    available_projects = [
        p for p in ("fuchsia", VENDOR_PROJECT) if p in repo_files
    ]
    target_projects = []
    per_regex_counts = []

    for idx, flt in enumerate(include_filters):
        projects = list(flt.get("projects") or [])
        if config_file == VENDOR_CONFIG_LABEL:
            if any(p != VENDOR_PROJECT for p in projects):
                errors.append(
                    f"include_filters[{idx}] in {VENDOR_CONFIG_LABEL} lists"
                    f" projects={projects!r}; vendor/google config only applies"
                    f" to {VENDOR_PROJECT!r}."
                )
            eff_projects = (
                [VENDOR_PROJECT] if VENDOR_PROJECT in repo_files else []
            )
        else:
            eff_projects = projects if projects else list(available_projects)

        for p in eff_projects:
            if p not in target_projects:
                target_projects.append(p)

        checked_projects = [p for p in eff_projects if p in repo_files]
        unchecked_projects = [p for p in eff_projects if p not in repo_files]
        for rx in flt.get("path_regexes") or []:
            rx_counts = {}
            for proj in checked_projects:
                matched_rx = [
                    path
                    for path in repo_files[proj]
                    if re.fullmatch(rx, path)
                    and not any(
                        _filter_matches(ex, path, proj)
                        for ex in raw_entry["exclude_filters"]
                    )
                ]
                rx_counts[proj] = len(matched_rx)
            per_regex_counts.append(
                {
                    "filter_index": idx,
                    "path_regex": rx,
                    "counts_by_project": rx_counts,
                }
            )
            if checked_projects and sum(rx_counts.values()) == 0:
                if unchecked_projects:
                    warnings.append(
                        f"path_regex {rx!r} in include_filters[{idx}] matched 0"
                        f" files in {checked_projects} (may target"
                        f" unchecked-out {unchecked_projects})."
                    )
                else:
                    errors.append(
                        f"path_regex {rx!r} in include_filters[{idx}] matched 0"
                        f" files via git ls-files across {checked_projects}."
                    )

    matched_counts = {}
    matched_top_dirs = {}
    sample_matches = {}
    for proj in target_projects:
        if proj not in repo_files:
            warnings.append(
                f"Project {proj!r} is not checked out locally; skipped"
                " git ls-files check for it."
            )
            continue
        matched = [
            path
            for path in repo_files[proj]
            if entry_matches(raw_entry, path, proj)
        ]
        matched_counts[proj] = len(matched)
        top_counts = {}
        for path in matched:
            top = path.split("/", 1)[0] if "/" in path else path
            top_counts[top] = top_counts.get(top, 0) + 1
        matched_top_dirs[proj] = dict(
            sorted(top_counts.items(), key=lambda kv: (-kv[1], kv[0]))[:15]
        )
        sample_matches[proj] = _sample_spread(matched, limit=8)
        if len(matched) == 0:
            warnings.append(
                f"Entry matched 0 files in project {proj!r} via git ls-files."
            )

    if matched_counts and sum(matched_counts.values()) == 0:
        errors.append(
            f"Entry matched 0 files across all checked-out target projects"
            f" ({list(matched_counts.keys())})."
        )

    return {
        "valid": len(errors) == 0,
        "errors": errors,
        "warnings": warnings,
        "matched_counts": matched_counts,
        "matched_top_dirs": matched_top_dirs,
        "sample_matches": sample_matches,
        "per_regex_counts": per_regex_counts,
    }


def _escape_textproto_str(value):
    return value.replace("\\", "\\\\").replace('"', '\\"')


def format_textproto_entry(entry):
    """Formats a normalized entry as a ready-to-paste `configs: { ... }` block."""
    lines = ["configs: {"]
    lines.append(f'  id: "{_escape_textproto_str(entry["id"])}"')
    if entry.get("display_name"):
        lines.append(
            f'  display_name: "{_escape_textproto_str(entry["display_name"])}"'
        )
    if entry.get("description"):
        lines.append(
            f'  description: "{_escape_textproto_str(entry["description"])}"'
        )
    for tag in entry.get("tags") or [REQUIRED_TAG]:
        lines.append(f'  tags: "{_escape_textproto_str(tag)}"')
    for skill_ref in entry.get("skills") or []:
        lines.append(f'  skills: "{_escape_textproto_str(skill_ref)}"')
    for flt in entry.get("include_filters") or []:
        lines.append("  include_filters: {")
        for proj in flt.get("projects") or []:
            lines.append(f'    project: "{_escape_textproto_str(proj)}"')
        for rx in flt.get("path_regexes") or []:
            lines.append(f'    path_regex: "{_escape_textproto_str(rx)}"')
        lines.append("  }")
    for flt in entry.get("exclude_filters") or []:
        if not (flt.get("projects") or flt.get("path_regexes")):
            continue
        lines.append("  exclude_filters: {")
        for proj in flt.get("projects") or []:
            lines.append(f'    project: "{_escape_textproto_str(proj)}"')
        for rx in flt.get("path_regexes") or []:
            lines.append(f'    path_regex: "{_escape_textproto_str(rx)}"')
        lines.append("  }")
    lines.append("}")
    rendered = "\n".join(lines)
    # Verify round-trip parseability with the resolver's parser.
    parse_textproto(rendered)
    return rendered


def render_config_blocks_markdown(
    proposed_entries, repo_files, workspace_root, has_vendor_google
):
    """Renders ready-to-paste `configs: { ... }` blocks grouped by config file."""
    actionable_by_config = {}
    removals_by_config = {}
    validation_issues = []

    for item in proposed_entries:
        action = item.get("action", "")
        entry = item.get("entry") or {}
        cfg = entry.get("config_file", FUCHSIA_CONFIG_LABEL)
        if action in ("ADD", "UPDATE"):
            check = validate_proposed_entry(
                entry, repo_files, workspace_root, has_vendor_google
            )
            if not check["valid"]:
                validation_issues.append(
                    f"- `{entry.get('id', '<unknown>')}` ({cfg}): "
                    + "; ".join(check["errors"])
                )
            actionable_by_config.setdefault(cfg, []).append((action, entry))
        elif action == "REMOVE":
            removals_by_config.setdefault(cfg, []).append(
                (entry.get("id", "<unknown>"), item.get("rationale", ""))
            )

    sections = ["## Ready-to-Paste `agent_configs.textproto` Blocks"]
    if not actionable_by_config and not removals_by_config:
        sections.append(
            "\nNo `ADD`, `UPDATE`, or `REMOVE` changes recommended."
        )
        return "\n".join(sections)

    ordered_configs = [FUCHSIA_CONFIG_LABEL, VENDOR_CONFIG_LABEL]
    for cfg in ordered_configs:
        items = actionable_by_config.get(cfg, [])
        removals = removals_by_config.get(cfg, [])
        if not items and not removals:
            continue
        sections.append(f"\n### `{cfg}`")
        if items:
            blocks = []
            for action, entry in items:
                blocks.append(
                    f"# Action: {action}\n{format_textproto_entry(entry)}"
                )
            sections.append("```textproto\n" + "\n\n".join(blocks) + "\n```")
        if removals:
            sections.append("\nEntries to remove:")
            for entry_id, reason in removals:
                sections.append(f"- `{entry_id}`: {reason}")

    if validation_issues:
        sections.append("\n### Validation Warnings")
        sections.extend(validation_issues)

    return "\n".join(sections)


FILTER_BLOCK_SCHEMA = {
    "type": "object",
    "properties": {
        "projects": {
            "type": "array",
            "items": {"type": "string", "enum": ["fuchsia", VENDOR_PROJECT]},
            "description": (
                "Gerrit projects matched by this filter block ('fuchsia' and/or"
                " 'vendor/google'; leave empty only when intentionally matching"
                " every project on the host)."
            ),
        },
        "path_regexes": {
            "type": "array",
            "items": {"type": "string"},
            "description": (
                "RE2 full-match regular expressions matched against the entire"
                " repo-relative path via re.fullmatch (e.g."
                " 'src/storage/fxfs/.*\\.(rs|gn)'). Write plain regex strings,"
                " not textproto double-escaped strings."
            ),
        },
    },
    "required": ["projects", "path_regexes"],
}

TEXTPROTO_ENTRY_SCHEMA = {
    "type": "object",
    "properties": {
        "config_file": {
            "type": "string",
            "enum": [FUCHSIA_CONFIG_LABEL, VENDOR_CONFIG_LABEL],
            "description": (
                "Target config file: '.agents/code-review/agent_configs.textproto'"
                " for public fuchsia.git skills, or"
                " 'vendor/google/.agents/code-review/agent_configs.textproto'"
                " for vendor/google-only skills."
            ),
        },
        "id": {
            "type": "string",
            "description": (
                "Unique kebab-case entry id across both fuchsia.git and"
                " vendor/google configs."
            ),
        },
        "display_name": {"type": "string"},
        "description": {"type": "string"},
        "tags": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Must include 'code-review-agent'.",
        },
        "skills": {
            "type": "array",
            "items": {"type": "string"},
            "description": (
                "Skill reference(s): '<name>' (no '/') for"
                " '<config_repo_root>/.agents/skills/<name>', otherwise"
                " '<skill_dir>' relative to the repo whose config lists it."
            ),
        },
        "include_filters": {
            "type": "array",
            "items": FILTER_BLOCK_SCHEMA,
        },
        "exclude_filters": {
            "type": "array",
            "items": FILTER_BLOCK_SCHEMA,
        },
    },
    "required": [
        "config_file",
        "id",
        "display_name",
        "description",
        "tags",
        "skills",
        "include_filters",
        "exclude_filters",
    ],
}

EVAL_SCHEMA = {
    "type": "object",
    "properties": {
        "repo": {"type": "string"},
        "skill_dir": {"type": "string"},
        "skill_name": {"type": "string"},
        "classification": {
            "type": "string",
            "enum": [
                "review_rubric",
                "mixed_authoring_and_review",
                "scaffolding_or_generator",
                "cli_or_test_runner",
                "triage_or_debugging_workflow",
                "merge_or_sync_workflow",
            ],
        },
        "is_review_suitable": {"type": "boolean"},
        "rubric_rules_summary": {
            "type": "array",
            "items": {"type": "string"},
            "description": (
                "Concrete diff-checkable invariants/anti-patterns found in the"
                " full SKILL.md body (empty if not a review rubric)."
            ),
        },
        "target_modified_paths": {
            "type": "array",
            "items": {"type": "string"},
            "description": (
                "Repo-root-relative directories or file patterns where diffs"
                " governed by this skill occur."
            ),
        },
        "commits_since_cutoff": {
            "type": "integer",
            "description": (
                "Number of git commits touching target_modified_paths since"
                " since_date."
            ),
        },
        "verdict": {
            "type": "string",
            "enum": ["ADD", "KEEP", "UPDATE", "REMOVE", "SKIP"],
        },
        "recommended_entries": {
            "type": "array",
            "items": TEXTPROTO_ENTRY_SCHEMA,
            "description": (
                "Proposed agent_configs.textproto entry (1 item for ADD, KEEP,"
                " UPDATE, or REMOVE; empty [] for SKIP)."
            ),
        },
        "rationale": {"type": "string"},
    },
    "required": [
        "repo",
        "skill_dir",
        "skill_name",
        "classification",
        "is_review_suitable",
        "rubric_rules_summary",
        "target_modified_paths",
        "commits_since_cutoff",
        "verdict",
        "recommended_entries",
        "rationale",
    ],
}

AUDIT_SCHEMA = {
    "type": "object",
    "properties": {
        "audited_verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "skill_dir": {"type": "string"},
                    "skill_name": {"type": "string"},
                    "initial_verdict": {"type": "string"},
                    "final_verdict": {
                        "type": "string",
                        "enum": ["ADD", "KEEP", "UPDATE", "REMOVE", "SKIP"],
                    },
                    "target_config_file": {"type": "string"},
                    "entry_id": {"type": "string"},
                    "commits_since_cutoff": {"type": "integer"},
                    "override_reason": {"type": "string"},
                },
                "required": [
                    "repo",
                    "skill_dir",
                    "skill_name",
                    "initial_verdict",
                    "final_verdict",
                    "target_config_file",
                    "entry_id",
                    "commits_since_cutoff",
                    "override_reason",
                ],
            },
        },
        "proposed_entries": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["ADD", "KEEP", "UPDATE", "REMOVE"],
                    },
                    "entry": TEXTPROTO_ENTRY_SCHEMA,
                    "rationale": {"type": "string"},
                },
                "required": ["action", "entry", "rationale"],
            },
            "description": (
                "Audited agent_configs.textproto entries for all ADD, KEEP,"
                " UPDATE, and REMOVE verdicts."
            ),
        },
        "executive_summary_markdown": {
            "type": "string",
            "description": (
                "Concise Markdown summary tables for (1) Actionable ADD/UPDATE/"
                "REMOVE verdicts, (2) Verified KEEP skills, and (3) Rejected"
                " SKIP skills with commit counts and reasons. Do not embed raw"
                " textproto blocks here; Phase 4 appends them from"
                " proposed_entries."
            ),
        },
    },
    "required": [
        "audited_verdicts",
        "proposed_entries",
        "executive_summary_markdown",
    ],
}


async def _run_workflow(workflow_args):
    # -----------------------------------------------------------------------
    # Phase 1: Discover Skills & Existing agent_configs.textproto Entries
    # -----------------------------------------------------------------------
    phase("Discover Skills & Existing Configs")

    ws = globals().get("WORKSPACES")
    workspace_root = workflow_args.get("workspace_root") or (
        ws[0] if ws else os.getcwd()
    )
    if not os.path.exists(os.path.join(workspace_root, ".git")):
        workspace_root = os.path.expanduser("~/fuchsia")

    repo_arg = (workflow_args.get("repo") or "all").strip("/")
    path_prefix = (workflow_args.get("path_prefix") or "").strip("/")
    only_unregistered = bool(workflow_args.get("only_unregistered", True))
    since_date = workflow_args.get("since_date") or _default_since_date()
    min_commits = int(workflow_args.get("min_commits", 20))
    max_concurrency = int(workflow_args.get("max_concurrency", 6))
    report_path = workflow_args.get("report_path")

    config_state = load_agent_configs(workspace_root)
    has_vendor_google = config_state["has_vendor_google"]
    vendor_root = config_state["vendor_root"]

    candidates, missing_repos = discover_candidates(
        workspace_root=workspace_root,
        repo_arg=repo_arg,
        path_prefix=path_prefix,
        only_unregistered=only_unregistered,
        config_state=config_state,
    )
    for missing in missing_repos:
        log(
            f"Requested repository `{missing}` is not present or unsupported in"
            f" `{workspace_root}` (supported: `fuchsia`, `vendor/google`,"
            " `all`); skipping."
        )

    repo_files = {"fuchsia": _discover_repo_files(workspace_root)}
    if has_vendor_google:
        repo_files[VENDOR_PROJECT] = _discover_repo_files(vendor_root)

    log(
        f"Discovered **{len(candidates)}** candidate skill(s) across"
        f" `{repo_arg}` (path_prefix={path_prefix or 'all'},"
        f" only_unregistered={only_unregistered},"
        f" vendor/google={'present' if has_vendor_google else 'absent'})."
    )

    # -----------------------------------------------------------------------
    # Phase 2: Evaluate Skills in Parallel
    # -----------------------------------------------------------------------
    phase("Evaluate Skills in Parallel")

    existing_ids_list = sorted(config_state["all_entry_ids"].keys())
    vendor_note = (
        f"`vendor/google` is checked out at `{vendor_root}`; its config is"
        f" `{VENDOR_CONFIG_LABEL}`."
        if has_vendor_google
        else "`vendor/google` is NOT present in this checkout; only"
        f" `{FUCHSIA_CONFIG_LABEL}` exists."
    )

    def _evaluate_one(item):
        prompt = f"""Evaluate the Fuchsia skill `{item['skill_rel']}` in repo `{item['repo_label']}` (`{item['repo_root']}`) for inclusion in `agent_configs.textproto` (which drives both local `/review` via `.agents/skills/review/scripts/resolve_skills.py` and the Gerrit CL Review Agent via CRACS).

### Metadata
- Fuchsia checkout root: `{workspace_root}`
- Vendor status: {vendor_note}
- Skill repository (`repo`): `{item['repo_label']}` (`{item['repo_root']}`)
- Skill directory (repo-relative): `{item['skill_dir']}`
- Skill file (absolute): `{item['skill_abs']}`
- Canonical `skills` value in `{item['default_config_file']}`: `"{item['canonical_skill_ref']}"`
- Currently registered `agent_configs.textproto` entries for this skill:
```json
{json.dumps(item['existing_entries'], indent=2)}
```
- Existing entry IDs across all configs (IDs must be unique across both files): `{existing_ids_list}`
- Commit activity cutoff (`since_date`): `{since_date}`
- Minimum commit threshold (`min_commits`): `{min_commits}`

### Full `SKILL.md` Content
```markdown
{item['skill_body']}
```

### Required Evaluation Steps
1. **Inspect Full Content & Referenced Files**:
   - Read the entire `SKILL.md` content above (do NOT rely only on the YAML description). If `SKILL.md` links to companion rubric or reference markdown files inside `{item['skill_dir']}/`, read them with `read_file`.
   - Classify the skill accurately:
     - `review_rubric` or `mixed_authoring_and_review`: Defines explicit code, API, FIDL, build, or documentation invariants, safety rules, versioning requirements, or anti-patterns checkable on a diff.
     - `scaffolding_or_generator`: Generates boilerplate files or READMEs without diff-review criteria -> NOT suitable (`SKIP` or `REMOVE`).
     - `cli_or_test_runner`: Teaches how to run `fx test`, `ffx`, `fx gh`, `issues`, emulators, debuggers, worktrees, or orchestrates `/review` itself (`.agents/skills/review`) or commit message formatting -> NOT suitable (`SKIP` or `REMOVE`).
     - `triage_or_debugging_workflow`: Guides crash, flake, or performance trace triage -> NOT suitable (`SKIP` or `REMOVE`).
     - `merge_or_sync_workflow`: Guides upstream merges or git/jiri sync operations -> NOT suitable (`SKIP` or `REMOVE`).
   - **Important**: Skills under top-level `.agents/skills/` (or `vendor/google/.agents/skills/`) are **NOT** auto-applied to code reviews. Both local `/review` and Gerrit CRACS load ONLY skills listed in `agent_configs.textproto`. Evaluate top-level skills by the same rubric and commit-volume criteria as any other skill.

2. **Apply Exact `resolve_skills.py` & CRACS Mapping Rules**:
   - There are at most TWO `agent_configs.textproto` files:
     1. `{FUCHSIA_CONFIG_LABEL}`: Public `fuchsia.git` skills go here, with `projects` in `include_filters` set to the repos they should apply to (`["fuchsia"]`, or `["fuchsia", "vendor/google"]` when general across both repos; an empty `projects` list applies to all projects). Skills listed here resolve relative to `{workspace_root}` (`"{item['canonical_skill_ref']}"` without `/` resolves to `.agents/skills/<name>/SKILL.md`; paths with `/` resolve to `<fuchsia root>/<skill>/SKILL.md`).
     2. `{VENDOR_CONFIG_LABEL}` (only when `vendor/google` exists): `vendor/google`-only skills (internal boards, chips, code names, or skills located inside `vendor/google/`) MUST go here with `projects: ["vendor/google"]`. Skills listed here resolve relative to `{vendor_root}`.
   - Every skill entry MUST carry `tags: ["code-review-agent"]` and a globally unique kebab-case `id`. Entries with `use_default_subagents: true` carry no skills and must never be modified or removed.
   - Filter semantics: an entry applies to a file when at least one `include_filters` block matches (`project` matches or is empty, and `path_regex` **full-matches** the entire repo-relative path via RE2 / `re.fullmatch`) and no `exclude_filters` block matches.

3. **Validate Every `path_regex` Against `git ls-files` of the Target Repo(s)**:
   - Identify the exact repo-relative paths where diffs governed by this skill occur (`target_modified_paths`). Remember a skill's own directory often differs from the files it reviews (e.g., `wlan-api-review` also governs `sdk/fidl/fuchsia\\.wlan\\..*\\.fidl`; `update-assembly-bundles` governs `bundles/assembly/.*`).
   - For any candidate `ADD`, `KEEP`, or `UPDATE` entry, test every `path_regex` via `run_command` using Python `re.fullmatch` against `git -C <target_repo_root> ls-files` so you confirm:
     1. Every `path_regex` full-matches real tracked files (`count > 0`) in the target repo(s), and
     2. It matches **nothing unintended** outside the skill's domain (tighten `path_regex` or add `exclude_filters` if needed).
   - In JSON output fields (`path_regexes`), write normal regex strings (e.g. `src/foo/.*\\.(rs|cc)` in regex syntax), NOT textproto-double-escaped strings.

4. **Measure Git Commit Volume in Target Paths**:
   - Run `git -C <target_repo_root> log --since="{since_date}" --oneline -- <target_modified_paths> | wc -l` via `run_command` to measure actual commit volume in the governed paths since `{since_date}`.
   - If `commits_since_cutoff < {min_commits}`, the skill is low-ROI for `agent_configs.textproto` even if it contains review rules.

5. **Assign Verdict**:
   - `ADD`: Suitable review rubric, `commits_since_cutoff >= {min_commits}`, validated `path_regex`, and not yet in `agent_configs.textproto`.
   - `KEEP`: Already in the right `agent_configs.textproto` with accurate `projects`, `path_regexes`, `exclude_filters`, and `"code-review-agent"` tag, and meets rubric + commit criteria.
   - `UPDATE`: Already in `agent_configs.textproto` and meets rubric + commit criteria, but its config file, `projects`, `path_regexes`, `exclude_filters`, `tags`, or `skills` need adjustment.
   - `REMOVE`: Currently in `agent_configs.textproto`, but fails the rubric check or falls below `min_commits`.
   - `SKIP`: Not in `agent_configs.textproto` and should not be added (set `recommended_entries` to `[]`).
"""
        return agent(
            prompt,
            schema=EVAL_SCHEMA,
            role=f"Evaluate {item['repo_label']}:{item['skill_name']}",
            model="flash",
        )

    evaluations = (
        await _run_parallel(candidates, _evaluate_one, max_concurrency)
        if candidates
        else []
    )

    enriched_evaluations = []
    for ev in evaluations:
        ev_copy = dict(ev)
        ev_copy["deterministic_validation"] = [
            validate_proposed_entry(
                rec, repo_files, workspace_root, has_vendor_google
            )
            for rec in ev.get("recommended_entries") or []
        ]
        enriched_evaluations.append(ev_copy)

    # -----------------------------------------------------------------------
    # Phase 3: Adversarial Audit & Cross-Check
    # -----------------------------------------------------------------------
    phase("Adversarial Audit & Cross-Check")

    if enriched_evaluations:
        audit_prompt = f"""You are the Adversarial Review Auditor for Fuchsia's `agent_configs.textproto` review skill configurations (used by both local `/review` via `.agents/skills/review/scripts/resolve_skills.py` and Gerrit CRACS).

Review the parallel skill evaluations and their deterministic `git ls-files` validation results below, and challenge every `ADD`, `KEEP`, `UPDATE`, or `REMOVE` verdict against these strict rules:
1. **No Generators, CLI Wrappers, Orchestrators, or Operational Workflows**: Skills that generate boilerplate/READMEs (e.g. `fuchsia-readme-generator`), run tests/emulators/CLI tools (e.g. `fx-test`, `fx-worktree`, `fuchsia-gerrit-cli`, `issues-cli`), orchestrate `/review` itself (`.agents/skills/review`), audit commit messages only, triage crashes/flakes/perf traces, or perform upstream merges (e.g. `chromium-media-merge-crosscheck`) MUST be marked `SKIP` (or `REMOVE` if currently registered in `agent_configs.textproto`).
2. **Commit Volume Cutoff (`>= {min_commits}` commits since `{since_date}`)**: Any target file pattern with fewer than `{min_commits}` commits since `{since_date}` MUST be marked `SKIP` (or `REMOVE` if currently registered).
3. **Top-Level `.agents/skills/` Are NOT Auto-Applied**: Neither local `/review` nor Gerrit CRACS auto-discovers `.agents/skills/` anymore; every review rubric needs an explicit `agent_configs.textproto` entry. Do NOT reject a valid review rubric merely because it lives under `.agents/skills/` (reference it as bare `"<skill_name>"` without `/`).
4. **Config File Placement & Resolver Semantics**:
   - Public `fuchsia.git` skills go in `{FUCHSIA_CONFIG_LABEL}` (skill paths resolved relative to `{workspace_root}`; `projects` set to `["fuchsia"]` and/or `["vendor/google"]`, or `[]` when intentionally matching all projects).
   - `vendor/google`-only skills (internal boards/chips/code names or any skill inside `vendor/google/`) MUST go in `{VENDOR_CONFIG_LABEL}` with `projects: ["vendor/google"]` (skill paths resolved relative to `{vendor_root}`). {vendor_note}
   - Every entry MUST include `tags: ["code-review-agent"]` and have a unique `id` across both config files. Never touch `use_default_subagents: true` entries (`fuchsia-code-review-agent`, `turquoise-code-review-agent`).
5. **Strict `git ls-files` Full-Match `path_regex` Verification**:
   - Inspect `deterministic_validation` (`valid`, `errors`, `warnings`, `matched_counts`, `matched_top_dirs`, `sample_matches`, `per_regex_counts`) on every candidate entry.
   - Every `path_regex` MUST full-match real tracked paths in `git ls-files` (`count > 0`) and match **nothing unintended** outside the skill's domain. If a proposed regex has errors, 0 matches, or overbroad matches in `matched_top_dirs` / `sample_matches`, either refine the `path_regex` / `exclude_filters` and verify via `run_command` against `git ls-files`, or downgrade an unsalvageable entry.

### Existing Parsed `agent_configs.textproto` Entries
```json
{json.dumps(config_state['parsed_by_config'], indent=2)}
```

### Parallel Skill Evaluations (with Deterministic `git ls-files` Validation)
```json
{json.dumps(enriched_evaluations, indent=2)}
```

Produce:
- `audited_verdicts`: Final verdict (`ADD`, `KEEP`, `UPDATE`, `REMOVE`, or `SKIP`) for every evaluated skill, including `target_config_file`, `entry_id` (empty string if `SKIP`), `commits_since_cutoff`, and `override_reason`.
- `proposed_entries`: Audited `agent_configs.textproto` entries for all `ADD`, `KEEP`, `UPDATE`, and `REMOVE` verdicts (closely related skills with identical filters may share a single entry). Write plain regex strings in `path_regexes`; Phase 4 escapes and formats `configs: {{ ... }}` blocks automatically.
- `executive_summary_markdown`: Concise Markdown tables covering (1) Actionable `ADD` / `UPDATE` / `REMOVE` verdicts (with config file, entry ID, `projects`, `path_regex` summary, matched file counts, and commit counts), (2) Verified `KEEP` entries, and (3) Rejected `SKIP` skills with commit counts and reasons. Do NOT embed raw textproto blocks in `executive_summary_markdown` (Phase 4 appends them grouped by target config file).
"""
        audit_result = await agent(
            audit_prompt,
            schema=AUDIT_SCHEMA,
            role="Adversarial Skill Auditor",
        )
    else:
        audit_result = {
            "audited_verdicts": [],
            "proposed_entries": [],
            "executive_summary_markdown": "No matching candidate skills found.",
        }

    # -----------------------------------------------------------------------
    # Phase 4: Summary Report & Ready-to-Paste Config Blocks
    # -----------------------------------------------------------------------
    phase("Summary Report")

    blocks_md = render_config_blocks_markdown(
        audit_result.get("proposed_entries") or [],
        repo_files,
        workspace_root,
        has_vendor_google,
    )
    summary_md = (
        audit_result["executive_summary_markdown"].rstrip() + "\n\n" + blocks_md
    )

    if report_path:
        await call_tool(
            "write_file",
            Path=report_path,
            Content=summary_md + "\n",
            Overwrite=True,
        )
        log(f"Wrote full Markdown evaluation report to `{report_path}`.")

    log(summary_md)


await _run_workflow(globals().get("args") or {})
