# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("fx_cc_binary.bzl", _fx_cc_binary = "fx_cc_binary")
load("fx_cc_library.bzl", _fx_cc_library = "fx_cc_library")
load("fx_cc_library_headers.bzl", _fx_cc_library_headers = "fx_cc_library_headers")

fx_cc_library = _fx_cc_library
fx_cc_binary = _fx_cc_binary
fx_cc_library_headers = _fx_cc_library_headers
