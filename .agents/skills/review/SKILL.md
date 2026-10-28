---
name: review
description: >
  Performs an AI code review of Fuchsia changes (fuchsia.git and
  vendor/google) using the same Fuchsia review skills as the Gerrit CL Review
  Agent. Use when asked to review, self-review, or `/review` changes in a
  Fuchsia checkout. Self-contained: picks skills from the Gerrit agent config
  and runs one read-only reviewer per skill.
---

# Fuchsia code review

This skill reviews a Fuchsia change with the review skills listed in
`.agents/code-review/agent_configs.textproto` (fuchsia.git, plus the
vendor/google copy for vendor/google changes), the same files the Gerrit CL
Review Agent (`groupagent-fuchsia-review` / `groupagent-turquoise-review`)
reads. Local and hosted reviews therefore load the same skills.

It is self-contained: do not load or follow any other `review` skill. It is
named `review` so that `/review` in a Fuchsia checkout runs it.

## Step 1: Determine the scope

1.  Find the repository that owns the change: the fuchsia.git root, or
    `vendor/google` inside it for vendor/google changes. Detect the VCS there
    (`jj` if a `.jj` directory exists, otherwise `git`).
1.  Pick the target:
    - If the user named a revision, commit, branch, or Gerrit change, review
      that.
    - Otherwise, if the working copy has changes, review them (`jj diff -r @` or
      `git diff HEAD`).
    - Otherwise, review the most recent commit (`@-` in `jj`, `HEAD` in `git`).
1.  Record the immutable commit hash(es) and the list of changed files as paths
    relative to the owning repository, exactly as Gerrit shows them.

## Step 2: Summarize the intent

Read the commit description(s) and any task context from the conversation, and
write a short, factual intent summary: the objective and the key logic changes.
When reviewing code written earlier in the same session, do not include claims
about correctness or justifications for design choices; they bias the reviewers.

## Step 3: Resolve review tasks

Run from the fuchsia.git root:

```bash
fuchsia-vendored-python .agents/skills/review/scripts/resolve_skills.py \
  --files="<path 1>,<path 2>" \
  --project="<fuchsia|vendor/google>"
```

If `fuchsia-vendored-python` is not on `PATH`, use
`prebuilt/third_party/python3/<host>/bin/python3` or any `python3` 3.9 or later;
both scripts use only the standard library.

- The output is a JSON array of `{"skill_path": ..., "files": [...]}`. Each
  entry is one review task.
- For `vendor/google` changes, pass `--project=vendor/google` and paths relative
  to `vendor/google`. The script then also reads
  `vendor/google/.agents/code-review/agent_configs.textproto` and resolves the
  skills listed there relative to `vendor/google`. Pass
  `--repo_root=<fuchsia.git root>` if you run the script outside the checkout.
- Always add one more task, `general`, with no skill and all changed files. It
  stands in for the hosted agent's default reviewer and covers correctness,
  error handling, edge cases, tests, and the Fuchsia style of the surrounding
  code.

## Step 4: Run the reviewers

Use `${TMPDIR:-/tmp}/fuchsia_review_<run_id>` as `<out_dir>`, where `<run_id>`
comes from `uuidgen`. Never write review files inside the checkout.

If `define_subagent` and `invoke_subagent` are available:

1.  Define a subagent named `fuchsia_code_reviewer` with write tools enabled (so
    it can run VCS diff commands and write its result file). Its system prompt
    must say that it is a strictly read-only reviewer: it never edits files in
    the checkout and never runs builds or tests. It must read and follow
    `.agents/skills/review/references/reviewer.md`.
1.  In a single turn, invoke one `fuchsia_code_reviewer` per task so they run in
    parallel. Give each one: the repository root and VCS, the target scope and
    commit hash(es), the intent summary, its task (`skill_path` and `files`, or
    `general`), and its output file `<out_dir>/task_<n>.json`.
1.  Wait for all of them. If a reviewer fails or its output file is missing or
    invalid, invoke it once more; if it fails again, leave it out and say so in
    the report summary.
1.  Invoke one more, **new** `fuchsia_code_reviewer` (not a reused first-round
    reviewer) for the `general` task with the output file
    `<out_dir>/round2.json` and the paths of all first-round files. Ask it to
    report only defects that are not already in those files.

Otherwise, follow `references/reviewer.md` yourself once per task, in sequence,
then do the round-2 `general` pass yourself (output `<out_dir>/round2.json`),
reporting only defects not already in the first-round files. Reviewing your own
change in the same session is not independent; say so in the report summary.

## Step 5: Merge, render, and present

1.  Read all result files and write one merged JSON object to
    `<out_dir>/review.json` with:
    - `decision`: `changes_requested` if any finding is `blocking`,
      `approved_with_suggestions` if there are only suggestions, `approved` if
      there are none, or `not_applicable` if there were no tasks.
    - `summary`: one paragraph summarizing the findings. Name any reviewer that
      failed twice.
    - `scope`: the reviewed target, for example ``Commit `1a2b3c4d` `` or
      ``Working copy (`@` in jj, `git diff HEAD` in git)``.
    - `agent_skills`: the union of all reviewers' `agent_skills`.
    - `reviewed_files`: the union of all reviewers' `reviewed_files`.
    - `findings`: all findings concatenated, without duplicates. Treat findings
      as duplicates when they have the same `file_path`, overlapping lines, and
      the same defect (file-level findings with no location: same `file_path`
      and defect). Keep the one from a skill task over `general`; between
      skills, keep the one whose `rule_reference` cites the rule that is
      violated; otherwise keep the more detailed comment. Never merge different
      defects at the same line.
1.  Render the report:

    ```bash
    fuchsia-vendored-python .agents/skills/review/scripts/render_report.py \
      --review_result_file=<out_dir>/review.json \
      --workspace=<owning repository root>
    ```

    If it reports a validation error, fix `review.json` and run it again.

1.  Present the rendered Markdown to the user unchanged. Findings are numbered
    in the report (`#1`, `#2`, ...); use those numbers when discussing fixes.
1.  If the user asked to fix the findings, apply the fixes with the active VCS
    after presenting the report.

## Notes

- `agent_configs.textproto` matches like the hosted agent: `path_regex` must
  match the whole repo-relative path, a file matching any `exclude_filters`
  block is skipped, and an entry with no `project` applies to every project.
  Entries without `skills` add nothing here; the `general` task covers them.
- A skill without `/` resolves to `.agents/skills/<name>/SKILL.md`; a skill with
  `/` resolves to `<fuchsia.git root>/<skill>/SKILL.md`. A skill that already
  ends in `/SKILL.md` names the file itself. Skills from the vendor/google
  config resolve the same way relative to `vendor/google`. The script warns on
  stderr if a resolved `SKILL.md` is missing.
- Per-directory `AGENTIC_REVIEW` files are not read by this skill.
