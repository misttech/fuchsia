# Workspace Context Design for `fx ide-query`

This document describes the design for collecting and managing workspace-level metadata in the `ide-query` tool.

## Overview

The `ide-query` tool needs to understand the structure of the Fuchsia checkout it is running in to resolve file paths, find build artifacts, and interact with other build tools (like GN and Ninja). This state is encapsulated in a `WorkspaceContext`.

## The `WorkspaceContext` Structure

The context is a central data structure passed to query handlers:

```go
type FileStatus string

const (
    StatusFound    FileStatus = "found"
    StatusNotFound FileStatus = "not_found"
)

type FileEntry struct {
    // AbsPath is the absolute, canonicalized path to the file.
    AbsPath string `json:"abs_path"`
    // OriginalPath is the path as provided by the user (relative or absolute).
    OriginalPath string `json:"original_path"`
    // Status indicates if the file exists on disk.
    Status FileStatus `json:"status"`
    // IsDirectory is true if the path points to a directory.
    IsDirectory bool `json:"is_directory"`
}

type WorkspaceContext struct {
    // FuchsiaDir is the absolute path to the root of the Fuchsia checkout.
    FuchsiaDir string `json:"fuchsia_dir"`
    // BuildDir is the absolute path to the current build output directory.
    // This may be empty if not provided and .fx-build-dir is missing.
    BuildDir string `json:"build_dir"`
    // Files is a list of file entries provided via the --file flag or --file-list flag.
    Files []FileEntry `json:"files"`
}

type ErrorResponse struct {
    // Error is a human-readable error message.
    Error string `json:"error"`
}
```

## Input Collection

### 1. Fuchsia Root (`FuchsiaDir`)
The root directory must be provided via the `--fuchsia-dir` command-line flag. The Go tool does not
perform auto-discovery and will only trust this flag.

If this flag is not provided, the tool will report an error and exit. This ensures that the tool always has a
consistent view of the workspace regardless of how it is invoked.

The shell wrapper (`//tools/devshell/ide-query`) is responsible for reading the `FUCHSIA_DIR` environment
variable and passing it as the `--fuchsia-dir` flag to the underlying binary.

Once the `--fuchsia-dir` is provided, it is immediately canonicalized (e.g., via `filepath.EvalSymlinks`)
to its physical location. This canonical root is used for all subsequent relative path resolutions.

All flag arguments (like `--build-dir` and `--file-list`) are initially resolved relative to the
command's current working directory (CWD) before being canonicalized.

### 2. Build Directory (`BuildDir`)
The build directory is determined in the following order of precedence:
1.  The `--build-dir` command-line flag.
2.  Reading the file `${FuchsiaDir}/.fx-build-dir`.
    - The tool will read strictly the first line of the file and trim all leading and trailing whitespace (including the line terminator).
    - This line must contain a relative path from the root (e.g., `out/default`).
    - If the line contains an absolute path, the tool will report an error and exit.
    - The `BuildDir` is resolved as `filepath.Join(FuchsiaDir, line)`.

If neither is provided, `BuildDir` will be empty. Queries that require a build directory (e.g., to find generated headers) will report an error during the execution phase.

### 3. File List (`Files`)
Users can specify files to query using either the `--file` flag or the `--file-list` flag. Both flags are repeatable.

- **`--file`**: Specifies a single file path.
- **`--file-list`**: Specifies a path to a file containing a newline-separated list of file paths.

Example: `fx ide-query --file path/to/a.cc --file-list my_files.txt`

If a file path provided to `--file-list` cannot be read, the tool will report an error and exit immediately.

#### Input Normalization (File List):
When reading from a `--file-list`:
- Each line is trimmed of leading and trailing whitespace.
- If a line is empty after trimming, the tool will report an error and exit immediately.
- Lines starting with `#` are treated as comments and ignored.

#### Path Resolution Rules:
- **Relative Paths**: Paths provided via `--file` or inside a `--file-list` are always resolved relative to `FuchsiaDir`.
  - *Example*: `--file scripts/main.go` becomes `${FuchsiaDir}/scripts/main.go`.
- **Absolute Paths**: Accepted as-is, but subject to canonicalization.
- **Paths Outside Fuchsia Root**: Files located outside of `${FuchsiaDir}` are accepted and included in the
  `Files` list with their absolute paths.
- **Missing Files**:
  - If a file provided via `--file` or inside a `--file-list` does not exist on disk, it is included in the `Files` list with a `status` of `"not_found"` and `IsDirectory` set to `false`.
  - For its `AbsPath`, the tool will resolve symlinks for all existing parent directories and then append the missing filename to the canonicalized parent path.
- **Directories**: Paths that resolve to directories are included in the `Files` list with `status` `"found"` and `IsDirectory` set to `true`.
- **Original Path**: The `OriginalPath` field in `FileEntry` must contain the path as it was received from the flag or file list (before any prepending or canonicalization). If an empty string is provided as a path via `--file`, the tool will report an error and exit.
- **Canonicalization**: Existing paths (including `FuchsiaDir` and `BuildDir`) are converted to absolute paths and have symbolic links evaluated (e.g., via `filepath.EvalSymlinks`).
- **Normalization and Deduplication**: Any duplicates are removed after path canonicalization. If multiple inputs resolve to the same canonical path, the tool will keep the `OriginalPath` of the last occurrence (based on the order they appear inside flags or across different flags on the command line) and discard the earlier ones.
- **Empty Inputs**: If no files are provided via `--file` or `--file-list`, the tool will still succeed and return a `WorkspaceContext` with an empty `Files` list. It is not an error to query zero files.


## Command Line Interface

The tool uses the `github.com/spf13/pflag` package to support POSIX-style flags, consistent with other Fuchsia Go tools. This is provided via the GN dependency `//third_party/golibs:github.com/spf13/pflag`.

| Flag | Type | Description |
| :--- | :--- | :--- |
| `--fuchsia-dir` | string | Overrides the Fuchsia root directory. |
| `--build-dir` | string | Overrides the build output directory. |
| `--file` | stringSlice | A path to a file to be queried. Can be repeated. |
| `--file-list` | stringSlice | A path to a file containing a list of files to be queried. Can be repeated. |

## Implementation Details

- **Validation**:
  - The tool verifies that `FuchsiaDir` is provided and is an existing directory.
  - If `BuildDir` is non-empty, the tool verifies it is an existing directory.
  - Validation occurs *after* path canonicalization.
  - The tool fails immediately if any provided `--file-list` cannot be read.

- **Output Format**:
  - On success, the tool will output the `WorkspaceContext` as a single JSON object to `stdout` with 2-space indentation.
  - On error, the tool will output an `ErrorResponse` as a single JSON object to `stdout` with 2-space indentation. The tool will exit with status `1` for all errors.
- **Structure**:
  - Flag parsing and validation will be encapsulated in a function: `func NewWorkspaceContext(args []string) (*WorkspaceContext, error)`.
  - For the initial implementation, this logic will reside in the `main` package.
- **Immutability**: Once initialized, the `WorkspaceContext` is treated as immutable. Query handlers should not modify the context.

## Testing Strategy

- **Unit Tests**: `NewWorkspaceContext` will be tested using Go's `t.TempDir()` to create a temporary filesystem layout, ensuring correct resolution of `.fx-build-dir` and `--file` paths.
- **Integration**: The tool's ability to correctly identify its environment will be verified by running it within a standard Fuchsia checkout.
