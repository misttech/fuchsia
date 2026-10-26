# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Jetski Dynamic Workflow: Evaluate Fuchsia in-tree skills for AI code review.

Discovers SKILL.md files across fuchsia.git and/or vendor/google, evaluates each
skill's full content and target directory commit activity in parallel against
three strict criteria:
  1. Rubric vs. Generator/Workflow (must define diff-checkable review rules, not
     scaffolding generators, CLI wrappers, or operational triage runbooks).
  2. Ancestor Traversal Alignment (AGENTIC_REVIEW must reside in an ancestor
     directory of the actual modified files so /review's upward walk finds it).
  3. Commit Volume Threshold (target files/directories must meet min_commits
     since since_date to justify review overhead).

Then runs an adversarial cross-check pass and outputs ready-to-apply
AGENTIC_REVIEW stanzas and .agents/code-review/agent_configs.textproto entries.
"""

import calendar
import datetime
import json
import os
import subprocess

try:
    from advanced import parallel as _adv_parallel
except ImportError:
    _adv_parallel = None


def _run_parallel(items, fn, max_concurrency):
    if _adv_parallel is not None:
        return _adv_parallel(items, fn, max_concurrency=max_concurrency)
    return parallel(items, fn)


def _git_lines(repo_root, *git_args):
    cmd = ["git", "-C", repo_root] + list(git_args)
    out = subprocess.check_output(cmd, text=True, stderr=subprocess.DEVNULL)
    return [line.strip() for line in out.splitlines() if line.strip()]


def _discover_repo_files(repo_root, pattern):
    tracked = _git_lines(repo_root, "ls-files", pattern)
    untracked = _git_lines(
        repo_root, "ls-files", "--others", "--exclude-standard", pattern
    )
    return sorted(set(tracked + untracked))


def _default_since_date(today=None):
    today = today or datetime.date.today()
    year = today.year
    month = today.month - 2
    if month <= 0:
        month += 12
        year -= 1
    max_day = calendar.monthrange(year, month)[1]
    return datetime.date(year, month, min(today.day, max_day)).isoformat()


# ---------------------------------------------------------------------------
# Phase 1: Discover Skills & Existing Review Configurations
# ---------------------------------------------------------------------------
phase("Discover Skills & Existing Configs")

_ws = globals().get("WORKSPACES")
workspace_root = args.get("workspace_root") or (_ws[0] if _ws else os.getcwd())
if not os.path.exists(os.path.join(workspace_root, ".git")):
    workspace_root = os.path.expanduser("~/fuchsia")

repo_arg = (args.get("repo") or "all").strip("/")
path_prefix = (args.get("path_prefix") or "").strip("/")
only_unregistered = bool(args.get("only_unregistered", True))
since_date = args.get("since_date") or _default_since_date()
min_commits = int(args.get("min_commits", 20))
max_concurrency = int(args.get("max_concurrency", 6))
report_path = args.get("report_path")

repos = []
if repo_arg in ("fuchsia", "all"):
    repos.append(("fuchsia", workspace_root))
if repo_arg == "all":
    vendor_parent = os.path.join(workspace_root, "vendor")
    if os.path.isdir(vendor_parent):
        for entry in sorted(os.listdir(vendor_parent)):
            sub_root = os.path.join(vendor_parent, entry)
            if os.path.isdir(sub_root) and os.path.exists(
                os.path.join(sub_root, ".git")
            ):
                repos.append((f"vendor/{entry}", sub_root))
elif repo_arg != "fuchsia":
    sub_root = os.path.join(workspace_root, repo_arg)
    if os.path.isdir(sub_root) and os.path.exists(
        os.path.join(sub_root, ".git")
    ):
        repos.append((repo_arg, sub_root))
    else:
        log(
            f"Requested sub-repository `{repo_arg}` is not present in"
            f" `{workspace_root}`; skipping."
        )


async def _read_optional(abs_path):
    try:
        return await call_tool("read_file", Path=abs_path)
    except Exception:
        return ""


candidates = []
existing_agentic_reviews = {}

for repo_label, repo_root in repos:
    skill_rels = _discover_repo_files(repo_root, "*SKILL.md")
    if path_prefix:
        skill_rels = [
            p
            for p in skill_rels
            if p.startswith(path_prefix + "/") or p == path_prefix
        ]

    ar_rels = _discover_repo_files(repo_root, "*AGENTIC_REVIEW")
    ar_contents = {}
    for ar_rel in ar_rels:
        ar_abs = os.path.join(repo_root, ar_rel)
        ar_contents[ar_rel] = await _read_optional(ar_abs)
    existing_agentic_reviews[repo_label] = ar_contents

    cracs_rel = ".agents/code-review/agent_configs.textproto"
    cracs_content = await _read_optional(os.path.join(repo_root, cracs_rel))

    for skill_rel in skill_rels:
        skill_dir = os.path.dirname(skill_rel)
        skill_name = os.path.basename(skill_dir)
        in_ar = [
            ar_rel
            for ar_rel, text in ar_contents.items()
            if skill_dir in text or skill_name in text
        ]
        in_cracs = (skill_dir in cracs_content) or (skill_name in cracs_content)
        if only_unregistered and (in_ar or in_cracs):
            continue
        skill_abs = os.path.join(repo_root, skill_rel)
        skill_body = await _read_optional(skill_abs)
        candidates.append(
            {
                "repo_label": repo_label,
                "repo_root": repo_root,
                "skill_rel": skill_rel,
                "skill_dir": skill_dir,
                "skill_name": skill_name,
                "skill_abs": skill_abs,
                "skill_body": skill_body,
                "existing_agentic_review_files": in_ar,
                "already_in_cracs": in_cracs,
            }
        )

log(
    f"Discovered **{len(candidates)}** candidate skill(s) across"
    f" `{repo_arg}` (path_prefix={path_prefix or 'all'},"
    f" only_unregistered={only_unregistered})."
)

# ---------------------------------------------------------------------------
# Phase 2: Evaluate Skills in Parallel
# ---------------------------------------------------------------------------
phase("Evaluate Skills in Parallel")

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
                "Repo-root-relative directories or file globs where the code,"
                " FIDL, build, or doc diffs governed by this skill actually"
                " occur."
            ),
        },
        "commits_since_cutoff": {
            "type": "integer",
            "description": (
                "Number of git commits touching target_modified_paths since"
                " since_date."
            ),
        },
        "recommended_agentic_review_files": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "agentic_review_path": {
                        "type": "string",
                        "description": (
                            "Repo-root-relative path to the AGENTIC_REVIEW file"
                            " (must be an ancestor directory of the modified"
                            " files)."
                        ),
                    },
                    "include_globs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": (
                            "Globs relative to the directory containing"
                            " AGENTIC_REVIEW."
                        ),
                    },
                },
                "required": ["agentic_review_path", "include_globs"],
            },
        },
        "verdict": {
            "type": "string",
            "enum": ["ADD", "KEEP", "UPDATE", "REMOVE", "SKIP"],
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
        "recommended_agentic_review_files",
        "verdict",
        "rationale",
    ],
}


def _evaluate_one(item):
    prompt = f"""Evaluate the Fuchsia skill `{item['skill_rel']}` in repo `{item['repo_label']}` (`{item['repo_root']}`) for inclusion in local `/review` (`AGENTIC_REVIEW`) and Gerrit AI code review (`CRACS`).

### Metadata
- Repository root: `{item['repo_root']}`
- Skill directory (repo-relative): `{item['skill_dir']}`
- Skill file (absolute): `{item['skill_abs']}`
- Currently referenced in `AGENTIC_REVIEW` files: `{item['existing_agentic_review_files']}`
- Currently referenced in `.agents/code-review/agent_configs.textproto`: `{item['already_in_cracs']}`
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
     - `review_rubric` or `mixed_authoring_and_review`: Contains explicit code, API, FIDL, build, or documentation invariants, safety rules, versioning requirements, or anti-patterns that a code reviewer checks on a diff.
     - `scaffolding_or_generator`: Generates boilerplate files or READMEs (e.g. `fuchsia-readme-generator`) without diff-review criteria -> NOT suitable for code review.
     - `cli_or_test_runner`: Teaches how to run `fx test`, `ffx`, emulators, debuggers, or worktrees -> NOT suitable for code review.
     - `triage_or_debugging_workflow`: Guides crash, flake, or performance trace triage -> NOT suitable for code review.
     - `merge_or_sync_workflow`: Guides upstream merges or git/jiri operations -> NOT suitable for code review.

2. **Determine True Target Scope & Ancestor Alignment**:
   - Identify the exact repo-root-relative directories/files where diffs governed by this skill happen (`target_modified_paths`).
   - Remember: a skill's `SKILL.md` location is often NOT where all target files live (e.g., `wlan-api-review` in `src/connectivity/wlan/` also reviews `sdk/fidl/fuchsia.wlan.*/`; `create-non-sdk-rule` in `build/bazel/rules/` reviews `build/**/*.bzl`; `update-assembly-bundles` in `build/assembly/` reviews `bundles/assembly/`).
   - Because local `/review` (`review-customization-discovery`) walks upward through **ancestor directories** from each modified file to `{item['repo_root']}`, each `AGENTIC_REVIEW` file MUST be placed in an ancestor directory of the modified files.
   - Avoid root-level `AGENTIC_REVIEW` unless the skill is truly a universal language rubric (like `rust_best_practices` or `fuchsia-cpp-guidelines` in `.agents/skills/` which `/review` already discovers globally).

3. **Measure Git Commit Volume in Target Paths**:
   - Run `git -C {item['repo_root']} log --since="{since_date}" --oneline -- <target_modified_paths> | wc -l` via `run_command` to measure actual commit volume in the target paths since `{since_date}`.
   - If `commits_since_cutoff < {min_commits}`, the skill is low-ROI for `AGENTIC_REVIEW` even if it has review rules.

4. **Assign Verdict**:
   - `ADD`: Suitable review rubric, meets commit threshold (`>= {min_commits}`), and not yet in all needed `AGENTIC_REVIEW` ancestor directories.
   - `KEEP`: Already configured in the right `AGENTIC_REVIEW` file(s) and meets all criteria.
   - `UPDATE`: Currently in `AGENTIC_REVIEW`, but its ancestor directory placement or `include:` globs need adjustment.
   - `REMOVE`: Currently in `AGENTIC_REVIEW`, but fails the rubric check (it's a generator/workflow/runner) or falls below the commit threshold.
   - `SKIP`: Not in `AGENTIC_REVIEW` and should not be added (not a review rubric, already global in `.agents/skills/`, or below commit threshold).
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

# ---------------------------------------------------------------------------
# Phase 3: Adversarial Audit & Cross-Check
# ---------------------------------------------------------------------------
phase("Adversarial Audit & Cross-Check")

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
                    "commits_since_cutoff": {"type": "integer"},
                    "override_reason": {"type": "string"},
                },
                "required": [
                    "repo",
                    "skill_dir",
                    "skill_name",
                    "initial_verdict",
                    "final_verdict",
                    "commits_since_cutoff",
                    "override_reason",
                ],
            },
        },
        "proposed_agentic_review_files": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string"},
                    "agentic_review_path": {"type": "string"},
                    "action": {
                        "type": "string",
                        "enum": ["CREATE", "UPDATE", "KEEP", "DELETE"],
                    },
                    "content": {"type": "string"},
                    "rationale": {"type": "string"},
                },
                "required": [
                    "repo",
                    "agentic_review_path",
                    "action",
                    "content",
                    "rationale",
                ],
            },
        },
        "executive_summary_markdown": {"type": "string"},
    },
    "required": [
        "audited_verdicts",
        "proposed_agentic_review_files",
        "executive_summary_markdown",
    ],
}

if evaluations:
    audit_prompt = f"""You are the Adversarial Review Auditor for Fuchsia's `AGENTIC_REVIEW` and CRACS skill configurations.

Review the parallel skill evaluations below and challenge every `ADD`, `KEEP`, or `UPDATE` verdict against these strict rules:
1. **No Generators or Operational Workflows**: Skills that generate boilerplate/READMEs (e.g. `fuchsia-readme-generator`), run tests/emulators/CLI tools (e.g. `editing-fuchsia-controller`), triage crashes/flakes/perf traces, or perform upstream merges (e.g. `chromium-media-merge-crosscheck`) MUST be marked `SKIP` (or `REMOVE` if currently in `AGENTIC_REVIEW`).
2. **Commit Volume Cutoff (`>= {min_commits}` commits since `{since_date}`)**: Any target directory or file pattern with fewer than `{min_commits}` commits since `{since_date}` MUST be marked `SKIP` or `REMOVE`.
3. **No Redundant Top-Level Global Skills**: Skills under top-level `.agents/skills/` in `fuchsia.git` are already discovered globally by `/review` and must NOT be duplicated into subsystem `AGENTIC_REVIEW` files unless a skill lives outside `.agents/skills/` or needs scoping.
4. **Ancestor Traversal Accuracy & Canonical YAML Format**: Every proposed `AGENTIC_REVIEW` file must sit in an ancestor directory of the modified files it governs, using the canonical YAML format:
   ```yaml
   # Copyright 2026 The Fuchsia Authors. All rights reserved.
   # Use of this source code is governed by a BSD-style license that can be
   # found in the LICENSE file.

   skills:
     - path: <repo-root-relative path to skill directory>
       include:
         - '<glob relative to the directory containing AGENTIC_REVIEW>'
   ```

### Existing `AGENTIC_REVIEW` Files by Repo
```json
{json.dumps(existing_agentic_reviews, indent=2)}
```

### Parallel Skill Evaluations
```json
{json.dumps(evaluations, indent=2)}
```

Produce:
- `audited_verdicts`: Final verdict for each skill (noting any overrides in `override_reason`).
- `proposed_agentic_review_files`: Complete `AGENTIC_REVIEW` file contents grouped by `agentic_review_path` (including the `# Copyright 2026 The Fuchsia Authors.` header and `skills:` YAML list).
- `executive_summary_markdown`: A concise Markdown report with tables for (1) Actionable `ADD`/`UPDATE`/`REMOVE` changes, (2) Verified `KEEP` skills, and (3) Rejected `SKIP` skills with commit counts and reasons.
"""
    audit_result = await agent(
        audit_prompt,
        schema=AUDIT_SCHEMA,
        role="Adversarial Skill Auditor",
    )
else:
    audit_result = {
        "audited_verdicts": [],
        "proposed_agentic_review_files": [],
        "executive_summary_markdown": "No matching candidate skills found.",
    }

# ---------------------------------------------------------------------------
# Phase 4: Summary Report
# ---------------------------------------------------------------------------
phase("Summary Report")

summary_md = audit_result["executive_summary_markdown"]
if report_path:
    await call_tool(
        "write_file",
        Path=report_path,
        Content=summary_md + "\n",
        Overwrite=True,
    )
    log(f"Wrote full Markdown evaluation report to `{report_path}`.")

log(summary_md)
