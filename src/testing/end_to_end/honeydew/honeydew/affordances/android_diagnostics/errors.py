# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Contains errors raised by AndroidDiagnostics affordance."""

from honeydew import errors


class AndroidDiagnosticsError(errors.AffordanceError):
    """Exception for errors raised by AndroidDiagnostics affordance."""
