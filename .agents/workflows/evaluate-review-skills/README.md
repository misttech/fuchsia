# `evaluate-review-skills` Workflow

A Jetski Dynamic Workflow (`go/jetski-workflows`) that discovers in-tree
`SKILL.md` files across `fuchsia.git` and (when present) `vendor/google`,
evaluates each skill in parallel for inclusion in
`.agents/code-review/agent_configs.textproto` and
`vendor/google/.agents/code-review/agent_configs.textproto` (which drive both
local `/review` via `.agents/skills/review/scripts/resolve_skills.py` and the
Gerrit CL Review Agent via CRACS), validates every proposed `path_regex` against
`git ls-files`, and runs an adversarial cross-check pass.

## Files

- [`workflow.json`](workflow.json): Workflow manifest and JSON Schema for `Args`
  validation. Exposes `/evaluate-review-skills` in Jetski Hub and the Jetski
  CLI.
- [`workflow.py`](workflow.py): 4-phase Python orchestration script executed by
  `run_workflow`.

## Phases

1. **Discover Skills & Existing Configs**:
   Parses `<fuchsia root>/.agents/code-review/agent_configs.textproto` and (when
   `vendor/google` is present)
   `<fuchsia root>/vendor/google/.agents/code-review/agent_configs.textproto`
   using the exact skill-path and filter semantics of `resolve_skills.py`
   (skills without `/` resolve to `<config repo root>/.agents/skills/<name>`;
   skills with `/` resolve relative to the root of the repository whose config
   lists them; `use_default_subagents` entries carry no skills). Discovers
   tracked and untracked `*SKILL.md` files via `git ls-files` (including skills
   under top-level `.agents/skills/`, which are not auto-applied to reviews
   without a textproto entry) and pre-reads each candidate `SKILL.md` body.
2. **Evaluate Skills in Parallel**:
   Fans out one `flash` agent per skill (`parallel(..., max_concurrency=6)`) to:
   - Inspect the **entire `SKILL.md` body** and any companion reference files
     (never relying solely on the YAML frontmatter description).
   - Classify whether the skill defines diff-checkable review invariants
     (`review_rubric` / `mixed_authoring_and_review`) vs. scaffolding/README
     generators, CLI/test runners, crash/perf triage workflows, or merge
     runbooks.
   - Select the right target config file (public `fuchsia.git` skills in
     `.agents/code-review/agent_configs.textproto` with `project` filters for
     the repos they apply to; `vendor/google`-only skills in
     `vendor/google/.agents/code-review/agent_configs.textproto`) and validate
     full-match `path_regex` filters (plus `exclude_filters` when needed)
     against `git ls-files`.
   - Measure commit volume (`git log --since=<since_date> --oneline`) on the
     governed paths against `min_commits` (`since_date` defaults dynamically to
     2 months prior to the run date).
   Then runs deterministic `git ls-files` regex and skill-path validation across
   every proposed entry.
3. **Adversarial Audit & Cross-Check**:
   Runs an auditor agent over all parallel evaluations and their deterministic
   `git ls-files` match statistics to reject false positives (generators, CLI
   workflows, low-churn scopes, or broken/overbroad regexes) and produce final
   `ADD` / `KEEP` / `UPDATE` / `REMOVE` / `SKIP` verdicts.
4. **Summary Report**:
   Logs a structured Markdown report and ready-to-paste `configs: { ... }`
   textproto blocks grouped by target config file (and optionally writes the
   report to `report_path`).

## Usage

By default (`repo="all"`, `only_unregistered=true`), running the workflow with
no arguments scans `fuchsia.git` plus `vendor/google` when present (gracefully
scanning only `fuchsia.git` in an open-source checkout) and evaluates only
skills not yet registered in `agent_configs.textproto`:

```text
/evaluate-review-skills
```

To re-audit all skills (including already-registered entries) or scope to a
specific repository or directory:

```text
/evaluate-review-skills only_unregistered=false
/evaluate-review-skills repo=fuchsia path_prefix=src/storage
/evaluate-review-skills repo=vendor/google only_unregistered=false
/evaluate-review-skills {"repo": "all", "report_path": "/tmp/skill_eval.md"}
```
