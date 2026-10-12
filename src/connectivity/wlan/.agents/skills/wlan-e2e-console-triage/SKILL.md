---
name: wlan-e2e-console-triage
description: >
  Workflow to sweep and triage all WLAN E2E builders in the turquoise wlan_e2e
  LUCI console (https://ci.chromium.org/ui/p/turquoise/g/wlan_e2e/builders),
  list recent WLAN test failures and their associated bugs, search Buganizer
  for candidate bugs for unassociated failures, and propose creating or
  associating a bug for human approval. Don't use for deep-dive log/snapshot
  root-causing or bot failure-rate analysis of a specific test (use
  wlan-e2e-test-luci-triage).
---

# Fuchsia WLAN E2E Console Triage (`wlan_e2e` Builders Sweep)

> [!IMPORTANT]
> **Human-in-the-Loop Required**: Never autonomously create a bug or modify a
> LUCI Analysis rule. Present your proposals in Step 2 and wait for explicit
> user approval before running `create-bug` or `associate-bug`.

## Workflow

### Step 1: Scan `wlan_e2e` Builders & Search Candidate Bugs

1. Run `scan` to fetch recent failures across all `wlan_e2e` builders and cache
   unassociated failure groups `(1)..(N)`:
   ```bash
   python3 src/connectivity/wlan/.agents/skills/wlan-e2e-console-triage/scripts/console_triage.py scan
   ```
2. For each distinct unassociated failure cause, use the `issues-cli` skill to
   search Buganizer for open candidate bugs (by key error phrase or suite name)
   and inspect promising candidates.

### Step 2: Propose Triage Actions

Present a concise report and ask the user which actions to execute:

1. **Builder Coverage & Associated Failures**:
   * Summarize active builders, `WARNING` lines, and associated bug groups.
   * Flag (for human awareness) any associated bug whose `Rule` only matches
     `test` without constraining `reason`, or where multiple distinct `Reason:`
     lines are masked under one bug.
2. **Proposals for Unassociated Failures `(N)`**:
   * **Missing or bare error tag** (e.g., `[AssertionError]`,
     `[FuchsiaDeviceError]`, `No primary error message found`): Do not propose a
     rule or download logs during the sweep; recommend follow-up via
     `wlan-e2e-test-luci-triage`.
   * **Merge related groups**: If multiple `(N)` entries share the same root
     cause (e.g., identical error across multiple suites or differing only in
     dynamic SSIDs/timestamps/KOIDs), combine them into a single proposal
     (`--group 1,2`).
   * **Draft the `--rule` SQL expression** (validated against `--group` before
     mutation):
     * Must constrain `reason` using **double quotes** (`"..."`, never single
       quotes) and full-string `LIKE` matching (include leading/trailing `%` for
       substrings; replace dynamic SSIDs, timestamps, PIDs/TIDs, KOIDs, ADB
       `-P`/`-s` ports/serials, fault addresses, durations, and newlines/quotes
       with `%`).
     * **Default (`reason`-only)**: `reason LIKE "%<stable error phrase>%"`.
     * **Suite-scoped (`test+reason`)**:
       `test LIKE "<suite_prefix>%" AND reason LIKE "%<error phrase>%"` (always
       parenthesize `OR` clauses: `test LIKE "..." AND (reason LIKE "..." OR ...)`
       ) — use only when the failure is specific to a single suite/family and
       the error phrase is too generic to match globally (e.g.,
       `network is not in connected state`, `DUT failed to get an ipv4 address`).
   * For each proposed action, show the group index(es), suite(s), board(s),
     builder(s), failure reason, sample build link, candidate bugs, proposed
     `--rule`, and recommendation (**Associate** with `b/<id>` or **Create New
     Bug** titled `[<board>] <suite_or_scope> failing: <reason>`, omitting
     `[<board>]` if board-agnostic.

### Step 3: Execute Approved Actions

Run **one command per bug** after user approval:

```bash
# Associate with an existing bug:
python3 src/connectivity/wlan/.agents/skills/wlan-e2e-console-triage/scripts/console_triage.py associate-bug --group <GROUP_INDICES> --bug-id <BUG_ID> --rule '<RULE_SQL>'

# Create a new bug and LUCI Analysis rule:
python3 src/connectivity/wlan/.agents/skills/wlan-e2e-console-triage/scripts/console_triage.py create-bug --group <GROUP_INDICES> --title "<BUG_TITLE>" --rule '<RULE_SQL>'
```

Report the resulting `bug_url` and `rule_url` to the user.
