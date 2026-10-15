A number of GN config() definitions depend on the presence of various
tag strings in `toolchain_variant.tags`, which indicate certain properties
of the current GN toolchain(). This takes the form of a conditional
check that can look like:

```py
# Holds true if "instrumented" appears in toolchain_variant.tags
# GN doesn't have a string_list_contains() operator hence the weird syntax
_is_instrumented = toolchain_variant.tags + ["instrumented"] - ["instrumented"] != toolchain_variant.tags

config("foo") {
  if (_is_instrumented) {
    rustflags = [ "...." ]
  }
}
```

When converting these to Bazel `build_flags()` equivalents, a corresponding
`build_setting()` variable must be defined, with appropriate predicates to
check for true/false conditions.

This directory contains a script (`update.py`) that can automate the definition
of such Bazel targets. When run, it produces the following:

- `//build/bazel/config/toolchain_tags/BUILD.bazel`

  A file containing a separate `bool_flag()` target definition for each
  one of the supported toolchain tags.

- `//build/bazel/config/toolchain_tags/toolchain_tags.gni`

  A .gni file included from `//build/bazel/config/bazel_args.gni` to
  produce a list of Bazel configuration flags that will ensure the
  Bazel platform sets the values of the `bool_flag()` targets properly
  for the host and Fuchsia platforms.

- `//build/bazel/config/toolchain_tags/<tag>/BUILD.bazel`:

  For each tag, a build file declaring predicates usable in `select()`
  statements for conditional definitions in other BUILD.bazel files
  that need them.

NOTE: The list of supported tags is currently hard-coded in the `update.py`
script, and will be updated as more `build_flags()` requiring flags for
new tags appear. Most of the toolchain and variant tags defined in GN
are mainly used by the Zircon part of the build, so do not need to be
exposed to Bazel at all.

The script must be run manually each time to add new supported tag
definitions.

The `config()` example above could be translated as:

```
build_flags(
    name = "foo",
    rustflags = select({
        "//build/bazel/config/toolchain_tags/instrumented:true": [ "....." ],
        "//conditions:default": [],
    }),
    visibility = ["//visibility:public"],
)
```
