#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A lightweight formatter script to strip Fuchsia copyright headers and mypy ignore directives.
#
# WHY THIS IS NEEDED:
# 1. protoc generates candidate Python bindings (_pb2.py) without copyright headers.
# 2. Fuchsia repository policies require checked-in golden files to have copyright headers.
# 3. GN's golden_files template applies the formatter script ONLY to the golden file before
#    comparing.
#
# By stripping the copyright header from the golden file on-the-fly during comparison,
# we can match the raw candidate perfectly without having to modify protoc's output,
# while still keeping mandatory copyright headers in the source tree goldens.

line_num=0
skipped_ignore=0

while IFS= read -r line || [[ -n "$line" ]]; do
  ((line_num++))

  # 1. Skip standard Fuchsia copyright lines (1 to 3) if they match the pattern
  if (( line_num <= 3 )) && [[ "$line" =~ ^#\ (Copyright|Use\ of|found\ in) ]]; then
    continue
  fi

  # 2. Skip any blank line immediately following the copyright header
  if (( line_num == 4 )) && [[ -z "$line" ]]; then
    continue
  fi

  # 3. Skip the "# type: ignore" comment
  if [[ "$line" =~ ^#\ type:\ ignore ]]; then
    skipped_ignore=1
    continue
  fi

  # 4. Skip any blank line immediately following the "# type: ignore" comment
  if (( skipped_ignore == 1 )) && [[ -z "$line" ]]; then
    skipped_ignore=0
    continue
  fi

  # Print the remaining lines exactly as they are
  printf "%s\n" "$line"
done
