# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""All Fuchsia Providers."""

load(
    "@fuchsia_rules_common//debug_symbols:providers.bzl",
    _FuchsiaCollectedUnstrippedBinariesInfo = "FuchsiaCollectedUnstrippedBinariesInfo",
    _FuchsiaUnstrippedBinaryInfo = "FuchsiaUnstrippedBinaryInfo",
    _make_fuchsia_unstripped_binary_info = "make_fuchsia_unstripped_binary_info",
)
load(
    "@fuchsia_rules_common//packages:providers.bzl",
    _FuchsiaPackageResourcesInfo = "FuchsiaPackageResourcesInfo",
)

FuchsiaPackageResourcesInfo = _FuchsiaPackageResourcesInfo

FuchsiaAssembledArtifactInfo = provider(
    "Artifacts that can be included into a product. It consists of the artifact and the corresponding config data.",
    fields = {
        "artifact": "The base artifact",
        "configs": "A list of configs that is attached to artifacts",
    },
)

FuchsiaConfigDataInfo = provider(
    "The config data which is used in assembly.",
    fields = {
        "source": "Config file on host",
        "destination": "A String indicating the path to find the file in the package on the target",
    },
)

FuchsiaComponentInfo = provider(
    "Contains information about a fuchsia component",
    fields = {
        "name": "name of the component",
        "manifest": "A file representing the compiled component manifest file",
        "resources": "any additional resources the component needs",
        "moniker": "The moniker to run the non-driver, non-test, non-session component in",
        "is_driver": "True if this is a driver",
        "is_test": "True if this is a test component",
        "run_tag": "A tag used to identify the component when put in a package to be later used by the run command",
    },
)

FuchsiaDeviceTreeSegmentInfo = provider(
    "Contains information about a fuchsia devicetree fragment",
    fields = {
        "includes": "A depset of include directory paths used when compiling the devicetree binary.",
        "files": "A depset of transitive dependencies needed for future devicetree compile.",
    },
)

FuchsiaPackagedComponentInfo = provider(
    "Contains information about a fuchsia component that has been included in a package",
    fields = {
        "component_info": "The original FuchsiaComponentInfo provider if this is built locally. Otherwise it will be empty",
        "dest": "The install location for this component in a package (meta/foo.cm)",
    },
)

FuchsiaCollectedUnstrippedBinariesInfo = _FuchsiaCollectedUnstrippedBinariesInfo
FuchsiaUnstrippedBinaryInfo = _FuchsiaUnstrippedBinaryInfo
make_fuchsia_unstripped_binary_info = _make_fuchsia_unstripped_binary_info

FuchsiaComponentManifestInfo = provider(
    "Contains information about a Fuchsia component manifest",
    fields = {
        "compiled_manifest": "A File pointing to the compiled manifest",
        "component_name": "The name of the component",
        "config_package_path": "The path to the generated cvf file",
    },
)

FuchsiaComponentManifestShardInfo = provider(
    "Contains information about a Fuchsia component manifest shard",
    fields = {
        "file": "The file of the shard",
        "base_path": "Base path of the shard, used in includepath argument of cmc compile",
    },
)

FuchsiaComponentManifestShardCollectionInfo = provider(
    "Contains information about a collection of shards to add as dependencies for each cmc invocation",
    fields = {
        "shards": "A list of shards's as targets in the collection",
    },
)

FuchsiaFidlLibraryInfo = provider(
    "Contains information about a FIDL library",
    fields = {
        "info": "List of structs(name, files) representing the library's dependencies",
        "name": "Name of the FIDL library",
        "ir": "Path to the JSON file with the library's intermediate representation",
    },
)

FuchsiaBindLibraryInfo = provider(
    "Contains information about a Bind Library.",
    fields = {
        "name": "Name of the Bind Library.",
        "transitive_sources": "A depset containing transitive sources of the Bind Library.",
    },
)

FuchsiaCoreImageInfo = provider(
    "Private provider containing platform artifacts",
    fields = {
        "esp_blk": "EFI system partition image.",
        "kernel_zbi": "Zircon image.",
        "vbmetar": "vbmeta for zirconr boot image.",
        "zirconr": "zedboot boot image.",
    },
)

FuchsiaCollectedPackageResourcesInfo = provider(
    """A provider which represents a package resource and all of its transitive resources.

    This provider should not be directly created. If a rule wants to expose a set
    of resources it should create a FuchsiaPackageResourcesInfo provider instead.
    """,
    fields = {
        "collected_resources": "A depset containing the direct and transitive resources",
    },
)

FuchsiaPackageGroupInfo = provider(
    doc = "The raw files that make up a set of fuchsia packages.",
    fields = {
        "packages": "a list of all packages that make up this package group",
    },
)

FuchsiaProductImageInfo = provider(
    doc = "Info needed to pave a Fuchsia image",
    fields = {
        "esp_blk": "EFI system partition image.",
        "blob_blk": "BlobFS partition image.",
        "data_blk": "MinFS partition image.",
        "images_json": "images.json file",
        "blobs_json": "blobs.json file",
        "kernel_zbi": "Zircon image.",
        "vbmetaa": "vbmeta for zircona boot image.",
        "vbmetar": "vbmeta for zirconr boot image.",
        "zircona": "main boot image.",
        "zirconr": "zedboot boot image.",
        "flash_json": "flash.json file.",
    },
)

FuchsiaAssemblyConfigInfo = provider(
    doc = "Private provider that includes a single JSON configuration file.",
    fields = {
        "config": "JSON configuration file",
    },
)

FuchsiaProductBundleConfigInfo = provider(
    doc = "Config data used for pbm creation",
    fields = {
        "packages": "Path to packages directory.",
        "images_json": "Path to images.json file.",
        "zbi": "Path to ZBI file.",
        "fvm": "Path to FVM file.",
    },
)

FuchsiaProvidersInfo = provider(
    doc = """
    Keeps track of what providers exist on a given target.
    Construct with utils.bzl > track_providers.
    Used by utils.bzl > alias.
    """,
    fields = {
        "providers": "A list of providers values to carry forward.",
    },
)

FuchsiaVersionInfo = provider(
    doc = "version information passed in that overwrite sdk version",
    fields = {
        "version": "The version string.",
    },
)

AccessTokenInfo = provider(
    doc = "Access token used to upload to MOS repository",
    fields = {
        "token": "The token string.",
    },
)

FuchsiaPackageRepoInfo = provider(
    doc = "A provider which provides the contents of a fuchsia package repo",
    fields = {
        "packages": "The paths to the package_manifest.json files",
        "repo_dir": "The directory of the package repo.",
        "blobs": "The blobs needed by packages in this package repo.",
    },
)

FuchsiaRunnableInfo = provider(
    doc = "A provider which provides the script and runfiles to run a Fuchsia component or test package.",
    fields = {
        "executable": "A file corresponding to the runnable script.",
        "runfiles": "A list of runfiles that the runnable script depends on.",
        "is_test": "Whether this runnable is a test.",
    },
)

FuchsiaDriverToolInfo = provider(
    doc = "A provider which contains information about a driver tool.",
    fields = {
        "tool_path": "A tool's binary package-relative path (e.g. 'bin/tool').",
    },
)

FuchsiaProductBundleInfo = provider(
    doc = "Product Bundle Info.",
    fields = {
        "product_bundle": "The full URL for the product bundle. Can be empty.",
        "is_remote": "Whether the product bundle is a local path or a remote url.",
        "product_bundle_name": "The name of the product to be used if product_bundle is empty.",
        "product_version": "The version of the product to use.",
        "product_version_file": "A path to a file containing the version of the product to use.",
        "repository": "The name of the repository to host extra packages in the product bundle.",
        "build_id_dirs": "Directories containing the debug symbols",
    },
)

FuchsiaStructuredConfigInfo = provider(
    doc = "A provider which contains the generated cvf for structured configs.",
    fields = {
        "cvf_source": "The generated cvf",
        "cvf_dest": "The location where the cvf is stored within a fuchsia package archive.",
    },
)
