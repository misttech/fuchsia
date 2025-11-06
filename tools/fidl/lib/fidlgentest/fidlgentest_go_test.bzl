"""A macro for running go tests that use the fidlgentest library."""

# TODO(https://fxbug.dev/442637596): This will likely need to be changed to a
# custom version of `go_test()` as in GN.
load("@io_bazel_rules_go//go:def.bzl", "go_test")

def fidlgentest_go_test(
        name,
        embed,
        deps = [],
        data = [],
        args = [],
        visibility = None,
        **kwargs):
    """Declares a Go test that uses the fidlgentest library.

    The fidlgentest library runs fidlc at runtime, so tests using it must make
    fidlc available and pass its path as an argument to the test binary. This
    macro takes care of that.

    Args: Same as go_test().
    """
    _fidlc_target = "//tools/fidl/fidlc"

    test_data_name = name + "_test_data"

    # TODO(https://fxbug.dev/442637596): Make this host_test_data() or similar.
    native.filegroup(
        name = test_data_name,
        srcs = [_fidlc_target],
        visibility = ["//visibility:private"],
    )
    data = data[:]
    data.append(test_data_name)

    # The test of fidlgentest itself already passes fidlgentest as the `embed`
    # so do not duplicate it as a `deps`.
    if embed != [":fidlgentest"]:
        deps = deps[:]
        deps.append("//tools/fidl/lib/fidlgentest")

    args = args[:]
    args.extend([
        "--fidlc",
        "$(execpath %s)" % _fidlc_target,
    ])
    data.append(_fidlc_target)

    go_test(
        name = name,
        embed = embed,
        args = args,
        data = data,
        deps = deps,
        visibility = visibility,
        **kwargs
    )
