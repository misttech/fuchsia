#!/bin/sh
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

INCLUDE_DIRS=$1
shift

DTC_ARGS=""
if [[ "$INCLUDE_DIRS" != "--" ]]; then

  for i in `cat "$INCLUDE_DIRS"`; do
    DTC_ARGS="$DTC_ARGS -i $i"
  done
fi

exec dtc $@ $DTC_ARGS
