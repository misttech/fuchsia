# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
A repository rule `fuchsia_icu_config_repository` generates an external repo
that contains git commit ID information about the third party ICU repositories
contained in @//third_party/icu.

Defines a constant `icu_flavors` which is a struct containing these elements:

- `default_git_commit`(string): the detected git commit ID for
   `@//:third_party/icu_default`
- `latest_git_commit`(string): the detected git commit ID for
   `@//:third_party/icu_latest`

This struct can be ingested by main build rules by using:

In WORKSPACE.bazel:

```
load ("//:bazel/icu/repository_rules.bzl:", "fuchsia_icu_config_repository")

fuchsia_icu_config_repository(name = "fuchsia_icu_config")
```

in BUILD files:

```
load("@fuchsia_icu_config//:constants.bzl", "icu_flavors")
```

"""

_CONSTANTS_BZL_TEMPLATE = """# AUTO_GENERATED - DO NOT EDIT!

icu_flavors = struct(
    default_git_commit = "{default_commit}",
    latest_git_commit = "{latest_commit}",
)
"""

def _fuchsia_icu_config_impl(repo_ctx):
    ninja_build_dir_path = repo_ctx.path("%s/%s" % (repo_ctx.workspace_root, repo_ctx.attr.ninja_build_dir))

    # Ensure this repository is regenerated any time the content hash file
    # changes. Creating a content hash file at `fx gen` time allows
    # grabbing the correct path to the real .git/HEAD when submodules are
    # used, which is harder to use in Starlark than in Python.
    if hasattr(repo_ctx.attr, "content_hash_file"):
        repo_ctx.path(Label("@//:" + repo_ctx.attr.content_hash_file))

    icu_build_config_file = repo_ctx.path("%s/icu_build_config.json" % ninja_build_dir_path)
    contents = repo_ctx.read(icu_build_config_file)
    icu_config = json.decode(contents)

    constants_bzl = _CONSTANTS_BZL_TEMPLATE.format(
        default_commit = icu_config["default"],
        latest_commit = icu_config["latest"],
    )

    repo_ctx.file("constants.bzl", constants_bzl)

    repo_ctx.file("WORKSPACE.bazel", """# DO NOT EDIT! Automatically generated.
workspace(name = "fuchsia_icu_config")
""")
    repo_ctx.file("BUILD.bazel", """# DO NOT EDIT! Automatically generated.
exports_files(glob(["**/*"]))""")

fuchsia_icu_config_repository = repository_rule(
    implementation = _fuchsia_icu_config_impl,
    doc = "Create a repository that contains ICU configuration information in its //:constants.bzl file.",
    attrs = {
        "content_hash_file": attr.string(
            doc = "Path to content hash file for this repository, relative to workspace root.",
            mandatory = False,
        ),
    },
)
