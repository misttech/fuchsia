# Bazel Migration and CL Creation

Each **migration group** is migrated by one Planter task and uploaded as one CL.
* If there is one **migration group**, refer to [One Migration Group](#one-migration-group).
* If there are several **migration groups**, refer to [Several Migration Groups](#several-migration-groups) to migrate them in parallel with the Planter fleet.

The `<planter>` below is the `planter` binary resolved with `"$(command -v planter || echo "$HOME/.local/bin/planter")"`, because `~/.local/bin` may not be on `PATH`.

Follow the [Planter Rules](../SKILL.md#planter-rules) in every step below: keep the **Planter tag** verbatim and never invent one, and relay the **Planter warnings** that Planter prints (including the ones `planter fleet` prints while it runs).

## Planter Task Parameters

For a **migration group**, the `<package_names>` are the GN labels of the packages of the **migration group**, separated by `, `. Create the parameters of its Planter task as below:
* `--target-dir`: The directories of the packages of the **migration group**, separated by `,` without spaces (e.g. `src/lib/foo,src/lib/bar`). Order them as below:
  * If there is a **migration directory** and it is one of the packages of the **migration group**, put it first, followed by the other directories in the order they were selected.
  * Otherwise, if one of the packages of the **migration group** is the sole same-area GN consumer of another package of the **migration group** (pattern A) or the root target of other packages of the **migration group** as its private helpers (pattern B) in the [Grouping Rules](migration_package_selection.md#grouping-rules) (Other packages, such as direct test subdirectories (pattern D), do not prevent this), put its directory first, followed by the other directories in the order they were selected.
  * Otherwise, list the directories in the order they were selected.
* `--title`: Refer to **commit message guidelines** to create the title for the migration of the **migration group** and provide the created title here. Planter's commit subject check (`build/beads/planter/machinery/checks/commit_message_format.sh`) only accepts `[bazel_migration][<area>] //<dir>` or `[bazel_migration] //<dir>`, with a single `//` label and no spaces after it. So if there is more than one package in the **migration group**, use the label of the first directory in `--target-dir` (e.g., `[bazel_migration][starnix] //src/starnix/lib/selinux`), instead of the multi-directory forms in the **commit message guidelines** (e.g., `//src/starnix/lib: selinux, ebpf`). List all the migrated directories in `--desc`.
* `--task-id`: The components of the first directory in `--target-dir` joined by `_` (e.g. the task-id of `src/connectivity/bluetooth/lib/bt-obex/objects` is `src_connectivity_bluetooth_lib_bt-obex_objects`, and the task-id of `src/lib/foo,src/lib/bar` is `src_lib_foo`). The **migration groups** do not share packages, so their task IDs are different.
* `--desc`: Refer to **commit message guidelines** to create the commit message body for the migration of the **migration group** and provide the created message body here.
  * Put the `Fixed:`, `Bug:` and `Bazel-Migration-Target:` footers in the last paragraph of `--desc`. Add one footer for the **buganizer ticket number** of each package of the **migration group**. If several packages have the same ticket number, add it only once.
    * If the ticket number is the "Parent ID" defined in the [Buganizer ticket template](../assets/buganizer_ticket_template.md), use a `Bug:` footer, so landing the CL does not close the parent ticket.
    * Otherwise, use a `Fixed:` footer, so landing the CL closes the migration ticket.
  * Leave out the `Test:` footer, because Planter adds its own test footers.
  * Leave out the `TAG:` footer and the `Change-Id:` footer. Planter adds the **Planter tag** and replaces any `TAG:` line in `--desc` with it.

## One Migration Group

1. Record the current commit with `git rev-parse HEAD`.

2. From the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh` with the [Planter task parameters](#planter-task-parameters) of the **migration group** to migrate it.
   * `FUCHSIA_DIR`: The root of the Fuchsia checkout on the **migration branch**. Always set it, because the script otherwise defaults to `$HOME/fuchsia`, which may be a different checkout.
   * If the script returns error, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate `<package_names>`."

3. Run `git log -1 --format=%B` to get the commit message of the commit Planter just made, and then use "fuchsia-gerrit-cli" skill to get the CL number with the `Change-Id` in the retrieved commit message.
   * If it fails to get the CL number, fail the Bazel Migration and CL Creation step with the reason: "Fail to get the generated CL for `<package_names>` migration."

4. Review the commit message retrieved at the previous step based on the **commit message guidelines**. If there are review errors, fix them, amend the commit with updated commit message, and run `git push origin HEAD:refs/for/main` to upload a new patchset.
   * Keep the `Change-Id:` line and the **Planter tag** of the retrieved commit message exactly as they are: copy them verbatim into the updated commit message, with the **Planter tag** right before the `Change-Id:` line. Never write a new `TAG:` line or change the `<freshness>` value.
   * If the retrieved commit message has no **Planter tag**, do not add one. Tell the user that the commit has no **Planter tag**.
   * If it fails to amend the commit or upload a new patchset, ignore the failure.

5. Use "gsheets" skill: Update the "Migration Status" of each **migration package** in the **control table** to "CL_created" and set "Last Status Update" to the **status timestamp**.
   * If it fails to update the **control table**, ignore the failure.

6. If **assign reviewer** is enabled, refer to [Assign a Reviewer](#assign-a-reviewer) to assign a reviewer to the CL.

## Several Migration Groups

The Planter fleet runs the Planter tasks of the **migration groups** in parallel, one task in each registered Fuchsia checkout (slot) at a time, and uploads one CL for each **migration group**. See the Planter fleet guide in the "References" section of the document at go/planter_check_and_update for how the fleet works.

A **migration group** fails when its Planter task cannot be queued, or the task is not uploaded to Gerrit. A failed **migration group** does not stop the other **migration groups**; handle it as described in [Migration Group Failure](error_handling.md#migration-group-failure).

1. Record the current commit with `git rev-parse HEAD`.

2. Run `<planter> slot list` to check the registered slots.
   * If slots are registered, use them, and do not allocate worktree slots.
   * If no slot is registered, refer to [Allocate Worktree Slots](#allocate-worktree-slots) to allocate the slots with the "fx-worktree" skill.

3. For each **migration group**, from the root of the Fuchsia checkout, run `FUCHSIA_DIR="$(git rev-parse --show-toplevel)" build/beads/.agent/skills/e2e_bazel_migration/scripts/planter_task_add_run.sh --add-only` with the [Planter task parameters](#planter-task-parameters) of the **migration group** to queue its Planter task without running it.
   * If the script returns error, the **migration group** fails with the reason: "Planter failed to queue the migration of `<package_names>`."
   * If every **migration group** fails, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to queue the migration groups."

4. From the root of the Fuchsia checkout, run `<planter> fleet --gerrit --until-uploaded --task=<task IDs>` to migrate the queued **migration groups** in parallel and upload their CLs. The `<task IDs>` are the task IDs of the queued **migration groups**, separated by `,`.
   * The fleet exits when every task is uploaded or halted. It can take hours; run it in the background and wait for it to exit. Do not treat a slow run as a failure.
   * If the fleet fails to start, every queued **migration group** fails with the reason: "Planter fleet failed to start."

5. Run `<planter> status` to get the result of each queued **migration group**. The line of each task has the form `- <task ID> target=... status=<status> phase=<phase> cl=<CL number> ...`.
   * If the status is `AWAITING_HUMAN_REVIEW` or `LANDED` and the `cl=` field has the CL number, the CL of the **migration group** is created.
   * Otherwise, the **migration group** fails with the reason: "Planter failed to migrate `<package_names>`." Run `<planter> inspect --task=<task ID>` to find the cause and add it to the reason.

6. For each **migration group** whose CL is created:
   1. Use "fuchsia-gerrit-cli" skill to get the commit message of the CL, and review it based on the **commit message guidelines**. If there are review errors, fetch the CL into the current Fuchsia checkout, fix them, amend the commit with updated commit message, and run `git push origin HEAD:refs/for/main` to upload a new patchset. Then switch back to the **migration branch**.
      * Keep the `Change-Id:` line and the **Planter tag** of the CL's commit message exactly as they are: copy them verbatim into the updated commit message, with the **Planter tag** right before the `Change-Id:` line. Never write a new `TAG:` line or change the `<freshness>` value.
      * If the CL's commit message has no **Planter tag**, do not add one. Tell the user that the CL has no **Planter tag**.
      * If it fails to amend the commit or upload a new patchset, ignore the failure.
   2. Use "gsheets" skill: Update the "Migration Status" of each package of the **migration group** in the **control table** to "CL_created" and set "Last Status Update" to the **status timestamp**.
      * If it fails to update the **control table**, ignore the failure.
   3. If **assign reviewer** is enabled, refer to [Assign a Reviewer](#assign-a-reviewer) to assign a reviewer to the CL. Run it for the CLs in parallel, at most one for each slot, before the slots are released in the next step, because fixing CQ failures needs a slot.

7. If worktree slots were allocated in step 2, refer to [Release Worktree Slots](#release-worktree-slots) to release them.

8. Run `git reset --hard <recorded commit>` and then `git clean -fd -- <migration package dirs>` to remove what the fleet left in the current Fuchsia checkout. The `<recorded commit>` is the commit recorded in step 1, and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces. The created CLs are on Gerrit, so no work is lost.

9. If no CL is created for any **migration group**, fail the Bazel Migration and CL Creation step with the reason: "Planter failed to migrate all the migration groups." The failed **migration groups** have already been handled, so this is a **STOP** condition, not a **PLANTER ERROR** condition.

### Allocate Worktree Slots

Each slot needs its own Fuchsia checkout with a working build directory, because Planter builds in it. Use the "fx-worktree" skill to claim one worktree for each slot from the worktree pool of the current Fuchsia checkout; `fx worktree add` provisions a new worktree when no free one is available.

1. The number of worktree slots is the number of **migration groups**, but at most 4, or at most the number of slots the user specifies in the prompt. Builds dominate the run time, so more slots than this slow each other down.
2. For each worktree slot, choose the name `planter-<n>`, where `<n>` is 1, 2, ... (e.g., `planter-1`), skipping the names already used by `fx worktree list`.
   1. Run `fx worktree add <name>` from the root of the current Fuchsia checkout, and `fx worktree locate <name>` to get its path.
   2. If the worktree has no build directory (`fx get-build-dir` in the worktree fails or prints a directory without `args.gn`), configure it like the current Fuchsia checkout: run `fx status` in the current Fuchsia checkout to get its product and board, and run `fx set <product>.<board>` in the worktree.
   3. Run `<planter> slot add <path> --id=<name>` to register the worktree as a slot.
   * If any of these fails, run `fx worktree remove <name>` and skip this worktree slot. If the worktree failed to be configured in step 2, also report the error to the user.
3. If no worktree slot can be allocated, the fleet uses one slot in the current Fuchsia checkout and migrates the **migration groups** one after another. Tell the user, and continue.

### Release Worktree Slots

Release the worktree slots allocated in [Allocate Worktree Slots](#allocate-worktree-slots), after the failed **migration groups** are handled as described in [Migration Group Failure](error_handling.md#migration-group-failure), so that no Planter task holds a slot. For each allocated worktree slot `<name>`:

1. Run `<planter> slot remove <name>` to unregister the slot.
2. Run `fx worktree remove <name>` to return the worktree to the pool.
* If either command fails, ignore the failure, and report the slots that could not be released to the user.

## Assign a Reviewer

Assign a human reviewer to a created CL with Planter's `--assign-reviewer`. Planter assigns the reviewer only after a CQ dry run of the CL passes, so this also runs CQ on the CL and fixes CQ failures.

1. From the root of the Fuchsia checkout, run `<planter> run --gerrit --cq --assign-reviewer --task=<task ID>`, where `<task ID>` is the task ID of the **migration group** of the CL. Planter votes Commit-Queue+1, fixes CQ failures and uploads new patchsets (at most 3 rounds), and assigns a reviewer once a CQ dry run passes.
   * A CQ dry run can take hours; run the command in the background and wait for it to exit. Do not treat a slow run as a failure.
2. Use "fuchsia-gerrit-cli" skill to get the reviewers of the CL, and record the assigned reviewer for the "Completed" notification.
   * If the command fails or no reviewer is assigned (e.g., CQ still fails after the fix rounds), do not fail the Bazel Migration and CL Creation step: the CL is created. Record "not assigned" with the reason for the "Completed" notification, and continue. The user can assign one later with `<planter> assign-reviewer <CL number>`.

