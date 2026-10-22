# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Wrapper around //tools/protobuf:rust_proto_library.bzl."""

load("//tools/protobuf:rust_proto_library.bzl", _rust_proto_library = "rust_proto_library")

def rust_proto_library(**kwargs):
    """Forwards to //tools/protobuf:rust_proto_library.bzl.

    Args:
        **kwargs: Forwarded to rust_proto_library (`crate_root` is ignored).
    """
    kwargs.pop("crate_root", None)
    _rust_proto_library(**kwargs)
