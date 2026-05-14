# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Providers for debug symbols."""

FuchsiaDebugSymbolInfo = provider(
    doc = "Contains information that can be used to register debug symbols.",
    fields = {
        "build_id_dirs_mapping": """A { source_search_root -> depset[build_id_dir] } dictionary,
            where 'build_id_dir' is a File value pointing to a .build-id/ directory, and
            'source_search_root' is either a string or a File value, used to locate source files
            when using the debugger.

            The source paths embedded in debug symbol files are usually relative. Historically, these
            were relative to the Ninja build directory (e.g. "../../src/foo/foo.cc"), which is why
            this is key is named 'build_dir' in files like symbol-index.json. In the context of
            Bazel, these source paths are relative to the Bazel exec_root instead, which is
            different from the Ninja build directory.

            If 'source_search_root' is a string, it is interpreted as an environment variable
            name, which must be defined by Bazel when the action that registers debug symbols
            is run, such as BUILD_WORKSPACE_DIRECTORY (see Bazel user manual).

            If 'source_search_root' is a File pointing to a directory, the latter is used
            directly as a possible source search directory.

            If 'source_search_root' is a File pointing to a file, its parent directory is used
            instead as a possible source search directory.
            """,
    },
)
