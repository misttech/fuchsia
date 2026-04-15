# Build Verification for `fx ide-query`

This document outlines the design for verifying that source files and their generated dependencies are correctly built. This ensures that IDEs have access to fresh artifacts (like FIDL-generated headers) before performing analysis.

## Overview

To provide a consistent and isolated analysis environment, `ide-query` uses a dedicated "shadow" build directory. This prevents IDE-triggered builds from interfering with, locking, or dirtying the user's primary build directory.

## Shadow Build Directory

The tool will use a specific directory for all analysis-related builds:
- **Path**: `out/.ide-analysis` (relative to the Fuchsia root).
- **Rationale**: Since the tool runs in a network-mounted file system (Cog), disk space is not a primary concern. Using a dedicated directory allows for a clean separation of build artifacts.

## Build Workflow

The build verification process follows these steps:

### 1. Synchronization and Setup

Before any build is attempted, the shadow directory must be configured to match the user's current build environment:

1.  **Locate `args.gn`**: Find the `args.gn` file in the user's primary `BuildDir` (as determined by `WORKSPACE_CONTEXT_DESIGN.md`).
2.  **Verify Configuration**: If `args.gn` is missing in the primary build directory (indicating the user has not configured a build with `fx set`), the tool will report an `AnalysisError`.
3.  **Sync `args.gn`**:
    - Compare `${BuildDir}/args.gn` with `out/.ide-analysis/args.gn`.
    - If they differ, or if the shadow directory does not exist:
        - Copy `args.gn` to the shadow directory.
        - Execute `fx --dir out/.ide-analysis gen` to regenerate the Ninja files.

### 2. Identifying Targets

The tool identifies the GN labels for all requested files as described in `BUILD_TARGETS_DESIGN.md`.

### 3. Executing the Build

To accurately report build failures for each file, targets are built individually. The tool iterates through each unique target and executes `fx --dir out/.ide-analysis build <target>`. While this is less parallel than a single multi-target build, it prevents a single failing target from obscuring the status of other targets.

1.  **Collect Targets**: Filter for all unique, known GN labels.
2.  **Run Build**: For each unique target, execute `fx --dir out/.ide-analysis build <target>`.
3.  **Capture Failures**: If the build fails for a target, the tool marks all files associated with that target as failed.

## Error Handling and Reporting

The results are reported back to the IDE via two fields in the `FileEntry` structure.

### `AnalysisError` (string)

This field is used for terminal errors that prevent the tool from progressing to the build phase. If this is set, the `AnalysisResult` is typically omitted.

**Scenarios**:
- Fuchsia root or build directory not provided/found.
- `args.gn` missing in the primary build directory.
- `compile_commands.json` is missing or unreadable.
- Internal tool errors (e.g., failure to execute `fx gen`).

### `AnalysisResult` (object)

This field captures the outcome of the build attempt for a specific file.

```go
type AnalysisStatus string

const (
    StatusOk           AnalysisStatus = "OK"
    StatusNotFound     AnalysisStatus = "NOT_FOUND"
    StatusBuildFailed  AnalysisStatus = "BUILD_FAILED"
    StatusUnknown      AnalysisStatus = "UNKNOWN"
)

type AnalysisResult struct {
    Status  AnalysisStatus `json:"status"`
    Message string         `json:"message,omitempty"`
}
```

**Status Mapping**:
- **`OK`**: The building of the target(s) associated with the file succeeded.
- **`NOT_FOUND`**: The file itself does not exist on disk.
- **`BUILD_FAILED`**: Ninja reported an error specifically while building the targets for this file.
    - **Message**: Instead of the full Ninja log, the message will succinctly state: `"File failed to build."`
- **`UNKNOWN`**: The file was not found in `compile_commands.json` and heuristics failed to identify a target.

## Consistency and Performance

- **Deduplication**: Even if 10 files belong to the same target, they are only built once.
- **Incrementalism**: Ninja naturally handles incremental builds in the shadow directory, ensuring that subsequent queries are fast.
- **Isolation**: By passing `--dir out/.ide-analysis` to `fx` commands, we ensure that no side effects impact the user's primary development workflow.
