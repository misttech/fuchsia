# Cog Binaries

This directory contains scripts that mimic standard binaries which may not be available or fully functional within a Cog workspace environment.

## Usage

When `fx` detects that it is running inside a Cog workspace (by looking for a `.citc` directory), it automatically prepends this directory to the `PATH`. This allows these wrapper scripts to intercept calls to the standard binaries and provide Cog-specific behavior or fallbacks.

## Adding New Binaries

**IMPORTANT:** Only add files to this directory if they are intended to mimic a binary that is missing or requires special handling in a Cog workspace.

Any executable placed here will override the system version of that command when running `fx` commands in a Cog workspace. Ensure that your wrapper script correctly handles all necessary arguments or delegates to the real binary when appropriate (e.g., via `FUCHSIA_REAL_GIT` for the `git` wrapper).
