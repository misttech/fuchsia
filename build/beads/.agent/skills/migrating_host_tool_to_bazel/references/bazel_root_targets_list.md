# Using Bazel Host Tool Targets in GN

After creating your Bazel host tool target, you need to expose it to the GN
build graph so it can be used by other GN targets.

## 1. Register the Target

Add your migrated Bazel host tool to the `default_bazel_root_host_targets` list
in `//build/bazel/bazel_root_targets_list.gni`.

A typical entry in the list looks like:

```gn
default_bazel_root_host_targets = sdk_host_tool_bazel_targets + [
  {
    # ...
    {
      bazel_label = "//path/to/your/bazel:tool"

      # By default, this list looks for your Bazel host tool output at
      #
      #   {{BAZEL_TARGET_OUT_DIR}}/{tool_name}
      #
      # For the above label, it is
      #
      #   bazel-bin/path/to/your/bazel/tool
      #
      # Only set this field if your output is written to a different location
      # (e.g. `go_binary` in Bazel puts output in a `tool_` directory).
      #
      # This field supports special substitution expressions, which can be found
      # in //build/bazel/bazel_action.gni.
      #
      copy_outputs = [
        {
          bazel = "{{BAZEL_TARGET_OUT_DIR}}/tool_/tool"
          ninja = "tool"
        }
      ]

      # Only set this to true if the migrated host tool target was previously
      # wrapped with an `install_host_tools` target in `BUILD.gn`.
      install_host_tool = true
    },
  }
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
fx build --host //build/bazel/host:bazel_root_host_tools.{tool}
```

If you set `install_host_tool = true`, also verify:

```bash
fx build --host //build/bazel/host:bazel_root_host_tools.{tool}.host_tool
```

## 3. Update Dependencies

Replace all references to the old GN targets with the new Bazel shortcuts:

*   **Direct Binary Reference:**
    Replace references to the GN binary target (e.g., `go_binary`, `rustc_binary`, `executable`) with:
    `//build/bazel/host:bazel_root_host_tools.{tool}`

*   **Install Host Tool Reference:**
    Replace references to the `install_host_tools` wrapper with:
    `//build/bazel/host:bazel_root_host_tools.{tool}.host_tool`

## 4. Cleanup

Remove the migrated GN targets (binary and `install_host_tools`) from `BUILD.gn`.