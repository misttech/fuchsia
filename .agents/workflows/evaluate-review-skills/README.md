# `evaluate-review-skills` Workflow

A Jetski Dynamic Workflow (`go/jetski-workflows`) that discovers in-tree
`SKILL.md` files across `fuchsia.git` and/or `vendor/google`, evaluates each
skill in parallel for suitability in local `/review` (`AGENTIC_REVIEW`) and
Gerrit AI code review (`.agents/code-review/agent_configs.textproto`), and runs
an adversarial cross-check pass.

## Files

- [`workflow.json`](workflow.json): Workflow manifest and JSON Schema for
  `Args` validation. Exposes `/evaluate-review-skills` in Jetski Hub and the
  Jetski CLI.
- [`workflow.py`](workflow.py): 4-phase Python orchestration script executed by
  `run_workflow`.

## Phases

1. **Discover Skills & Existing Configs**:
   Uses fast Git index queries (`git ls-files '*SKILL.md'` and
   `git ls-files '*AGENTIC_REVIEW'`) to locate all tracked and untracked skills
   plus existing `AGENTIC_REVIEW` and CRACS configurations, and pre-reads each
   `SKILL.md` body.
2. **Evaluate Skills in Parallel**:
   Fans out one `flash` agent per skill (`parallel(..., max_concurrency=6)`) to:
   - Inspect the **entire `SKILL.md` body** and any companion reference files
     (never relying solely on the YAML frontmatter description).
   - Classify whether the skill defines diff-checkable review invariants
     (`review_rubric` / `mixed_authoring_and_review`) vs. scaffolding/README
     generators, CLI/test runners, crash/perf triage workflows, or merge
     runbooks.
   - Identify the actual target directories/globs where reviewed changes occur
     and align `AGENTIC_REVIEW` placement with `/review`'s upward ancestor
     directory traversal.
   - Measure actual commit volume (`git log --since=<since_date> --oneline`) on
     the target paths against `min_commits` (defaulting `since_date` to the past
     2 months, i.e., `2026-08-26`, computed dynamically as 2 months prior to the
     run date).
3. **Adversarial Audit & Cross-Check**:
   Runs a critic/auditor agent over all parallel evaluations to reject false
   positives (such as generator skills or low-churn directories) and synthesize
   ready-to-apply `AGENTIC_REVIEW` files grouped by ancestor directory.
4. **Summary Report**:
   Logs a structured Markdown report of `ADD` / `UPDATE` / `REMOVE`, `KEEP`, and
   `SKIP` verdicts (and optionally writes it to `report_path`).

## Usage

By default (`repo="all"`, `only_unregistered=true`), running the workflow with
no arguments scans `fuchsia.git` plus any `vendor/*` Git sub-repositories
present in the checkout (such as `vendor/google` in an internal checkout, while
gracefully scanning only `fuchsia.git` in an open-source checkout) and evaluates
only skills not yet registered in `AGENTIC_REVIEW` or
`.agents/code-review/agent_configs.textproto`:

```text
/evaluate-review-skills
```

To re-audit all skills (including already-registered ones) or scope to a
specific repository/directory:

```text
/evaluate-review-skills only_unregistered=false
/evaluate-review-skills repo=fuchsia path_prefix=src/storage only_unregistered=false
/evaluate-review-skills repo=vendor/google only_unregistered=false
/evaluate-review-skills {"repo": "all", "only_unregistered": true, "report_path": "/tmp/skill_eval_report.md"}
```
