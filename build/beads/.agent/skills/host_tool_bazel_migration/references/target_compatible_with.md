# target_compatible_with

All Bazel host tool targets need to set the `target_compatible_with` attribute
to:

* `HOST_OS_CONSTRAINTS`, if it is shipped in the IDK;
* `HOST_CONSTRAINTS`, otherwise.

```bazel
# BUILD.bazel

...
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
...

go_binary_host_tool(
  name = "tool",
  target_compatible_with = HOST_CONSTRAINTS,
  ...
)

go_binary_host_tool(
  name = "idk_tool",
  target_compatible_with = HOST_OS_CONSTRAINTS,
  ...
)
```