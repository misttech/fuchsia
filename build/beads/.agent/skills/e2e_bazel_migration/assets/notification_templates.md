# Notification Templates

In the templates below:

* `<migrating_target_name>`: The names of the **migration packages**,
  separated by `, `.
* `<paths_of_the_migrating_targets>`: The paths of the **migration packages**,
  separated by `, `.
* `<Buganizer_ticket_numbers>`: The **buganizer ticket numbers** of the
  **migration packages**, separated by `, `, with duplicates removed.
* `<Fuchsia_Gerrit_CLs>`: One line for each **migration group** whose CL is
  created, in the form `<CL number>: <paths of its packages>`. If there is one
  **migration group**, only its CL number. If **assign reviewer** is enabled,
  append ` (reviewer: <assigned reviewer>)`, or ` (reviewer: not assigned,
  <reason>)`.
* `<failed_migration_groups>`: One line for each failed **migration group**, in
  the form `<paths of its packages>: <reason>`. If no **migration group**
  failed, `None`.

## Start Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> started`

**Notification Body:**
```
The migration for <migrating_target_name> started at <current_time>.
Migrated Targets: <paths_of_the_migrating_targets>
Buganizer Tickets: <Buganizer_ticket_numbers>
```

## Completed Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> has been completed`

**Notification Body:**
```
The migration for <migrating_target_name> was completed at <current_time>.
Migrated Targets: <paths_of_the_migrating_targets>
Buganizer Tickets: <Buganizer_ticket_numbers>
Fuchsia Gerrit CLs:
<Fuchsia_Gerrit_CLs>
Failed Migration Groups:
<failed_migration_groups>
```

## Interrupted Notification Template

**Notification Subject:**
`Bazel Migration for <migrating_target_name> was interrupted`

**Notification Body:**
```
The migration for <migrating_target_name> was interrupted at <current_time>
because of <reason>.
Migrated Targets: <paths_of_the_migrating_targets>
Buganizer Tickets: <Buganizer_ticket_numbers>
Control Table: <the_go_link_of_fuchsia-bazel-migration-control-table>
```
