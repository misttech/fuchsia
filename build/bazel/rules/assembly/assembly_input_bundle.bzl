# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")
load(
    "@fuchsia_rules_common//assembly:product_configuration.bzl",
    "BUILD_TYPES",
    "FEATURE_SET_LEVELS",
)
load("@fuchsia_rules_common//packages:providers.bzl", "FuchsiaPackageInfo")

def _assembly_input_bundle_impl(ctx):
    out_dir = ctx.actions.declare_directory(ctx.label.name)

    args = ctx.actions.args()
    args.add("create")
    args.add("--outdir", out_dir.path)

    if ctx.attr.experimental:
        args.add("--experimental")

    # Validate and add the allowed_in, scrutiny_required, and auto_include_in values.
    for value in ctx.attr.allowed_in:
        _validate_allowed_in_value("allowed_in", value)
        args.add("--allowed-in", value)

    for value in ctx.attr.scrutiny_required:
        _validate_allowed_in_value("scrutiny_required", value)
        args.add("--scrutiny-required", value)

    for value in ctx.attr.auto_include_in:
        _validate_allowed_in_value("auto_include_in", value)
        args.add("--auto-include-in", value)

    inputs = []

    # Handle package lists
    package_sets = {
        "base_packages": "--base-pkg-list",
        "cache_packages": "--cache-pkg-list",
        "flexible_packages": "--flexible-pkg-list",
        "system_packages": "--system-pkg-list",
        "bootfs_packages": "--bootfs-pkg-list",
        "bootfs_or_base_packages": "--bootfs-or-base-pkg-list",
        "on_demand_packages": "--on-demand-pkg-list",
        "anchored_automatic_packages": "--anchored-automatic-pkg-list",
        "anchored_on_demand_packages": "--anchored-on-demand-pkg-list",
    }

    for attr_name, arg_name in package_sets.items():
        deps = getattr(ctx.attr, attr_name)
        if deps:
            manifests = [dep[FuchsiaPackageInfo].package_manifest.path for dep in deps]
            list_file = ctx.actions.declare_file(ctx.label.name + "_" + attr_name + ".json")
            ctx.actions.write(
                output = list_file,
                content = json.encode(manifests),
            )
            args.add(arg_name, list_file)
            inputs.append(list_file)

            # Add all inputs from the packages
            for dep in deps:
                inputs.extend(dep[FuchsiaPackageInfo].files)

    if ctx.attr.kernel_cmdline:
        cmdline_file = ctx.actions.declare_file(ctx.label.name + "_kernel_cmdline.json")
        ctx.actions.write(
            output = cmdline_file,
            content = json.encode(ctx.attr.kernel_cmdline),
        )
        args.add("--kernel-cmdline", cmdline_file)
        inputs.append(cmdline_file)

    if ctx.attr.shell_commands:
        parsed_shell_commands = json.decode(ctx.attr.shell_commands)
        for shell_cmd in parsed_shell_commands:
            if "package" not in shell_cmd:
                fail("shell_command entries must specify a package name: %s" % shell_cmd)
            if "components" not in shell_cmd:
                fail("shell_command components must be a list of strings pointing to binaries that are components in the package that make up the package: %s" % shell_cmd)
            for key in shell_cmd.keys():
                if key not in ["package", "components", "bootfs_package"]:
                    fail("unknown field in shell_command entry:  %s" % key)

        shell_commands_file = ctx.actions.declare_file(ctx.label.name + "shell_commands.json")
        ctx.actions.write(
            output = shell_commands_file,
            content = json.encode_indent(parsed_shell_commands),
        )
        args.add("--shell-cmds-list", shell_commands_file)
        inputs.append(shell_commands_file)

    if ctx.files.memory_buckets:
        for memory_bucket_file in ctx.files.memory_buckets:
            args.add("--memory-buckets", memory_bucket_file)
            inputs.append(memory_bucket_file)

    ctx.actions.run(
        inputs = inputs,
        outputs = [out_dir],
        executable = ctx.executable._tool,
        arguments = [args],
        mnemonic = "AssemblyInputBundle",
        progress_message = "Creating Assembly Input Bundle %s" % ctx.label.name,
        **LOCAL_ONLY_ACTION_KWARGS
    )

    return [
        DefaultInfo(files = depset([out_dir])),
    ]

_assembly_input_bundle = rule(
    implementation = _assembly_input_bundle_impl,
    attrs = {
        "allowed_in": attr.string_list(),
        "scrutiny_required": attr.string_list(),
        "auto_include_in": attr.string_list(),
        "experimental": attr.bool(),
        "base_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "cache_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "flexible_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "system_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "bootfs_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "bootfs_or_base_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "on_demand_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "anchored_automatic_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "anchored_on_demand_packages": attr.label_list(providers = [FuchsiaPackageInfo]),
        "kernel_cmdline": attr.string_list(),
        "shell_commands": attr.string(),
        "memory_buckets": attr.label_list(allow_files = True),
        "_tool": attr.label(
            default = "//build/assembly/scripts:assembly_input_bundle_tool",
            executable = True,
            cfg = "exec",
        ),
    },
)

def assembly_input_bundle(
        name,
        allowed_in = [],
        scrutiny_required = [],
        auto_include_in = [],
        experimental = False,
        base_packages = [],
        cache_packages = [],
        flexible_packages = [],
        system_packages = [],
        bootfs_packages = [],
        bootfs_or_base_packages = [],
        on_demand_packages = [],
        anchored_automatic_packages = [],
        anchored_on_demand_packages = [],
        kernel_cmdline = [],
        shell_commands = [],
        memory_buckets = [],
        **kwargs):
    """Creates an Assembly Input Bundle.

    Args:
        name: The name of the target.

        experimental: [boolean, default False]
            Whether this AIB is experimental and should be excluded from the scrutiny goldens.
            Experimental AIBs must be available in userdebug and never in user. Experimental AIBs
            are allowed in any feature set level.

            The typical way to mark an AIB as available in userdebug but not user is to
            set `experimental = true` and `allowed_in = ["userdebug", "eng"]`.

        allowed_in: [list of strings] Which feature set + build type combinations this AIB is
           allowed to be included in. Assembly asserts during product assembly that
           the AIB is not included in any feature set + build type combinations
           that are not in this list.

           Options:
           - "everything"
           - "standard", "utility", "bootstrap", "embeddable"
           - "user", "userdebug", "eng"
           - "standard::user", "utility::eng", etc.

        scrutiny_required [list of strings]
            Which feature set + build type combinations to expect the contents of this AIB to be
            added to. Adding to this list will force the contents of this AIB to be listed as
            required in scrutiny goldens.  Valid values are the same as for 'allowed_in'.

        auto_include_in: [list of strings]
            Which feature set + build type combinations to automatically include this AIB in.  Valid
            values are the same as for 'allowed_in'

        base_packages: [list of labels] Package targets to include in the base package set.

        cache_packages: [list of labels] Package targets to include in the cache package set.

        flexible_packages: [list of labels] Package targets that assembly may choose to put in base,
            cache, or elsewhere.

        system_packages: [list of labels] Package targets to include in the system package set.

        bootfs_packages: [list of labels] Package targets to include in the bootfs package set.

        bootfs_or_base_packages: [list of labels] Package targets to include in the bootfs_or_base
            package set.

        on_demand_packages: [list of labels] Package targets to include in the on-demand package
            set.

        anchored_automatic_packages: [list of labels] Package targets to include in the anchored
            automatic package set.

        anchored_on_demand_packages: [list of labels] Package targets to include in the anchored
            on-demand package set.

        kernel_cmdline: [list of strings] Kernel cmdline arguments.

        shell_commands: [list of dicts] A list of dicts that describe the shell commands for each
            listed package

            Example:
            shell_commands = [
              {
                package = "//third_party/sbase"
                components = [ "ls" ]
              },
            ]

        memory_buckets: [list of labels] Paths to memory bucket configs that should get merged
            and passed to memory monitor.

        **kwargs: Other arguments to pass to the rule.
    """

    _assembly_input_bundle(
        name = name,
        experimental = experimental,
        allowed_in = allowed_in,
        scrutiny_required = scrutiny_required,
        auto_include_in = auto_include_in,
        base_packages = base_packages,
        cache_packages = cache_packages,
        flexible_packages = flexible_packages,
        system_packages = system_packages,
        bootfs_packages = bootfs_packages,
        bootfs_or_base_packages = bootfs_or_base_packages,
        on_demand_packages = on_demand_packages,
        anchored_automatic_packages = anchored_automatic_packages,
        anchored_on_demand_packages = anchored_on_demand_packages,
        kernel_cmdline = kernel_cmdline,
        shell_commands = json.encode_indent(shell_commands),
        memory_buckets = memory_buckets,
        **kwargs
    )

# Construct the lists of valid build types and feature set levels.  Only do this once, not each
# time we validate a value.
_build_types = json.decode(json.encode(BUILD_TYPES)).values()
_feature_set_levels = json.decode(json.encode(FEATURE_SET_LEVELS)).values()

def _validate_allowed_in_value(list_name, value):
    if value == "everything":
        return

    if value in _build_types:
        return

    if value in _feature_set_levels:
        return

    tokens = value.split("::")
    if len(tokens) == 2:
        if tokens[0] in _feature_set_levels and tokens[1] in _build_types:
            return

    fail("'%s' is an invalid value for '%s'.  The valid values are '" % (value, list_name) + "', '".join(_build_types + _feature_set_levels) + "' or a '<feature_set_level>::<build_type>'.")
