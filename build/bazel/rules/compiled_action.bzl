# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""compiled_action() custom rule definition.

This is the Bazel counterpart of the GN `compiled_action()` template defined in
//build/compiled_action.gni: it runs a tool built in the same build (in the exec
configuration, i.e. for the host) as a build action.

`bazel2gn` converts `compiled_action()` targets to GN `compiled_action()`
targets, see //build/tools/bazel2gn/compiled_action.go. To keep that conversion
exact, `args` only supports the following substitutions:

  * `$(location <label>)` and `$(execpath <label>)`, where `<label>` is a file
    in `srcs` or `outs`. In GN, these become
    `rebase_path("<path>", root_build_dir)`.
  * `$(RULEDIR)`: the output directory of this target, which contains all
    `outs`. In GN, this becomes `rebase_path(target_gen_dir, root_build_dir)`.

Example:

    compiled_action(
        name = "generate_foo",
        tool = ":foo_generator",
        srcs = ["foo.json"],
        outs = ["foo.rs"],
        args = [
            "--input",
            "$(location :foo.json)",
            "--output",
            "$(location :foo.rs)",
        ],
    )
"""

visibility(["//..."])

def _compiled_action_impl(ctx):
    rule_dir = "/".join([
        p
        for p in [ctx.bin_dir.path, ctx.label.workspace_root, ctx.label.package]
        if p
    ])
    args = []
    for raw_arg in ctx.attr.args:
        for part in raw_arg.split("$(")[1:]:
            if not (part.startswith("RULEDIR)") or part.startswith("location ") or part.startswith("execpath ")):
                fail("Unsupported $() substitution in compiled_action args: %r (only $(RULEDIR), $(location ...), and $(execpath ...) are supported)" % raw_arg)
        args.append(
            ctx.expand_location(
                raw_arg.replace("$(RULEDIR)", rule_dir),
                targets = ctx.attr.srcs,
            ),
        )

    ctx.actions.run(
        outputs = ctx.outputs.outs,
        inputs = ctx.files.srcs,
        executable = ctx.executable.tool,
        arguments = args,
        env = {"RUST_BACKTRACE": "1"},
        mnemonic = "CompiledAction",
        progress_message = "Running %s for %%{label}" % ctx.attr.tool.label.name,
    )

    return [
        DefaultInfo(
            files = depset(ctx.outputs.outs),
            runfiles = ctx.runfiles(files = ctx.outputs.outs),
        ),
    ]

compiled_action = rule(
    implementation = _compiled_action_impl,
    doc = """Runs a tool built in the same build as a build action.

    The Bazel counterpart of the GN `compiled_action()` template. The tool is
    executed directly (without a shell) from the execution root.

    See the documentation at the top of this file for supported substitutions in
    `args`.
    """,
    attrs = {
        "tool": attr.label(
            doc = """The tool to run, e.g. a `rustc_binary()` or `cc_binary()`.
            It is built in the exec (host) configuration.
            """,
            executable = True,
            mandatory = True,
            cfg = "exec",
        ),
        "srcs": attr.label_list(
            doc = """Input files of the action. These can be referred to in
            `args` with `$(location ...)`.
            """,
            allow_files = True,
        ),
        "outs": attr.output_list(
            doc = """Output files of the action. These can be referred to in
            `args` with `$(location ...)`, and are all located in `$(RULEDIR)`.
            """,
            mandatory = True,
            allow_empty = False,
        ),
        "args": attr.string_list(
            doc = """Command line arguments of the tool. Each element is passed
            as a single argument (no shell tokenization), after substituting
            `$(location ...)`, `$(execpath ...)`, and `$(RULEDIR)`.
            """,
            mandatory = True,
        ),
    },
)
