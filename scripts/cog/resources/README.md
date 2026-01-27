# Resources for Cog Workspaces

This directory contains resources used to configure a Fuchsia "cog" workspace. In a cog environment, these files are typically symlinked into the root of a Fuchsia checkout (`$FUCHSIA_DIR`).

*   **`args.gn`**: Provides a default set of GN build arguments tailored for cog workspaces. When a user runs `fx set`, these arguments configure the build, ensuring a consistent development environment. This file is intended to be symlinked from `local/args.gn` within the CartFS filesystem.

*   **`fx`**: A wrapper script for the main `fx` command. It intercepts specific commands (like `set`, `build`, and `test`) and redirects them to `fx cog`, ensuring they execute correctly within the CartFS environment. All other commands are passed through to the standard `fx` script.
