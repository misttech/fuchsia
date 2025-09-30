#!/usr/bin/env bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

diff $2 $3 > $1
if [[ $? -ne 0 ]]; then
  echo "Error: Golden check failure. The following files differ:"
  echo "  Current:   $2"
  echo "  Generated: $3"
  echo "If this is intentional, update the checked-in file using the following command:"
  echo "  cp \$FUCHSIA_DIR/out/default/$3 \\"
  echo "     \$FUCHSIA_DIR/out/default/$2"
  exit 1
fi
