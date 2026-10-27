# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks that config_setting() and exports_files() explicitly specify visibility."""

from framework import (
    BazelCheck,
    CallInfo,
    FileContext,
    FileKind,
    register_check,
)

from .visibility_utils import (
    PUBLIC_BY_DEFAULT_TYPES,
    VISIBILITY_FOR_PUBLIC_BY_DEFAULT_REFERENCE_URL,
    specifies_visibility,
)


@register_check
class VisibilityForPublicByDefaultTypesCheck(BazelCheck):
    """Requires `config_setting()` and `exports_files()` to explicitly set `visibility`."""

    name = "bazel_visibility_for_public_by_default_types"
    file_kinds = frozenset({FileKind.BUILD, FileKind.BZL})
    supports_allowlist = True
    # Note: Only named targets (such as `config_setting(name = ...)`) can be
    # exempted via `"targets"` in the allowlist. `exports_files()` is a
    # function without a `name` attribute, so files with `exports_files()`
    # violations require a whole-file `"path"` allowlist entry.
    supports_targets_allowlist = True

    def check_call(self, fc: FileContext, call: CallInfo) -> None:
        if call.func_name not in PUBLIC_BY_DEFAULT_TYPES:
            return
        if specifies_visibility(call):
            return

        fc.report(
            f"`{call.func_name}()` must explicitly specify `visibility` because it is public by default.",
            call.node,
            level="error",
            url=VISIBILITY_FOR_PUBLIC_BY_DEFAULT_REFERENCE_URL,
        )
