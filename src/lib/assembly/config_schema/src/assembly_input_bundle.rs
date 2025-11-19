// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::{DriverDetails, PackageDetails, PackageName};
use assembly_constants::{CompiledPackageDestination, FileEntry};
use assembly_container::WalkPaths;
use assembly_file_relative_path::{FileRelativePathBuf, SupportsFileRelativePaths};
use assembly_package_utils::PackageInternalPathBuf;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A bundle of inputs to be used in the assembly of a product.
#[derive(
    Debug, Default, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssemblyInputBundle {
    /// The parameters that specify which kernel to put into the ZBI.
    pub kernel: Option<PartialKernelConfig>,

    /// The qemu kernel to use when starting the emulator.
    pub qemu_kernel: Option<Utf8PathBuf>,

    /// The list of additional boot args to add.
    pub boot_args: Vec<String>,

    /// The packages that are in the bootfs package list, which are
    /// added to the BOOTFS in the ZBI.
    pub bootfs_packages: Vec<Utf8PathBuf>,

    /// The set of files to be placed in BOOTFS in the ZBI.
    pub bootfs_files: Vec<FileEntry<String>>,

    /// Package entries that internally specify their package set, instead of being grouped
    /// separately.
    pub packages: Vec<PackageDetails>,

    /// Entries for the `config_data` package.
    pub config_data: BTreeMap<String, Vec<FileEntry<String>>>,

    /// The blobs index of the AIB.  This currently isn't used by product
    /// assembly, as the package manifests contain the same information.
    pub blobs: Vec<Utf8PathBuf>,

    /// Configuration of base driver packages. Driver packages should not be
    /// listed in the base package list and will be included automatically.
    pub base_drivers: Vec<DriverDetails>,

    /// Configuration of boot driver packages. Driver packages should not be
    /// listed in the bootfs package list and will be included automatically.
    pub boot_drivers: Vec<DriverDetails>,

    /// Map of the names of packages that contain shell commands to the list of
    /// commands within each.
    pub bootfs_shell_commands: ShellCommands,

    /// Map of the names of packages that contain shell commands to the list of
    /// commands within each.
    pub shell_commands: ShellCommands,

    /// Packages to create dynamically as part of the Assembly process.
    #[file_relative_paths]
    #[walk_paths]
    pub packages_to_compile: Vec<CompiledPackageDefinition>,

    /// A package that includes files to include in bootfs.
    pub bootfs_files_package: Option<Utf8PathBuf>,

    /// A list of memory buckets to pass to memory monitor.
    #[file_relative_paths]
    #[walk_paths]
    pub memory_buckets: Vec<FileRelativePathBuf>,
}

/// The information required to specify a kernel and its arguments, all optional
/// to allow for the partial specification
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PartialKernelConfig {
    /// The path to the prebuilt kernel.
    pub path: Option<Utf8PathBuf>,

    /// The list of command line arguments to pass to the kernel on startup.
    #[serde(default)]
    pub args: Vec<String>,
}

/// A typename to represent a package that contains shell command binaries,
/// and the paths to those binaries
pub type ShellCommands = BTreeMap<PackageName, BTreeSet<PackageInternalPathBuf>>;

/// Contents of a compiled package. The contents provided by all
/// selected AIBs are merged by `name` into a single package
/// at assembly time.
#[derive(Debug, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths)]
#[serde(deny_unknown_fields)]
pub struct CompiledPackageDefinition {
    /// Name of the package to compile.
    pub name: CompiledPackageDestination,

    /// Components to compile and add to the package.
    #[file_relative_paths]
    #[walk_paths]
    #[serde(default)]
    pub components: Vec<CompiledComponentDefinition>,

    /// Non-component files to add to the package.
    #[serde(default)]
    pub contents: Vec<FileEntry<String>>,

    /// CML files included by the component cml.
    #[serde(default)]
    pub includes: Vec<Utf8PathBuf>,

    /// Whether the contents of this package should go into bootfs.
    /// Gated by allowlist -- please use this as a base package if possible.
    #[serde(default)]
    pub bootfs_package: bool,
}

/// Contents of a compiled component. The contents provided by all
/// selected AIBs are merged by `name` into a single package
/// at assembly time.
#[derive(Debug, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths)]
#[serde(deny_unknown_fields)]
pub struct CompiledComponentDefinition {
    /// The name of the component to compile.
    pub component_name: String,

    /// CML file shards to include in the compiled component manifest.
    #[file_relative_paths]
    #[walk_paths]
    pub shards: Vec<FileRelativePathBuf>,
}

impl AssemblyInputBundle {
    /// Are all containers in this AIB empty.
    pub fn is_empty(&self) -> bool {
        // destructure to ensure that when new fields are added, they require this function
        // to be touched.
        let Self {
            kernel,
            qemu_kernel,
            boot_args,
            bootfs_packages,
            bootfs_files,
            packages,
            config_data,
            blobs,
            base_drivers,
            boot_drivers,
            bootfs_shell_commands,
            shell_commands,
            packages_to_compile,
            bootfs_files_package,
            memory_buckets,
        } = &self;

        qemu_kernel.is_none()
            && boot_args.is_empty()
            && bootfs_packages.is_empty()
            && bootfs_files.is_empty()
            && packages.is_empty()
            && config_data.is_empty()
            && blobs.is_empty()
            && base_drivers.is_empty()
            && boot_drivers.is_empty()
            && bootfs_shell_commands.is_empty()
            && shell_commands.is_empty()
            && packages_to_compile.is_empty()
            && bootfs_files_package.is_none()
            && memory_buckets.is_empty()
            && (match &kernel {
                Some(kernel) => kernel.args.is_empty() && kernel.path.is_none(),
                None => true,
            })
    }
}
