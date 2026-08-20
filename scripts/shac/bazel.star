# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines SHAC checks for Bazel files."""

def _bazel_default_applicable_licenses(ctx):
    """Checks that non-third-party BUILD.bazel files have default_applicable_licenses set."""
    allowlist_path = "build/bazel/shac/allowlists.json"
    data = json.decode(str(ctx.io.read_file(allowlist_path)))
    ignored_prefixes = data.get("ignored_prefixes", [])
    ignored_files = data.get("ignored_files", [])

    for f in ctx.scm.affected_files(glob = ["BUILD.bazel"]):
        should_ignore = False
        if f in ignored_files:
            should_ignore = True
        else:
            for p in ignored_prefixes:
                prefix = p if p.endswith("/") else p + "/"
                if f.startswith(prefix):
                    should_ignore = True
                    break

        if should_ignore:
            continue

        contents = str(ctx.io.read_file(f))

        # Match package( ... default_applicable_licenses = ["//:license"] ... )
        # Using [^)]* to match anything except closing parenthesis to stay within the package call.
        if not ctx.re.allmatches(r"package\([^)#]*default_applicable_licenses\s*=\s*\[\"//:license\"\][^)]*\)", contents):
            ctx.emit.finding(
                level = "error",
                message = "BUILD.bazel files must include `default_applicable_licenses = [\"//:license\"]` within a `package()` call to set the default file-level license.",
                filepath = f,
            )

def _enforce_bazel_build_file(ctx):
    """Checks that a BUILD.bazel file exists when a new FIDL library is created."""

    # Check when :
    # 1. The file is a new BUILD.gn file added under //sdk/fidl/.
    # 2. The BUILD.gn file contains at least one fidl() template.
    for path, meta in ctx.scm.affected_files(glob = "BUILD.gn").items():
        if meta.action != "A":
            continue

        # TODO(https://fxbug.dev/487883318): Extend the check to FIDL libraries outside //sdk/fidl directory.
        if not path.startswith("sdk/fidl/"):
            continue

        # Read the BUILD.gn content
        content = str(ctx.io.read_file(ctx.scm.root + "/" + path))

        # Check if it contains at least one fidl() template
        has_fidl = False
        for line in content.splitlines():
            # Ignore comments
            hash_idx = line.find("#")
            if hash_idx != -1:
                line_code = line[:hash_idx]
            else:
                line_code = line
            if ctx.re.allmatches(r"\bfidl\s*\(\s*[\"\']", line_code):
                has_fidl = True
                break

        # Skip if the BUILD.gn file doesn't contain any fidl() template
        if not has_fidl:
            continue

        # Check if BUILD.bazel exists in the same directory
        bazel_path = "/".join(path.split("/")[:-1] + ["BUILD.bazel"])

        if not ctx.scm.all_files(glob = bazel_path):
            ctx.emit.finding(
                level = "error",
                filepath = path,
                message = "BUILD.bazel file is missing in the same directory as the added FIDL BUILD.gn file.",
            )

def register_bazel_checks():
    shac.register_check(shac.check(_bazel_default_applicable_licenses, formatter = False))
    shac.register_check(shac.check(_enforce_bazel_build_file))
