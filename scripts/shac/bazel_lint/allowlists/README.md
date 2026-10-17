# Bazel Lint Allowlists

This directory holds optional per-check allowlist files for the Bazel `shac`
lint checks in [`//scripts/shac/bazel_lint/`](../README.md).

**AI agents MUST NOT add entries to allowlists.** AI agents should attempt to
fix the reported issue instead. If an agent cannot easily fix the issue, it
should report this to the user. Agents _may_ temporarily add entries to
allowlists when prototyping or investigating, but they MUST fix the issues and
revert changes to this directory before uploading a change and ideally before
the end of a turn and/or related code is presented to the user.

## When to Add an Entry

Prefer fixing the underlying Bazel file to follow the Fuchsia Bazel style guide.
If an exception is temporarily required (for example, during an incremental
migration) or a finding is a false positive:

1. Reach out to the Build team.
2. File a tracking bug in the Build > Bazel component:
   <https://issues.fuchsia.dev/issues/new?component=1477645>.
3. Add an entry referencing that bug to `<check_name>.json` in this directory.
4. Verify that the check passes across the entire workspace and include this
   command as a `Test:` footer in the commit message:
   ```sh
   fx host-tool shac check --only <check_name> --all
   ```
5. Send the change to a member of [`/build/OWNERS`](/build/OWNERS) for review.

Permanent by-design exemptions (for example, allowing `//visibility:public`
inside `bazel_sdk/`) belong in `policy_exempt_globs` on the `BazelCheck`
class in [`../checks/`](../checks/README.md) rather than in an allowlist file.

## File Naming

Each allowlist file is named `<shac_check_name>.json` (for example,
`bazel_target_visibility.json`). If no `<shac_check_name>.json` file exists for
a check, that check has no allowlisted exceptions.

## Format

Each allowlist file is a JSON object with a single top-level `"entries"` array.
Entries should be kept in alphabetical order by `path` or `path_prefix`.

### Entry Fields

* `path` or `path_prefix` (**exactly one required**):
  * `path`: Repository-relative path to a single file (must not end with `/`).
  * `path_prefix`: Repository-relative directory prefix (must end with `/`).
  * To avoid leaking private paths, entries beginning with `vendor` may only be
    `vendor/BUILD.bazel`, `vendor/google/BUILD.bazel`, `vendor/`, or
    `vendor/google/`.
* `bug` (**required**): Full Fuchsia issue URL of the form
  `https://fxbug.dev/<id>`.
* `comment` (*optional*): Non-empty string explaining why the exception is
  needed.
* `targets` (*optional*): Non-empty list of target names within `path`. Only
  supported for checks with `supports_targets_allowlist = True` and only valid
  when `path` (not `path_prefix`) is specified.

### Example

```json
{
    "entries": [
        {
            "path": "examples/my_component/BUILD.bazel",
            "bug": "https://fxbug.dev/123456789",
            "comment": "Replace with new_component and delete."
        },
        {
            "path_prefix": "examples/tools/",
            "bug": "https://fxbug.dev/567965595",
            "comment": "Migrate tool targets in //examples/tools."
        },
        {
            "path": "tools/fake_host_tool/BUILD.bazel",
            "targets": [
                "host_tests"
            ],
            "bug": "https://fxbug.dev/987654321",
            "comment": "Fix something in the host tool test suite."
        }
    ]
}
```
