# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by AndroidSession affordance."""

from honeydew import errors


class AndroidSessionError(errors.AffordanceError):
    """Exception to be raised by AndroidSession affordance."""
