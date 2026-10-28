# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule for archiving a prebuilt emulator directory for host tests."""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")

visibility("private")

def _emulator_archive_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.name + ".tar.gz")
    manifest = ctx.actions.declare_file(ctx.attr.name + ".manifest")
    ctx.actions.write(manifest, ".=%s\n" % ctx.file.src.path)
    ctx.actions.run(
        executable = ctx.executable._tarmaker,
        arguments = ["--manifest", manifest.path, "--output", out.path],
        inputs = [manifest, ctx.file.src],
        outputs = [out],
        mnemonic = "EmulatorArchive",
        progress_message = "Archiving emulator %{label}",
        # The emulator prebuilts are hundreds of MB, so uploading them to
        # remote execution would be slower than archiving locally.
        **LOCAL_ONLY_ACTION_KWARGS
    )
    return [DefaultInfo(files = depset([out]))]

emulator_archive = rule(
    implementation = _emulator_archive_impl,
    doc = """Archives an emulator directory as `<name>.tar.gz`.

    The infrastructure does not provide a way for host tests to access
    emulators, so host tests that boot one carry their own copy, which
    //tools/emulator extracts at runtime.
    """,
    attrs = {
        "src": attr.label(
            doc = "The emulator directory to archive, such as a " +
                  "`cipd_prebuilt_directory()` target. Its contents are " +
                  "placed at the root of the archive.",
            allow_single_file = True,
            mandatory = True,
        ),
        "_tarmaker": attr.label(
            default = "//build/tools/tarmaker",
            executable = True,
            cfg = "exec",
        ),
    },
)
