# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@rules_cc//cc:defs.bzl", "cc_library")

visibility(["//build/bazel/rules/cc/..."])

# LINT.IfChange(library_headers)

def _cc_library_headers_impl(
        name,
        hdrs,
        include_dir,
        deps,
        implementation_deps,
        defines,
        testonly,
        visibility):
    """Implementation for the cc_library_headers() macro."""

    # TODO(https://fxbug.dev/456186319): When adding support for building
    # Zircon, add the following when `is_kernel`.
    # deps += [ "//zircon/system/public" ]

    cc_library(
        name = name,
        hdrs = hdrs,
        includes = [include_dir],
        deps = deps,
        implementation_deps = implementation_deps,
        defines = defines,
        testonly = testonly,
        visibility = visibility,
    )

cc_library_headers = macro(
    doc = """Defines the set of public headers for a given cc_library() and its
eventual dependencies.

Other targets can depend on a cc_library_headers() target directly if they do
not need to link to the library itself, e.g. if they include the headers
to get type definitions only. This ensures higher build parallelism.

A few important tips to use these efficiently:

- Naming convention:

  A very common naming convention is to use "foo-headers" to name the
  cc_library_headers() target used by library "foo".

  Alternatively, the Zircon artifacts have been using a slightly different
  convention used to shorten references to the target's name, i.e.:

  - If the library's target name is the same as the directory that defines
    it (e.g. //some/dir/foo:foo), use //some/dir/foo:headers as the label
    for the headers target as in:

       # In //some/dir/foo/BUILD.bazel:
       cc_library_headers("headers") { ... }

       cc_library("foo") { ... }

    This allows references to look like //some/dir/foo:headers.

  - If this is not the case (e.g. //some/dir/foo:bar), then use the
    //some/dir/foo:bar.headers label instead, as in:

    # In //some/dir/foo/BUILD.bazel:
    cc_library_headers("bar.headers")

    static_library("bar") { ... }

  In the case where the target is the only thing defined by the BUILD.bazel
  file, it is ok to use //some/dir/foo:foo as its label, as in:

    cc_library_headers("foo") { ... }

  But try to limit this to cases where it is certain that no library with
  the same name will be created in the future, to avoid updating all
  dependents when renaming the target from "foo" to "foo-headers" if that
  happens.

- Header location:

  By default, the template assumes all paths listed in the `hdrs` argument
  are in an `include` subdirectory of the current package directory.
  This can be overriden by defining the `include_dir` argument to a different
  value. For example, if all headers and sources are in the same directory
  as the BUILD.bazel file, one can use:

    cc_library_headers("headers") {
      include_dir = "."
      hdrs = [ "foo.h" ]
    }

- Dependencies:

  It is important to always depend on a cc_library_headers() target through
  public `deps`, and _not_ `implementation_deps`, as it ensures dependents will
  use the right include directory in their search path.

  As such, a cc_library_headers() target should nearly never use
  `implementation_deps`, except when absolutely needed (i.e. when headers are
  auto-generated).

  This means that the library that owns the headers from the target should
  depend on the cc_library_headers() target through `deps`, to ensure that
  anything that depends on it will be able to include the headers properly,
  as in:

    cc_library_headers("headers") {
      hdrs = [
        "foo.h",
      ]
    }

    cc_library("foo") {
      srcs = [
        "foo.cc",
      ]
      deps = [ ":headers" ]
    }
""",
    implementation = _cc_library_headers_impl,
    attrs = {
        "hdrs": attr.label_list(
            doc = "A list of header file paths.",
            allow_files = True,
            mandatory = True,
        ),
        "include_dir": attr.string(
            doc = "Path to the top-level include directory that contains the " +
                  "header files for this library. Defaults to 'include'.",
            default = "include",
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "A set of public dependencies for the headers. This is " +
                  "useful when the headers include public headers from another library",
        ),
        "implementation_deps": attr.label_list(
            doc = "Avoid using this to depend on other headers targets. " +
                  "Using `implementation_deps` might be necessary in the " +
                  "case where the headers are auto-generated, though.",
        ),
        "defines": attr.string_list(
            doc = "Usual `cc_library()` meaning.",
        ),
        "testonly": attr.bool(
            doc = "Usual meaning.",
            configurable = False,
        ),
    },
)

# LINT.ThenChange(//build/cpp/library_headers.gni:library_headers)
