# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generates implicit unit test targets for deprecated with_*_unit_tests flags."""

load("@rules_rust//rust:rust_test.bzl", "rust_test")
load("//build/bazel/rules/host_tests:host_rustc_test.bzl", "define_and_wrap_host_rust_test")
load(
    "//build/bazel/rules/rust:common.bzl",
    "with_fuchsia_rust_link_exec_properties",
    "with_fuchsia_rust_stdlib_link_deps",
    "with_fuchsia_rust_test_aspect_hints",
)

#  ________  _________  ________  ________
# |\   ____\|\___   ___\\   __  \|\   __  \
# \ \  \___|\|___ \  \_\ \  \|\  \ \  \|\  \
#  \ \_____  \   \ \  \ \ \  \\\  \ \   ____\
#   \|____|\  \   \ \  \ \ \  \\\  \ \  \___|
#     ____\_\  \   \ \__\ \ \_______\ \__\
#    |\_________\   \|__|  \|_______|\|__|
#    \|_________|
#
# DO NOT ADD TO THIS LIST.
#
# TODO(https://fxbug.dev/572570591): Remove this allowlist once all remaining
# packages replace `with_unit_tests` and `with_host_unit_tests` on `rustc_*()`
# macros with standalone `rustc_test()` or `host_rustc_test()` targets.
_ALLOWED_PACKAGES = {
    "build/bazel/examples/hello_world": True,
    "build/bazel/rules/host_tests/tests/rust": True,
    "build/bazel/rules/rust/tests": True,
    "build/sdk/meta": True,
    "build/tools/formatjson5": True,
    "examples/power/wake_lease/rust": True,
    "examples/rcu": True,
    "examples/rust/rust_static_linking": True,
    "examples/rust/testing": True,
    "examples/testing/unittests/rust": True,
    "sdk/lib/c/rust": True,
    "sdk/lib/device-watcher/rust": True,
    "sdk/rust/zx-status": True,
    "sdk/rust/zx-types": True,
    "src/connectivity/overnet/lib/circuit": True,
    "src/developer/fastboot": True,
    "src/developer/ffx/command": True,
    "src/developer/ffx/command/error": True,
    "src/developer/ffx/config": True,
    "src/developer/ffx/config/domain": True,
    "src/developer/ffx/core/macro": True,
    "src/developer/ffx/lib/addr": True,
    "src/developer/ffx/lib/arg_parsing": True,
    "src/developer/ffx/lib/buildid": True,
    "src/developer/ffx/lib/compat_info": True,
    "src/developer/ffx/lib/diagnostics": True,
    "src/developer/ffx/lib/doctor_utils": True,
    "src/developer/ffx/lib/emulator_instance": True,
    "src/developer/ffx/lib/errors": True,
    "src/developer/ffx/lib/executor": True,
    "src/developer/ffx/lib/fastboot/interface": True,
    "src/developer/ffx/lib/fastboot/transport/interface": True,
    "src/developer/ffx/lib/fastboot_file_discovery": True,
    "src/developer/ffx/lib/fho": True,
    "src/developer/ffx/lib/fho/macro": True,
    "src/developer/ffx/lib/fho/metadata": True,
    "src/developer/ffx/lib/isolate": True,
    "src/developer/ffx/lib/logging": True,
    "src/developer/ffx/lib/metrics": True,
    "src/developer/ffx/lib/netext": True,
    "src/developer/ffx/lib/port_picker": True,
    "src/developer/ffx/lib/profiler": True,
    "src/developer/ffx/lib/rcs": True,
    "src/developer/ffx/lib/safe_string": True,
    "src/developer/ffx/lib/sdk": True,
    "src/developer/ffx/lib/ssh": True,
    "src/developer/ffx/lib/symbolize": True,
    "src/developer/ffx/lib/target_errors": True,
    "src/developer/ffx/lib/termio": True,
    "src/developer/ffx/lib/timeout": True,
    "src/developer/ffx/lib/trace": True,
    "src/developer/ffx/lib/traceable_error": True,
    "src/developer/ffx/lib/uart_driver_api": True,
    "src/developer/ffx/lib/usb_driver_api": True,
    "src/developer/ffx/lib/usb_fastboot_discovery": True,
    "src/developer/ffx/lib/version": True,
    "src/developer/ffx/lib/writer": True,
    "src/developer/ffx/tools/efi": True,
    "src/developer/fuchsia-criterion": True,
    "src/developer/lib/writer": True,
    "src/devices/lib/board_graph": True,
    "src/devices/lib/parsed_board_dump": True,
    "src/devices/lib/thermal/rust": True,
    "src/devices/usb/bin/lsusb": True,
    "src/diagnostics/lib/ring-buffer": True,
    "src/lib/agents": True,
    "src/lib/analytics/rust": True,
    "src/lib/assembly/api": True,
    "src/lib/assembly/artifact_cache": True,
    "src/lib/assembly/blobfs": True,
    "src/lib/assembly/cli_args": True,
    "src/lib/assembly/component_id_index": True,
    "src/lib/assembly/component_manager_config": True,
    "src/lib/assembly/config_capabilities": True,
    "src/lib/assembly/config_data": True,
    "src/lib/assembly/domain_config": True,
    "src/lib/assembly/driver_manifest": True,
    "src/lib/assembly/fvm": True,
    "src/lib/assembly/image_assembly_config_builder": True,
    "src/lib/assembly/memory_buckets": True,
    "src/lib/assembly/named_file_map": True,
    "src/lib/assembly/package_copy": True,
    "src/lib/assembly/package_list": True,
    "src/lib/assembly/package_utils": True,
    "src/lib/assembly/sdk": True,
    "src/lib/assembly/shell_commands": True,
    "src/lib/assembly/subpackage_blobs_package": True,
    "src/lib/assembly/tool": True,
    "src/lib/assembly/update_package": True,
    "src/lib/assembly/update_packages_manifest": True,
    "src/lib/assembly/util": True,
    "src/lib/assembly/validate_package": True,
    "src/lib/assembly/validate_util": True,
    "src/lib/assembly/vbmeta": True,
    "src/lib/async-utils": True,
    "src/lib/bitrs": True,
    "src/lib/buf-read-ext": True,
    "src/lib/collections/packed": True,
    "src/lib/cstringify": True,
    "src/lib/debug/rust": True,
    "src/lib/depfile": True,
    "src/lib/detect-stall": True,
    "src/lib/diagnostics/data/rust": True,
    "src/lib/diagnostics/fuchsia-async-inspect": True,
    "src/lib/diagnostics/hierarchy/rust": True,
    "src/lib/diagnostics/inspect/contrib/stubs": True,
    "src/lib/diagnostics/inspect/format/rust": True,
    "src/lib/diagnostics/inspect/rust": True,
    "src/lib/diagnostics/log/encoding/rust": True,
    "src/lib/diagnostics/log/types": True,
    "src/lib/diagnostics/selectors": True,
    "src/lib/diagnostics/testing/diagnostics-assertions/rust": True,
    "src/lib/directed_graph": True,
    "src/lib/fdio/rust": True,
    "src/lib/fdomain/client": True,
    "src/lib/fdomain/container": True,
    "src/lib/fdomain/local": True,
    "src/lib/fidl/rust_next/fidl_next": True,
    "src/lib/fidl/rust_next/fidl_next_bind": True,
    "src/lib/fidl/rust_next/fidl_next_codec": True,
    "src/lib/fidl/rust_next/fidl_next_protocol": True,
    "src/lib/fidl/rust_next/fidl_next_util": True,
    "src/lib/fidl_codec/rust": True,
    "src/lib/fuchsia-async": True,
    "src/lib/fuchsia-component/client": True,
    "src/lib/fuchsia-component/directory": True,
    "src/lib/fuchsia-component/runtime": True,
    "src/lib/fuchsia-component/server": True,
    "src/lib/fuchsia-emulated-handle": True,
    "src/lib/fuchsia-fs": True,
    "src/lib/fuchsia-hyper": True,
    "src/lib/fuchsia-lockfile": True,
    "src/lib/fuchsia-rcu": True,
    "src/lib/fuchsia-rcu-collections": True,
    "src/lib/fuchsia-rseq": True,
    "src/lib/fuchsia-runtime": True,
    "src/lib/fuchsia-sync": True,
    "src/lib/fuchsia-url": True,
    "src/lib/gcs": True,
    "src/lib/http-uri-ext": True,
    "src/lib/injectable-time": True,
    "src/lib/mundane": True,
    "src/lib/shared-buffer": True,
    "src/lib/structured_ui": True,
    "src/lib/tempfile-ext": True,
    "src/lib/test_util": True,
    "src/lib/testing/expectation/ser": True,
    "src/lib/testing/expectation/tool": True,
    "src/lib/trace/rust": True,
    "src/lib/usb_rs": True,
    "src/lib/utf8_path": True,
    "src/lib/versioning/version-history/rust": True,
    "src/lib/versioning/version-history/rust/data": True,
    "src/lib/versioning/version-history/rust/proc-macro": True,
    "src/lib/webpki-roots-fuchsia": True,
    "src/performance/lib/fxt/rust": True,
    "src/starnix/lib/atomic_bitflags": True,
    "src/starnix/lib/crash_throttling": True,
    "src/starnix/lib/devicetree": True,
    "src/starnix/lib/ebpf": True,
    "src/starnix/lib/ebpf_loader": True,
    "src/starnix/lib/estimate_timeline": True,
    "src/starnix/lib/expando": True,
    "src/starnix/lib/extended_pstate": True,
    "src/starnix/lib/features": True,
    "src/starnix/lib/line_discipline": True,
    "src/starnix/lib/linux_uapi": True,
    "src/starnix/lib/memory_pinning": True,
    "src/starnix/lib/seq_lock": True,
    "src/starnix/lib/starnix_c_file_buffer": True,
    "src/starnix/lib/starnix_crypto": True,
    "src/starnix/lib/starnix_ext": True,
    "src/starnix/lib/starnix_lifecycle": True,
    "src/starnix/lib/starnix_rcu": True,
    "src/starnix/lib/starnix_registers": True,
    "src/starnix/lib/starnix_stack": True,
    "src/starnix/lib/starnix_syscalls": True,
    "src/starnix/lib/starnix_task_command": True,
    "src/starnix/lib/starnix_types": True,
    "src/starnix/lib/starnix_uapi": True,
    "src/starnix/lib/syncio": True,
    "src/starnix/lib/third_party/rust_netlink/netlink_packet_core": True,
    "src/starnix/lib/third_party/rust_netlink/netlink_packet_generic": True,
    "src/starnix/lib/third_party/rust_netlink/netlink_packet_route": True,
    "src/starnix/lib/third_party/rust_netlink/netlink_packet_sock_diag": True,
    "src/starnix/lib/third_party/rust_netlink/netlink_packet_utils": True,
    "src/starnix/lib/thread_create_vmars": True,
    "src/starnix/lib/usercopy": True,
    "src/starnix/tools/adb_proxy": True,
    "src/storage/fxfs/crypto": True,
    "src/storage/lib/blob_metadata": True,
    "src/storage/lib/blob_writer": True,
    "src/storage/lib/fscrypt": True,
    "src/storage/lib/fsverity-merkle": True,
    "src/storage/lib/ptr_slice": True,
    "src/storage/lib/storage_xts": True,
    "src/storage/lib/trace": True,
    "src/storage/lib/vfs/rust": True,
    "src/storage/lib/vfs/rust/name": True,
    "src/storage/lib/vmo-fifo": True,
    "src/storage/type-fprint": True,
    "src/sys/lib/bitflags-serde-legacy": True,
    "src/sys/lib/cm_fidl_validator": True,
    "src/sys/lib/cm_graph": True,
    "src/sys/lib/cm_rust": True,
    "src/sys/lib/cm_types": True,
    "src/sys/lib/fuchsia_backoff": True,
    "src/sys/lib/moniker": True,
    "src/sys/lib/notify-batch-watcher": True,
    "src/sys/pkg/lib/eager-package-config": True,
    "src/sys/pkg/lib/far/rust": True,
    "src/sys/pkg/lib/fuchsia-merkle": True,
    "src/sys/pkg/lib/update-package": True,
    "src/sys/pkg/lib/work-queue": True,
    "src/sys/pkg/testing/fake-omaha-client": True,
    "src/ui/lib/input-device-constants": True,
    "third_party/lz4/rust": True,
    "tools/cmc": True,
    "tools/create": True,
    "tools/pretty_serial": True,
}

def _check_unit_test_allowlist(name):
    """Fails if a non-allowlisted package generates implicit unit tests."""
    pkg = native.package_name()
    if pkg not in _ALLOWED_PACKAGES:
        fail(
            ("`with_unit_tests` and `with_host_unit_tests` on `rustc_*()` " +
             "macros are deprecated (https://fxbug.dev/572570591) and not allowed in " +
             "`//{pkg}:{name}`. Declare a standalone `rustc_test()` or `host_rustc_test()` " +
             "target instead.").format(pkg = pkg, name = name),
        )

def generate_unit_tests(
        *,
        name,
        with_host_unit_tests,
        with_unit_tests,
        deps,
        test_deps,
        visibility,
        **kwargs):
    """Generate test targets for rustc_xxx() macros.

    This function is an implementation detail of the rustc_xxx() macros and
    no other uses are allowed.

    Only one of with_host_unit_tests or with_unit_tests can be true.
    See https://fxbug.dev/515405864 regarding these parameters.

    Args:
        name: Name of the parent rustc_xxx() target, the actual test target
            generated by this function will be named "<name>_test".
        with_host_unit_tests: If true, a host test target will be created.
        with_unit_tests: If true, a unit test target will be created.
        deps: Dependencies of the parent rustc_xxx() target.
        test_deps: Extra Rust dependencies for the test target, these will be appended
            to deps to form the full dependency list for the test target. Note that
            Bazel will complain if the same label appears in both deps and test_deps.
        visibility: Visibility of the test target.
        **kwargs: Additional arguments to pass to the test target rule.
          This is expected to contain the Fuchsia-specific changes performed
          by the rustc_xxx() caller.

    """
    _check_unit_test_allowlist(name)

    if with_host_unit_tests and with_unit_tests:
        fail("Cannot specify both with_host_unit_tests and with_unit_tests on {}".format(name))

    # Compute the full set of test dependencies by appending test_deps to deps
    # if available. Note that Bazel will complain if the same label appears in both list
    # and that there is no way to de-duplicate labels, since the values are configurables,
    # meaning they will be either None or opaque select() values at runtime.
    kwargs["deps"] = (deps or []) + (test_deps or [])

    # Test binaries are linked, so they need native libraries required by the
    # Rust standard library. Parent rustc_xxx() macros only add these to the
    # parent target if it is linked too, so they are never added twice.
    kwargs["link_deps"] = with_fuchsia_rust_stdlib_link_deps(kwargs.get("link_deps"))
    kwargs["exec_properties"] = with_fuchsia_rust_link_exec_properties(kwargs.get("exec_properties"))

    # rules_rust's `rust_test()` requires using either `crate` or source attributes
    # (`srcs`, `crate_root`), but not both. Since `crate = ":<name>"` is passed below,
    # remove these source attributes inherited from the parent `rustc_library`/`rustc_binary`
    # target `kwargs` to avoid mutually exclusive attribute errors.
    kwargs.pop("srcs", None)
    kwargs.pop("crate_root", None)

    if with_host_unit_tests:
        # Do not use host_rustc_test() since that one calls rustc_test() which
        # performs Fuchsia-specific changes that have already been performed by
        # the caller. Instead, use define_and_wrap_host_rust_test() with
        # "rust_test_rule = rust_test"
        define_and_wrap_host_rust_test(
            name = "{}_test".format(name),
            rust_test_rule = rust_test,
            crate = ":{}".format(name),
            visibility = visibility,
            **kwargs
        )

    if with_unit_tests:
        kwargs["aspect_hints"] = with_fuchsia_rust_test_aspect_hints(kwargs.get("aspect_hints"))
        rust_test(
            name = "{}_test".format(name),
            crate = ":{}".format(name),
            visibility = visibility,
            **kwargs
        )
