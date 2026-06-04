# Rust Migration Reference

GN `rustc_binary` and `rustc_library` map to Bazel `rustc_binary` and `rustc_library` (loaded from `//build/bazel/rules/rust:defs.bzl`).

## Field Mapping Gotchas

Only key field differences are listed here. Standard fields like `sources` -> `srcs` and `deps` -> `deps` apply normally.

| GN Field | Bazel Attribute | Notes |
| :--- | :--- | :--- |
| `output_name` | `crate_name` | The crate name used for linking and resulting binary name. |
| `with_unit_tests = true` | `with_host_unit_tests = True` | Set to `True` to enable host unit tests. |
| `features` | `crate_features` | Features enabled for this crate. |

### Third-Party Dependencies

When migrating third-party dependencies from GN to Bazel, prefix with the vendor directory path:
- **GN:** `"//third_party/rust_crates:anyhow"`
- **Bazel:** `"//third_party/rust_crates/vendor:anyhow"`

*Note: Some crates may be located under `ask2patch`, `fork`, or `intree` instead of `vendor` (e.g., `//third_party/rust_crates/ask2patch/walkdir`).*

## Example

```gn
# BUILD.gn
import("//build/rust/rustc_binary.gni")

if (is_host) {
  rustc_binary("tool_bin") {
    sources = [ "src/main.rs" ]
    edition = "2024"
    deps = [ "//third_party/rust_crates:anyhow" ]
    with_unit_tests = true
    test_deps = [ "//third_party/rust_crates:tempfile" ]
  }
}
```

should be migrated to:

```bazel
# BUILD.bazel
load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

rustc_binary(
    name = "tool_bin",
    srcs = ["src/main.rs"],
    edition = "2024",
    target_compatible_with = HOST_CONSTRAINTS,
    deps = [
        "//third_party/rust_crates/vendor:anyhow",
    ],
    with_host_unit_tests = True,
    test_deps = [
        "//third_party/rust_crates/vendor:tempfile",
    ],
)
```

## Common Pitfalls and Best Practices

### 1. Preventing Redundant Binary Syncs
Add `# @bazel2gn:skip` on the line immediately preceding `rustc_binary` in `BUILD.bazel` to prevent `bazel2gn` from generating conflicting GN targets.

### 2. Preserving GN Target Shape for Unit Tests
Match the GN test structure exactly to ensure correct `bazel2gn` sync:
- **Do not merge** standalone GN `rustc_test` targets into `with_host_unit_tests = True` in Bazel.
- **Do not split** a GN `rustc_library` with `with_unit_tests = true` into separate Bazel library and test targets; use `with_host_unit_tests = True`.

### 3. Specifying Crate Root
If a target has multiple `srcs` and does not use `src/lib.rs` (or `src/main.rs` for binary), explicitly set `crate_root` (e.g., `crate_root = "src/main.rs"`). Otherwise, `bazel2gn` won't generate `source_root` in GN, causing Ninja build errors if GN defaults to `src/lib.rs`.

### 4. Test Data and Genrules
If a host test requires test data:
- **Map Bazel `data` to GN `host_test_data`**: Use annotations to overwrite the path:
  ```bazel
  # @bazel2gn:transformer=deps
  data = [
      ":my_test_data",  # @bazel2gn:path_overwrite::my_gn_test_data
  ],
  ```
- **Skip complex genrules**: `bazel2gn` cannot sync `genrule`s using system commands (like `cp`). Add `# @bazel2gn:skip` above the `genrule` in Bazel and manually maintain the corresponding `host_test_data` in GN.
