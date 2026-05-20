---
name: update-assembly-bundles
description: Skill for updating assembly_input_bundle Bazel targets based on GN targets.
---

# Updating Assembly Input Bundle Bazel Targets

## Overview

This skill provides guidance on how to update `assembly_input_bundle` (or
`icu_assembly_input_bundle`) targets in `bundles/assembly/BUILD.bazel` to match
their counterparts in `bundles/assembly/BUILD.gn`, following specific
guidelines.

## Guidelines

1.  **Allowed Parameters:** We can currently only migrate a subset of
    parameters—those which have already been added to the Bazel rule. You must
    get this list of allowed parameters from the Bazel rule definition (e.g., in
    `build/bazel/rules/assembly/assembly_input_bundle.bzl`) **EVERY time** you
    use this skill, to account for changes in the rule. The `testonly` parameter
    MUST NOT be migrated.
1.  **Source File Inputs**: When the input to the GN template is a source file,
    That same path should be used, but it needs to be converted to a proper
    Bazel label for a file.  The directory of the file should become the package
    path, e.g. `//src/foo/tools/bar` becomes a Bazel label of
    `//src/foo/tools:bar` and a BUILD.bazel file (with license) added in that
    directory if one currently doesn't exist.  There MUST be an
    `exports_files()` rule in the BUILD.bazel that will export the source files
    in question.
    * Place `exports_files()` near the top of the `BUILD.bazel` file, after any
      `load()` and `package()` functions, but before any other targets.
    * Set the visibility on files that need to be used with `exports_files()` to
      `["//bundles/assembly:__subpackages__"]`.
    * If multiple files are exported from the same `BUILD.bazel` file, and have
      the same visibility, combine them into a single `exports_files()` rule.
    * For files located in a `meta/` subdirectory (e.g.,
      `//path/to/folder/meta/file.cml`), put the `exports_files()` rule in the
      parent directory's `BUILD.bazel` (e.g., `//path/to/folder/BUILD.bazel`),
      NOT in `meta/BUILD.bazel`. The label should be
      `//path/to/folder:meta/file.cml`.
1.  **GN Target Dependencies:** For each GN dependency that is a GN target (not
    a source file!), we need to have a `bazel_inputs` entry for it in the
    `bundles/assembly/bazel_inputs` directory tree.
    *   If the target is `//src/foo:bar`, then the entry should be
        `//bundles/assembly/bazel_inputs/src/foo:bar`.
    *   Packages should be exported to Bazel using the
        `export_fuchsia_package_to_bazel()` template in a `BUILD.gn` file
        located in the `bazel_inputs` entry directory (e.g.,
        `//bundles/assembly/bazel_inputs/src/foo/BUILD.gn`), NOT in the
        original source directory.
    *   Packages should be imported in Bazel using the `prebuilt_package()` rule
        in a corresponding `BUILD.bazel` file.
    *   All target names must match the original name found in the
        `//bundles/assembly/BUILD.gn` file.
    *   For **single-file GN target dependencies** (GN targets that output a
        single file to be consumed by assembly, such as config JSONs or version
        files, rather than full fuchsia packages):
        *   Define a `bazel_input_file()` target in a `BUILD.gn` under
            `bundles/assembly/bazel_inputs/...` as usual, pointing to the
            generator.
        *   Add the `bazel_input_file` target to the corresponding group (e.g.,
            `resources`) in `bundles/assembly/bazel_inputs/BUILD.gn` to ensure
            the workspace generator collects it.
        *   Do **NOT** define a local `BUILD.bazel` package file or
            `filegroup()` target redirection in `bazel_inputs/`.
        *   Instead, reference the generated `@gn_targets` label directly in
            the consumer target (e.g. in `bundles/assembly/BUILD.bazel`). The
            generated label in `@gn_targets` will be in the generator's
            original directory and name, of the form
            `@gn_targets//<generator_dir>:<generator_name>`.
    *   There MUST be a group target in `bundles/assembly/bazel_inputs/BUILD.gn`
        with the same name as the `assembly_input_bundle` or
        `icu_assembly_input_bundle` target, and it must contain references to
        the targets that are exported to Bazel.
1.  **Comments Preservation:** All comments for an assembly input bundle that
    are found in `//bundles/assembly/BUILD.gn` MUST be copied to the
    corresponding target in `BUILD.bazel`, including any comments before
    parameters to the template, and within lists of arguments to the template.
    The order of comments and the dependencies MUST be preserved.
1.  **Target Order:** Targets in `bundles/assembly/BUILD.bazel` should be kept
    in the same order that they are in the `BUILD.gn` file.
1.  **Buildifier Comment:** Each target in `//bundles/assembly/build.bazel` MUST
    carry a `# buildifier: leave-alone` comment directly above the target, so
    that buildifier keeps the ordering of parameters in the order we want them
    in.
1.  **ICU Targets:** Targets that use the `icu_assembly_input_bundle()` template
    should use the corresponding `icu_assembly_input_bundle()` Bazel rule, with
    its additional parameters. For these targets, use a the
    `icu_aib_bazel_inputs()` template in `//bundles/assembly/BUILD.gn` instead
    of a `group()`.
1.  **Orphaned Bazel Targets:** If you find a Bazel target that does not have a
    corresponding GN target in `bundles/assembly/BUILD.gn`, you must flag this
    to the user for instructions on how to proceed.

## Workflow

When requested to update Bazel targets based on GN targets:

1.  **Read Allowed Parameters:** Check the Bazel rule definition for
    `assembly_input_bundle` to know which parameters are supported. You MUST do
    this **every time** you use this skill.
1.  **Locate the GN Target:** Find the target in `bundles/assembly/BUILD.gn`.
1.  **Locate/Create the Bazel Target:** Find or create the corresponding target
    in `bundles/assembly/BUILD.bazel`.
1.  **Update Targets and Dependencies:** Follow the guidelines above to handle
    packages, dependencies, and comments.
1.  **Format files:** Use `fx format-code` to ensure that the files are
    formatted correctly.

## Verification

After updating the targets:
1.  Run `fx build //bundles/assembly/bazel` to ensure the Bazel targets are
    valid and build correctly. GEMINI.md states to always run `fx build` without
    additional arguments, but in this case, we MUST run this specific build
    command.
