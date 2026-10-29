# Reviewer Availability

Check whether the human reviewers of a migration CL are out of office (OOO),
and swap each OOO reviewer for an available reviewer from the reviewer routing
table at go/fuchsia-bazel-migration-reviewers. Run the steps below for each CL.

In the commands below, `<G>` is `https://fuchsia-review.googlesource.com`, and
`<planter>` is `"$(command -v planter || echo "$HOME/.local/bin/planter")"`.
Strip the `)]}'` prefix from Gerrit JSON responses. The Gerrit read endpoints
below do not need authentication.

This step never fails the migration. If a command fails, record the failure as
the result for the CL and continue with the next CL.

## 1. Find Pending Reviewers

1. Run `curl -s "<G>/changes/fuchsia~<CL>?o=DETAILED_LABELS&o=SUBMITTABLE"`.
   * If the CL is `MERGED` or `ABANDONED`, or `submittable` is true, record
     "no swap needed" and stop. Gerrit copies a Code-Review+2 to new
     patchsets, so a CL approved by a reviewer who is now OOO can still land.
2. The **pending reviewers** are the entries of `reviewers.REVIEWER` that have
   an `email` and:
   * are not the CL owner;
   * are not service accounts (e.g., `gserviceaccount.com` emails, or names
     containing `bot` or `planter`);
   * have not voted Code-Review+1 or +2 (`labels.Code-Review.all`).
   * If there are no **pending reviewers**, record "no swap needed" and stop.

## 2. Check Availability

For each **pending reviewer**, let `<now>` be the current time and `<end>` be
`<now>` + 7 days, both in RFC3339 UTC.

1. Run `curl -s "<G>/accounts/<email>/status"`. If the status contains `OOO`,
   `out of office`, `vacation`, `leave`, or `away` (case-insensitive), the
   reviewer is **OOO**. If the status has a return date, use it.
2. Otherwise, use the "gcalendar" skill to run
   `gcalendar readonly freebusy --email <email> --start <now> --end <end>`.
   The reviewer is **OOO** if a busy block starts before `<now>` + 24h and
   lasts 20 hours or more. Treat blocks of that kind separated only by a
   weekend as one absence. The return date is the first working day after the
   absence ends.
3. If neither check returns data (e.g., the calendar is not shared, or an auth
   error), the reviewer's availability is "unknown". Do not swap them.

A reviewer is **flagged** if they are **OOO** and their return date is more
than the **reviewer OOO threshold** working days after today.

## 3. Swap Flagged Reviewers

For each **flagged** reviewer:

1. **Pick a replacement with Planter.** From the root of the Fuchsia checkout,
   run `<planter> assign-reviewer --force <CL>`. Planter picks a reviewer from
   the "Reviewers" tab of the reviewer routing table and skips reviewers whose
   Gerrit availability is `OUT_OF_OFFICE`. It also checks code-owner
   eligibility, and logs the pick to the "Assignment Log" tab. Then get the
   reviewers of the CL again. The **replacement** is the human reviewer that
   was added.
   * If there is no new reviewer, if the new reviewer is the **flagged**
     reviewer, or if [step 2](#2-check-availability) flags the new reviewer,
     go to the fallback below. If Planter added a reviewer that step 2
     flags, remove them with
     `fx gh pr edit <CL> --remove-reviewer <their email>`.
   * If Planter fails because it cannot read the Trix "Reviewers" tab (e.g.,
     `gcloud ADC credentials missing spreadsheets scope`), record the command
     Planter suggests as a **Planter warning**, and go to the fallback below.
2. **Fallback.** Use the "gsheets" skill to read the "Reviewers" tab of the
   reviewer routing table (columns: Email, Daily Limit, Accept Overflow). The
   candidates are the reviewers whose Daily Limit is above 0, who are not the
   CL owner or a current reviewer of the CL, and who are code owners of every
   changed file of the CL. To find the code owners, list the files with
   `curl -s "<G>/changes/fuchsia~<CL>/revisions/current/files"` (skip
   `/COMMIT_MSG`), then for each file run
   `curl -s "<G>/changes/fuchsia~<CL>/revisions/current/code_owners/<url-encoded path>?limit=50"`.
   Order the candidates by how many CLs the "Assignment Log" tab gave them in
   the last 24 hours (fewest first), then by their order in the
   "Reviewers" tab. The **replacement** is the first candidate that
   [step 2](#2-check-availability) does not flag and does not report as
   "unknown". Add them with
   `fx gh pr edit <CL> --add-reviewer <replacement email>`.
   * If there is no such candidate, keep the **flagged** reviewer, record
     `<reviewer> OOO until <date>, no available replacement in the routing
     table`, and stop.
3. **Remove the OOO reviewer.** Only after the **replacement** is on the CL,
   run `fx gh pr edit <CL> --remove-reviewer <flagged reviewer email>`.
4. **Explain the swap on the CL.** Use the "fuchsia-gerrit-cli" skill to post
   a resolved top-level comment:
   `Reviewer swapped: <flagged reviewer> is out of office until <date>, so
   this CL was reassigned to <replacement> from the reviewer routing table
   (go/fuchsia-bazel-migration-reviewers).`
5. **Record the swap.** Record
   `<flagged reviewer> (OOO until <date>) -> <replacement>` for the
   notifications.

## 4. Notify the User

* During a migration run, the swaps are included in the "Completed"
  notification (see [Notification Templates](../assets/notification_templates.md)).
* When this step runs alone (see the "Reviewer Availability" section of
  SKILL.md), use the "gmail" skill to send the "Reviewer Swapped" notification
  from the [Notification Templates](../assets/notification_templates.md) to
  the user if at least one reviewer was swapped.
* In both cases, tell the user in the chat which reviewers were swapped, the
  date each OOO reviewer returns, and the CLs where a flagged reviewer could
  not be replaced.
