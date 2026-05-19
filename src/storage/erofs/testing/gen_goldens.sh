#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
set -euo pipefail

# Get the directory of this script
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"

cd "$SCRIPT_DIR"

# Recreate the golden files even if they exists
rm -f data/simple.erofs
rm -f data/simple_512.erofs

mkdir -p data/simple/large_dir
for i in $(seq 1 50); do
  echo "file $i" > "data/simple/large_dir/file_number_$i"
done

mkfs.erofs -b 4096 data/simple.erofs data/simple
mkfs.erofs -b 512 data/simple_512.erofs data/simple

# Clean up
rm -rf data/simple/large_dir

echo "All golden EROFS images generated successfully."
