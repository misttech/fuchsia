# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load(
    "@fuchsia_rules_common//build_flags:build_flags.bzl",
    "wrap_cc_rule_with_build_flags",
    "wrap_rust_rule_with_build_flags",
)
load("@rules_cc//cc:cc_binary.bzl", "cc_binary")
load("@rules_cc//cc:cc_library.bzl", "cc_library")
load("@rules_rust//rust:rust_binary.bzl", "rust_binary")
load("//build/bazel/host_tests:host_test.bzl", "host_test")

def build_flags_cc_binary_host_test(
        name,
        with_build_flags = [],
        without_build_flags = [],
        **kwargs):
    """Define a new C++ or C host test for build_flags() support.

    Args:
       name: (string) Name of the host_test() target, the binary will be named "{name}_bin".
       with_build_flags: (list[label]) optional list of labels to build_flags() targets.
       without_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to cc_binary().
    """
    binary_name = "{}_bin".format(name)

    new_kwargs = wrap_cc_rule_with_build_flags(
        kwargs,
        name,
        "cc_binary",
        with_build_flags,
        without_build_flags,
        "executable",
    )

    cc_binary(
        name = binary_name,
        **new_kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
    )

def build_flags_rust_binary_host_test(
        name,
        with_build_flags = [],
        without_build_flags = [],
        **kwargs):
    """Define a new Rust host test for build_flags() support.

    This creates a rust_binary() target and associated host_test() wrapper.
    This checks the implementation of wrap_rust_rule_with_build_flags()
    in isolation, independent of modification to the rustc_binary()
    wrapper macro.

    Args:
       name: (string) Name of the host_test() target, the binary will be named "{name}_bin".
       with_build_flags: (list[label]) optional list of labels to build_flags() targets.
       without_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to rustc_binary().
    """
    binary_name = "{}_bin".format(name)

    new_kwargs = wrap_rust_rule_with_build_flags(
        kwargs,
        name,
        "rust_binary",
        with_build_flags,
        without_build_flags,
        "executable",
    )

    rust_binary(
        name = binary_name,
        **new_kwargs
    )

    host_test(
        name = name,
        binary = binary_name,
    )

def build_flags_cc_library(
        name,
        with_build_flags = [],
        without_build_flags = [],
        **kwargs):
    """Define a new C++ library for build_flags() support.

    Args:
       name: (string) Name of the library target.
       with_build_flags: (list[label]) optional list of labels to build_flags() targets.
       without_build_flags: (list[label]) optional list of labels to build_flags() targets.
       **kwargs: All other arguments are passed to cc_library().
    """
    new_kwargs = wrap_cc_rule_with_build_flags(
        kwargs,
        name,
        "cc_library",
        with_build_flags,
        without_build_flags,
        "common",
    )

    cc_library(
        name = name,
        **new_kwargs
    )
