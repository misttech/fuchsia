# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Aspects for inspecting the build_flags() applied to a target."""

load(":providers.bzl", "BuildFlagsListInfo")

BuildFlagsLabelsInfo = provider(
    doc = "The ordered list of build_flags() labels applied to a target.",
    fields = {
        "labels": """(list[Label]) Labels of the final BuildFlagsInfo values
            applied to the target, in order, after deduplication and removal
            of `disable_build_flags`.""",
    },
)

def _collect_build_flags_labels_aspect_impl(target, ctx):
    # Targets wrapped with build_flags() support (see
    # wrap_cc_macro_args_with_build_flags() in cc.bzl) have exactly one dep
    # providing BuildFlagsListInfo: their `<name>.final_build_flags` target.
    deps = getattr(ctx.rule.attr, "deps", [])
    lists = [dep[BuildFlagsListInfo] for dep in deps if BuildFlagsListInfo in dep]
    if len(lists) != 1:
        fail("Expected exactly 1 dep providing BuildFlagsListInfo on {}, got {}".format(
            target.label,
            len(lists),
        ))
    return [
        BuildFlagsLabelsInfo(
            labels = [info.label for info in lists[0].infos],
        ),
    ]

collect_build_flags_labels_aspect = aspect(
    doc = """Collects the final build_flags() labels applied to a target.

    Provides BuildFlagsLabelsInfo from the single dep of the target that
    provides BuildFlagsListInfo.
    """,
    implementation = _collect_build_flags_labels_aspect_impl,
    provides = [BuildFlagsLabelsInfo],
)
