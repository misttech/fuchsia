# Using Bazel Host Tool Targets in GN

After creating your Bazel host tool target, expose it to the GN build graph so
it can be used by other GN targets.

## 1. Register the Target
Refer to the example below to add migrated Bazel tool to the `default_bazel_root_host_targets`.
- For host tools under `//tools` directory, add the migrated Bazel host tool
  to the `tools_bazel_root_targets` list in `//tools/bazel_root_targets_list.gni`.
- For other host tools, add the migrated Bazel host tool to the `default_bazel_root_host_targets`
  list in `//build/bazel/bazel_root_targets_list.gni`

```gn
default_bazel_root_host_targets = sdk_host_tool_bazel_targets + [
  # ...
  {
    bazel_label = "//{directory_path}:{target_name}"

    # By default, this list looks for your Bazel host tool output at
    #
    #   {{BAZEL_TARGET_OUT_DIR}}/{target_name}
    #
    # For the above label, it is
    #
    #   bazel-bin/{directory_path}/{target_name}
    #
    # Only set this field if your output is written to a different location
    # (e.g. `go_binary` in Bazel puts output in a `{target_name}_` directory).
    #
    # This field supports special substitution expressions, which can be found
    # in //build/bazel/bazel_action.gni.
    #
    copy_outputs = [
      {
        bazel = "{{BAZEL_TARGET_OUT_DIR}}/{target_name}_/{target_name}"
        ninja = "{target_name}"
      }
    ]

    # Only set this to true if the migrated host tool target was previously
    # wrapped with an `install_host_tools` target in `BUILD.gn`.
    install_host_tool = true
  },
]
```

### Handling `install_host_tools`

If your GN host tool was wrapped in an `install_host_tools` target:

1.  **Do NOT** create an `install_host_tools` target in `BUILD.bazel`.
2.  Set `install_host_tool = true` in the `default_bazel_root_host_targets`
    entry.
3.  Remove the `install_host_tools` target from `BUILD.gn`.

## 2. Verify Usability

Confirm the tool is usable from GN:

```bash
fx build --host //build/bazel/host:bazel_root_host_tools.{target_name}
```

If you set `install_host_tool = true`, also verify:

```bash
fx build --host //build/bazel/host:bazel_root_host_tools.{target_name}.host_tool
```

## 3. Update Dependencies

Replace all references to the old GN targets with the new Bazel shortcuts:

*   **Direct Binary Reference:**
    Replace references to the GN binary target (e.g., `go_binary`,
    `rustc_binary`, `executable`) with:
    `//build/bazel/host:bazel_root_host_tools.{target_name}`

*   **Install Host Tool Reference:**
    Replace references to the `install_host_tools` wrapper with:
    `//build/bazel/host:bazel_root_host_tools.{target_name}.host_tool`

## 4. Cleanup

Remove the migrated GN targets (binary and `install_host_tools`) from
`BUILD.gn`.