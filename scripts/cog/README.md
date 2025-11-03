# Cog Workspace Setup

This directory contains scripts for setting up and managing a
[cog](go/cog)-based workspace for Fuchsia development. Cog (Code on Git) is a
system that allows developers to work with Git-on-Borg repositories using a
virtual filesystem.

The primary script in this directory is `setup-cog-workspace`.

## `setup-cog-workspace`

This script initializes a new cog workspace. It is currently **highly
experimental** and not recommended for general use.

This script has a custom shebang which prevents it from creating bytecode and
creating __pycache__ directories. This is needed because the __pycache__ directories
will not be ignored by the VCS.

### Usage

To use this script, you must first set the
`FUCHSIA_ALLOW_SETUP_COG_WORKSPACE` environment variable to acknowledge its
experimental nature:

```bash
export FUCHSIA_ALLOW_SETUP_COG_WORKSPACE=1
```

Then, you can run the script from within your cog workspace directory. The script
will automatically detect the workspace root based on the current directory,
which must match the pattern `/google/cog/cloud/<user>/<workspace_name>`.

Example:

```bash
cd /google/cog/cloud/my-user/fuchsia-cog
scripts/cog/setup_cog_workspace.py
```
In the near future, we expect to automatically run this script when cog
activates the workspace. In order to prevent this from happening for users who
are not actively working on making the script production ready, we require
the `FUCHSIA_ALLOW_SETUP_COG_WORKSPACE` environment variable to be set.

### Testing

To run the tests for this script, run the following command:

```bash
fx add-host-test //scripts/cog:tests
```

You can then run the tests with:

```bash
fx test
```

### Why is this not in `fx`?

This script must be run before we can use `fx` because it sets up jiri_root which
is used by `fx` to find the Fuchsia source tree. Eventually, this script might be
integrated into `fx` but at this time it must be run separately.

### Symlink Directory Structure

When this script runs it will create a symlink directory structure which the cog
workspace will point to. When cog encounters a symlink that points outside of the
workspace it will ignore it which makes this ideal for working with prebuilts and
build artifacts.

/path/to/mount/<workspace_name>
  - prebuilt/ <-> /cog/path/<workspace>/<repo>/prebuilt
  - out/ (the out directory)
