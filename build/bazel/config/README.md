This directory contains configuration information that is read from several
scripts ot locations.

- `bazel_top_dir`:

  The `BAZEL_TOPDIR` for the main workspace, relative to the Ninja output
  directory. See `//build/bazel/README.md` for details.

  Format:  A single line of text for the path.
  Used by: `//build/bazel/bazel_workspace.gni`
  Used by: `//tools/devshell/lib/bazel_utils.sh`
  Used by: `//build/bazel/scripts/parse-workspace-event-log.py`
  Used by: `//build/bazel/scripts/workspace_utils.py`
  Used by: `//tools/integration/fint/build.go`

- bazel_args.gni:

  GNI file computing the Bazel command-line arguments to use to match
  the current GN build configuration, with regards to optimization mode,
  sanitizer mode, verbosity and remote builds.

- logging.gni:

  GNI file declaring a GN build variable that impacts Bazel invocations.

- no_downloads_allowed.config:

  A Bazel --experimental_downloader_config file referenced from the
  top-level .bazelrc file to disable all downloads when running repository
  rules.

- BUILD.gn:

  A GN build file that defines a `generated_file()` target dumping
  the current toolchain's bazel build arguments to a JSON file.

- BUILD_CONFIGURATION_VARIABLES.md:

  Documentation explaining how Fuchsia build configuration variables are
  classified and propagated from GN to Bazel.

- `variables/`:

  Bazel `build_setting()` targets (`bool_flag`, `int_flag`, `string_flag`)
  representing toolchain-specific GN build configuration variables.

- `select/`:

  Bazel `config_setting()` predicates matching the build settings in
  `variables/`, used in `select()` statements.

- `derived/`:

  Starlark constants and `config_setting_group()` targets derived from global
  or toolchain-specific build configuration values.

- `toolchain_tags/`:

  Auto-generated `bool_flag()`, `config_setting()`, and `.gni` definitions
  exposing GN `toolchain_variant.tags` to Bazel. See
  `toolchain_tags/README.md` for details.


