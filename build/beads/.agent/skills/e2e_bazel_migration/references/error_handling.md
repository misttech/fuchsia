# Error Handling

This document handles error conditions (e.g., STOP). For non-error conditions, refer to the notifications.md file.

## Common Error Handling Steps For All Error Conditions
* Use "gmail" skill: refer to the "Interrupted Notification Template" in the [Notification Templates](../assets/notification_templates.md) to draft the "Interrupted" notification and send it to the user.
* Use the "gsheets" skill: in the **control table**, for each **migration package**, change "Migration Status" to "blocked", write the reason of the failed step to "Failure Reason", and set "Last Status Update" to the **status timestamp**.
  * If no **migration package** has been selected yet, skip this step.
  * If the failed step stopped because Planter is out of date before any migration task ran, revert each claimed **migration package** instead of marking it "blocked": change "Migration Status" back to "ready_for_migration", clear "Owner", and set "Last Status Update" to the **status timestamp**.
  * A package dropped or blocked during [Migration Package Selection](migration_package_selection.md) is not a **migration package**; do not update its row.
  * Skip the packages already set to "blocked" by [Migration Group Failure](#migration-group-failure), so their "Failure Reason" keeps the reason of their **migration group**.


## Error Handling Steps specific to the "STOP and CLEANUP" Condition
* Common error handling steps.
* Use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dirs>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_planter.md](bazel_migration_planter.md), and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces.


## Error Handling Steps specific to the "PLANTER ERROR" condition
* Common error handling steps.
* Use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dirs>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_planter.md](bazel_migration_planter.md), and `<migration package dirs>` is the directories of all **migration packages**, separated by spaces.
* If CL was not created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task <task-id>` to delete the generated Planter task. If Planter returns an error, ignore it.
* If CL has been created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task --abandon <task-id>` to delete the generated Planter task and the CL.


## Migration Group Failure

When there are several **migration groups**, a failed **migration group** is handled by itself, and the other **migration groups** continue (see [Several Migration Groups](bazel_migration_planter.md#several-migration-groups)). For each failed **migration group**:
* Use the "gsheets" skill: in the **control table**, for each package of the failed **migration group**, change "Migration Status" to "blocked", write the reason of the failure to "Failure Reason", and set "Last Status Update" to the **status timestamp**. If it fails to update the **control table**, ignore the failure.
* If its Planter task was queued and has no CL, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task <task-id>` to delete the task. If it has a CL, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task --abandon <task-id>` to delete the task and abandon the CL. If Planter returns an error, ignore it.
* Do not send the "Interrupted" notification for it; the failed **migration groups** are listed in the "Completed" notification instead.

If the Bazel Migration and CL Creation step fails or stops after worktree slots were allocated, refer to [Release Worktree Slots](bazel_migration_planter.md#release-worktree-slots) to release them before handling the error condition.
