# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("./common.star", "FORMATTER_MSG", "cipd_platform_name", "get_fuchsia_dir", "os_exec")

def _dart_format(ctx):
    """Runs `dart format`.

    Args:
      ctx: A ctx instance.
    """
    dart_files = ctx.scm.affected_files(glob = "*.dart")
    if not dart_files:
        return

    dart_exe = "%s/prebuilt/third_party/dart/%s/bin/dart" % (
        get_fuchsia_dir(ctx),
        cipd_platform_name(ctx),
    )

    res = os_exec(
        ctx,
        [dart_exe, "format", "--output=json"] + list(dart_files),
    ).wait()
    for line in res.stdout.splitlines():
        doc = json.decode(line)
        filepath = doc["path"]
        new_contents = doc["source"]
        old_contents = str(ctx.io.read_file(filepath))
        if new_contents != old_contents:
            ctx.emit.finding(
                level = "error",
                message = FORMATTER_MSG,
                filepath = filepath,
                replacements = [new_contents],
            )

def register_dart_checks():
    shac.register_check(shac.check(_dart_format, formatter = True))
