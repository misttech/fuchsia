# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Aspects related to debug symbols for the Fuchsia build system."""

load(
    "@fuchsia_rules_common//assembly:providers.bzl",
    "FuchsiaAssembledPackageInfo",
    "FuchsiaProductConfigInfo",
    "FuchsiaProductInputBundleInfo",
)
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")
load("@rules_cc//cc/common:debug_package_info.bzl", "DebugPackageInfo")
load("@rules_fuchsia//fuchsia/private:providers.bzl", "FuchsiaUnstrippedBinariesInfo", "FuchsiaUnstrippedBinaryInfo")
load(
    "@rules_fuchsia//fuchsia/private/assembly:providers.bzl",
    "FuchsiaBoardConfigInfo",
    "FuchsiaBoardInputBundleInfo",
    "FuchsiaBoardInputBundleSetInfo",
    "FuchsiaProductAssemblyInfo",
)
load("@rules_rust//rust:rust_common.bzl", "CrateInfo")
load("//build/bazel/aspects:utils.bzl", "get_target_deps_from_attributes")
load(
    "//build/bazel/rules:current_platform_info.bzl",
    "CURRENT_PLATFORM_INFO_ATTRS",
    "get_current_platform_info",
)

DebugSymbolManifestEntryInfo = provider(
    doc = "A provider matching a single //:debug_symbols entry.",
    fields = {
        # LINT.IfChange(debug_symbols)
        "debug": "A File value pointing to a debug binary.",
        "stripped": "None or a File value pointing to the stripped version of the binary if available.",
        "os": "OS name for debug binary, using Fuchsia conventions.",
        "cpu": "CPU name for debug binary, using Fuchsia conventions.",
        "label": "Bazel Label of target generating this binary. Does not include build configuration info.",
        # LINT.ThenChange(//BUILD.gn:debug_symbols)
    },
)

PrebuiltPackageManifestEntryInfo = provider(
    doc = "A provider describing a prebuilt Fuchsia package whose ELF blobs may have debug symbols in //prebuilt/.build-id.",
    fields = {
        "package_manifest": "A File value pointing to the package_manifest.json file.",
        "files": "A depset of File values (including unpacked blob directories/files) required to read the package blobs.",
        "os": "OS name for debug binaries, using Fuchsia conventions.",
        "cpu": "CPU name for debug binaries, using Fuchsia conventions.",
        "label": "Bazel Label of the prebuilt package target.",
    },
)

PrebuiltContainerManifestEntryInfo = provider(
    doc = "A provider describing an assembly container directory whose prebuilt ELF blobs may have debug symbols in //prebuilt/.build-id.",
    fields = {
        "container_dir": "Path to the container directory.",
        "files": "A depset of File values required to read the container directory and blobs.",
        "os": "OS name for debug binaries, using Fuchsia conventions.",
        "cpu": "CPU name for debug binaries, using Fuchsia conventions.",
        "label": "Bazel Label of the target.",
    },
)

DebugSymbolManifestInfo = provider(
    doc = """A provider for a set of DebugSymbolManifestEntryInfo items and related build files.""",
    fields = {
        "entries": "A depset of DebugSymbolManifestEntryInfo",
        "dest_paths": "A depset of (debug, dest_path) pairs which provide a way to map debug binaries to " +
                      "their Fuchsia destination path, if available.",
        "prebuilt_packages": "A depset of PrebuiltPackageManifestEntryInfo",
        "prebuilt_containers": "A depset of PrebuiltContainerManifestEntryInfo",
    },
)

_PREBUILT_PACKAGE_RULE_KINDS = (
    "_unpack_prebuilt_package",
    "_pack_prebuilt_package",
    "prebuilt_package",
    "_fuchsia_package",
    "fuchsia_prebuilt_product_configuration_extract_package",
    "_fuchsia_package_default_task",
    "_fuchsia_package_default_task_for_test",
)

# First, an aspect used to identify debug binaries and propagate them
# up the dependency tree through DebugSymbolManifestInfo values.
def _collect_debug_symbols_manifest_aspect_impl(target, aspect_ctx):
    entries = []
    dest_paths = []
    prebuilt_packages = []
    prebuilt_containers = []

    kind = aspect_ctx.rule.kind
    current_platform = get_current_platform_info(aspect_ctx)

    # On Fuchsia, calling the SDK fuchsia_cc_binary() macro or fx_packaged_binary()
    # will instantiate a wrapper target (_fuchsia_cc() or _fx_packaged_binary())
    # that depends on a native cc_binary() or rust_binary() one.
    # The wrapper exposes a FuchsiaUnstrippedBinaryInfo or FuchsiaUnstrippedBinariesInfo
    # provider, which contains 'dest' paths. The aspect will always visit the
    # wrapper target before the underlying binary target.
    #
    # The same is true for fuchsia_wrap_rust_binary() which creates a
    # _fuchsia_cc() target that depends on a native rust_binary() one and
    # provides the same provider / destination path.
    if FuchsiaUnstrippedBinaryInfo in target:
        # Record the destination path for the debug binary.
        unstripped_info = target[FuchsiaUnstrippedBinaryInfo]
        dest_paths.append((unstripped_info.unstripped_file, unstripped_info.dest))

    if FuchsiaUnstrippedBinariesInfo in target:
        for unstripped_info in target[FuchsiaUnstrippedBinariesInfo].binaries:
            dest_paths.append((unstripped_info.unstripped_file, unstripped_info.dest))

    if FuchsiaAssembledPackageInfo in target:
        # Target wraps a package with config data (e.g. fuchsia_package_with_configs)
        pkg_info = target[FuchsiaAssembledPackageInfo].package
        pkg_files = target[DefaultInfo].files if DefaultInfo in target else depset()
        prebuilt_packages.append(PrebuiltPackageManifestEntryInfo(
            package_manifest = pkg_info.package_manifest,
            files = pkg_files,
            os = "fuchsia",
            cpu = current_platform.cpu,
            label = target.label,
        ))
    elif FuchsiaPackageInfo in target and kind in _PREBUILT_PACKAGE_RULE_KINDS:
        pkg_info = target[FuchsiaPackageInfo]
        pkg_files = pkg_info.files if type(pkg_info.files) == "depset" else depset(pkg_info.files)
        prebuilt_packages.append(PrebuiltPackageManifestEntryInfo(
            package_manifest = pkg_info.package_manifest,
            files = pkg_files,
            os = "fuchsia",
            cpu = current_platform.cpu,
            label = target.label,
        ))

    container_info = None
    if FuchsiaProductConfigInfo in target:
        container_info = target[FuchsiaProductConfigInfo]
    elif FuchsiaBoardConfigInfo in target:
        container_info = target[FuchsiaBoardConfigInfo]
    elif FuchsiaProductInputBundleInfo in target:
        container_info = target[FuchsiaProductInputBundleInfo]
    elif FuchsiaBoardInputBundleInfo in target:
        container_info = target[FuchsiaBoardInputBundleInfo]
    elif FuchsiaBoardInputBundleSetInfo in target:
        container_info = target[FuchsiaBoardInputBundleSetInfo]
    elif FuchsiaProductAssemblyInfo in target:
        assembly_out = target[FuchsiaProductAssemblyInfo].product_assembly_out
        container_dir = assembly_out.path if hasattr(assembly_out, "path") else str(assembly_out)
        container_info = struct(directory = container_dir)

    if container_info != None:
        target_files = target[DefaultInfo].files if DefaultInfo in target else depset()
        prebuilt_containers.append(PrebuiltContainerManifestEntryInfo(
            container_dir = container_info.directory,
            files = target_files,
            os = "fuchsia",
            cpu = current_platform.cpu,
            label = target.label,
        ))

    # TODO: Use something like FuchsiaPlatformBinaryInfo to support
    # non-SDK Fuchsia binaries generated in Bazel.

    if kind in ("cc_binary", "cc_test"):
        # cc_binary() provides a DebugPackageInfo that points to the unstripped
        # and stripped versions of the binary.
        debug_info = target[DebugPackageInfo]
        entries.append(DebugSymbolManifestEntryInfo(
            debug = debug_info.unstripped_file,
            stripped = debug_info.stripped_file,
            os = current_platform.os,
            cpu = current_platform.cpu,
            label = target.label,
        ))

    elif kind == "cc_shared_library":
        # cc_shared_library() does not provide DebugPackageInfo and
        # never strips its default output. Besides, unlike executables,
        # the library is not available as DefaultInfo.files_to_run.executable
        # but as the first item in DefaultInfo.files.to_list().
        shared_lib = target[DefaultInfo].files.to_list()[0]
        entries.append(DebugSymbolManifestEntryInfo(
            debug = shared_lib,
            os = current_platform.os,
            cpu = current_platform.cpu,
            label = target.label,
        ))
    elif kind in ("rust_binary", "rust_test", "rust_shared_library"):
        # Ignore binaries generated by @rules_rust (e.g. cargo/private:copy_file
        # and cargo/cargo_build_script_runnner:runner).
        # Ignore cargo_build_script() targets whose name is _bs_.
        # These are implementation details of the Rust build that are never distributed
        # or used during debugging or symbolization.
        if not (target.label.repo_name.startswith("rules_rust") or target.label.name == "_bs_"):
            # For Rust binaries and shared libraries, CrateInfo.output will point to the
            # output, which should be unstripped in the Fuchsia build (see README.md).
            crate_info = target[CrateInfo]
            rust_binary = crate_info.output
            entries.append(DebugSymbolManifestEntryInfo(
                debug = rust_binary,
                os = current_platform.os,
                cpu = current_platform.cpu,
                label = target.label,
            ))
    elif kind == "go_binary":
        # TODO(https://fxbug.dev/442509663): Use GNU BuildID value for Go binaries.
        # Alas, go binaries do not embed a GNU build-id note in their output ELF files.
        # Normally, this is done by passing `-ldflags -B gobuildid` to the `go build`
        # commands, but there is currently no way to customize the @rules_go toolchain
        # that wraps //prebuilt/third_party/go/{host_tag}/.
        pass
    else:
        # TODO(digit): Other binary types, e.g. prebuilt host toolchain binaries. For now
        # these are always provided by the GN graph, so there is no need to collect
        # anything from Bazel.

        # Do not fail, because any other type of target visited during the collection
        # will end up here.
        pass

    dep_infos = [
        dep[DebugSymbolManifestInfo]
        for dep in get_target_deps_from_attributes(aspect_ctx.rule.attr)
        if DebugSymbolManifestInfo in dep
    ]

    return [DebugSymbolManifestInfo(
        entries = depset(entries, transitive = [info.entries for info in dep_infos]),
        dest_paths = depset(dest_paths, transitive = [info.dest_paths for info in dep_infos]),
        prebuilt_packages = depset(prebuilt_packages, transitive = [info.prebuilt_packages for info in dep_infos]),
        prebuilt_containers = depset(prebuilt_containers, transitive = [info.prebuilt_containers for info in dep_infos]),
    )]

collect_debug_symbols_manifest_aspect = aspect(
    doc = """Collect DebugSymbolManifestInfo values across a DAG of dependencies.""",
    implementation = _collect_debug_symbols_manifest_aspect_impl,
    attr_aspects = ["*"],
    provides = [DebugSymbolManifestInfo],
    attrs = CURRENT_PLATFORM_INFO_ATTRS,
)

# Second, a non-propagating aspect that requires the first one and will use
# the result to generate a debug symbols manifest. This also exports the
# debug_symbol_files output group to force all debug binaries to be
# materialized in the bazel output_base (even when remote caching is enabled).
def _generate_manifest_impl(target, actx):
    if DebugSymbolManifestInfo not in target:
        fail("No DebugSymbolManifestInfo in %s" % target.label)

    manifest_info = target[DebugSymbolManifestInfo]
    output = actx.actions.declare_file("%s.debug_symbols.json" % target.label.name)

    files = [output]
    manifest = []

    # Convert manifest_info.entries to a list of dictionaries following
    # the //:debug_symbols schema, while adding a `dest_path` key if
    # one was recorded in manifest_info.dest_paths for it.

    debug_to_dest_path = {
        debug: dest_path
        for debug, dest_path in manifest_info.dest_paths.to_list()
    }

    for entry in manifest_info.entries.to_list():
        # Ignore targets from @bazel_tools as this confuses our bazel_consistency
        # builders. See https://fxbug.dev/445915240
        if entry.label.repo_name == "bazel_tools":
            continue

        debug = entry.debug
        symbol_entry = {
            "debug": str(debug.path),
            "os": entry.os,
            "cpu": entry.cpu,
            "label": str(entry.label),
        }
        files.append(debug)
        if hasattr(entry, "stripped"):
            stripped = entry.stripped
            if stripped:
                files.append(stripped)
                symbol_entry["stripped"] = str(stripped.path)

        dest_path = debug_to_dest_path.get(debug)
        if dest_path:
            symbol_entry["dest_path"] = dest_path
        manifest.append(symbol_entry)

    extra_files = []
    for prebuilt_entry in manifest_info.prebuilt_packages.to_list():
        if prebuilt_entry.label.repo_name == "bazel_tools":
            continue
        files.append(prebuilt_entry.package_manifest)
        extra_files.append(prebuilt_entry.files)
        manifest.append({
            "package_manifest": str(prebuilt_entry.package_manifest.path),
            "os": prebuilt_entry.os,
            "cpu": prebuilt_entry.cpu,
            "label": str(prebuilt_entry.label),
        })

    for container_entry in manifest_info.prebuilt_containers.to_list():
        if container_entry.label.repo_name == "bazel_tools":
            continue
        extra_files.append(container_entry.files)
        manifest.append({
            "container_dir": str(container_entry.container_dir),
            "os": container_entry.os,
            "cpu": container_entry.cpu,
            "label": str(container_entry.label),
        })

    actx.actions.write(output, json.encode_indent(manifest, indent = "  "))

    return [
        OutputGroupInfo(
            debug_symbol_files = depset(files, transitive = extra_files),
            debug_symbol_manifest = depset([output]),
        ),
    ]

generate_manifest = aspect(
    doc = """Generate a debug symbols manifest and materialize debug binaries.""",
    implementation = _generate_manifest_impl,
    # This aspect does not traverse, so no attr_aspects definition here.
    # Ensure that collect_debug_symbols_manifest_aspect is run first.
    requires = [collect_debug_symbols_manifest_aspect],
    # Ensure that the result of collect_debug_symbols_manifest_aspect is available.
    required_aspect_providers = [DebugSymbolManifestInfo],
    provides = [OutputGroupInfo],
)
