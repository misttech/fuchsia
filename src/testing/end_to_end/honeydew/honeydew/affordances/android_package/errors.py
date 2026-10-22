# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by AndroidPackage affordance."""

from honeydew import errors


class AndroidPackageError(errors.AffordanceError):
    """Exception to be raised by AndroidPackage affordance."""
