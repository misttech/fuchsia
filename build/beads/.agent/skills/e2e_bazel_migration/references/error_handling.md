# Error Handling

This document handles error conditions (e.g., STOP). For non-error conditions, refer to the notifications.md file.

## Common Error Handling Steps For All Error Conditions
* Use "gmail" skill: refer to the "Interrupted Notification Template" in the [Notification Templates](../assets/notification_templates.md) to draft the "Interrupted" notification and send it to the user.
* Use the "gsheets" skill: in the **control table**, for the **migration package**, change "Migration Status" to "blocked", write the reason of the failed step to "Failure Reason", and set "Last Status Update" to the **status timestamp**.
  * If no **migration package** has been selected yet, skip this step.


## Error Handling Steps specific to the "STOP and CLEANUP" Condition
* Common error handling steps.
* Use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dir>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_planter.md](bazel_migration_planter.md).


## Error Handling Steps specific to the "PLANTER ERROR" condition
* Common error handling steps.
* Use `git reset --hard <recorded commit>` command and then `git clean -fd -- <migration package dir>` to revert the changes created by Planter. The `<recorded commit>` is the commit recorded in step 1 of [bazel_migration_planter.md](bazel_migration_planter.md).
* If CL was not created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task <task-id>` to delete the generated Planter task. If Planter returns an error, ignore it.
* If CL has been created, run `"$(command -v planter || echo "$HOME/.local/bin/planter")" delete-task --abandon <task-id>` to delete the generated Planter task and the CL.
