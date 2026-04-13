# Bootstrapping Design for `fx ide-query`

This document details the self-bootstrapping mechanism used by the `fx ide-query` tool.

## Overview

The `ide-query` tool is implemented in Go but is triggered via a shell script wrapper at `//tools/devshell/ide-query`. This wrapper handles the compilation of the Go source on-demand, caching the resulting binary for subsequent executions.

This design mirrors the approach taken by other core `fx` tools like `fx set`.

## Initial Goal

The first step is to implement a minimal "Hello World" version of the Go tool. This ensures the bootstrapping logic (compilation, caching, and execution) is functional before any real query logic is added.

## Rationale

- **Performance**: Compiled Go binaries are faster than `go run` for complex tools.
- **Decoupling**: The tool can be built and run even if the main GN/Ninja build is in a broken or unconfigured state.
- **Workflow Integration**: It allows rapid iteration via a `--dev` flag to force recompilation without waiting for a full `fx build`.

## Bootstrapping Steps

The shell script wrapper performs the following steps:

1.  **Environment Initialization**: Sources `//tools/devshell/lib/vars.sh` to define `${FUCHSIA_DIR}`, `${FX_CACHE_DIR}`, and find the prebuilt Go toolchain.
2.  **Lazy Build Check**:
    - Compares the current git revision (`git rev-parse HEAD`) with a value stored in `${FX_CACHE_DIR}/ide-query.revision`.
    - If the revision differs, the binary is missing, or the `--dev` flag is set, a rebuild is triggered.
3.  **Source Preparation**:
    - Creates a temporary build directory.
    - Symlinks the necessary Go module metadata (`go.mod`, `go.sum`, `vendor`) from `//third_party/golibs`.
    - Symlinks required Fuchsia internal libraries (e.g., `//tools`).
4.  **Compilation**:
    - Runs `go build` using the prebuilt Go binary.
    - Disables network access and CGO (`GOPROXY=off`, `CGO_ENABLED=0`).
    - Outputs the binary to `${FX_CACHE_DIR}/ide-query.bin`.
5.  **Execution**: Executes the binary with any arguments passed to the script.

## Assumptions

- **Prebuilt Go**: Assumes the Fuchsia prebuilt Go toolchain is available in `${PREBUILT_GO_DIR}`.
- **Vendored Dependencies**: Relies on third-party dependencies being vendored in `//third_party/golibs`.
- **Cache Directory**: Assumes `${FX_CACHE_DIR}` exists and is writable.
- **Host Platform**: Currently assumes the host platform is Linux (mirroring Fuchsia development requirements).

## Testing Plan

To maintain high quality and correctness, the following testing strategies will be used:

### 1. Basic Unit Tests
All logic inside `//scripts/cog/ide_query` should be covered by standard Go unit tests (`*_test.go`).
-   **Execution**: Tests will be run using `go test ./scripts/cog/ide_query/...`.
-   **Mocking**: Use mock file systems or interfaces for testing code that interacts with the Fuchsia source tree.

### 2. GN-Based Tests
A `BUILD.gn` will be added to the directory to allow the tool to be tested by Fuchsia's standard infrastructure.
-   **Target**: A `go_test` target will be defined to run unit tests as part of `fx test`.
-   **Verification**: This ensures the tool's dependencies and logic are compatible with the broader Fuchsia build graph.

### 3. Bootstrap Integration Tests
The shell script wrapper's ability to compile and run the tool will be verified by:
-   Running `fx ide-query --dev` and verifying it prints "Hello World".
-   Modifying the source and ensuring `fx ide-query --dev` reflects the change.
