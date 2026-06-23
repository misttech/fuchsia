# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("./common.star", "compiled_tool_path", "os_exec")

def _filter_readme_fuchsia_files(ctx, files):
    allowlist_path = "tools/readme_fuchsia/assets/allowlist.json"
    data = json.decode(str(ctx.io.read_file(allowlist_path)))
    ignored_prefixes = data.get("ignored_prefixes", [])
    ignored_files = data.get("ignored_files", [])

    return [
        f
        for f in files
        if not any([f.startswith(p) for p in ignored_prefixes]) and f not in ignored_files
    ]

def _readme_fuchsia_required_fields(ctx):
    """Runs `readme_fuchsia validate <file>`

    Args:
      ctx: A ctx instance.
    """
    exe = compiled_tool_path(ctx, "readme_fuchsia")

    procs = []
    for f in _filter_readme_fuchsia_files(ctx, ctx.scm.affected_files(glob = "README.fuchsia")):
        args = [exe, "validate"]

        args.append(f)
        procs.append((f, os_exec(ctx, args, ok_retcodes = (0, 1))))

    for f, proc in procs:
        res = proc.wait()
        if res.retcode != 0:
            for line in res.stderr.strip().split("\n"):
                line = line.strip()
                if not line:
                    continue
                ctx.emit.finding(
                    level = "error",
                    message = line,
                    filepath = f,
                )

def register_readme_fuchsia_checks():
    shac.register_check(shac.check(_readme_fuchsia_required_fields, name = "readme_fuchsia_required_fields"))
