# Design for Build Target Identification in `fx ide-query`

This document outlines the design for identifying the build targets (GN labels) that correspond to a set of C++ source files. This is a critical step for ensuring that IDEs have access to all necessary generated artifacts (like headers) before performing analysis.

## Overview

The `ide-query` tool will provide a mechanism to translate a list of C++ source files into a list of GN labels that need to be built. This is achieved by mapping source files to their compilation outputs and then mapping those outputs to GN labels.

## Workflow

The target identification process follows these steps:

1.  **Locate Compilation Database**: Find the `compile_commands.json` file in the `BuildDir`.
2.  **Parse Compilation Database**: Load entries for the requested C++ files.
3.  **Infer Output Paths**: For each entry, analyze the compilation arguments to determine the output path (e.g., the `.o` file).
4.  **Translate to GN Labels**: Use the Fuchsia Build API to map the Ninja output paths to GN labels.
5.  **Deduplicate and Return**: Collect the unique GN labels for the requested files. If a file or target cannot be found, the status will be set to "Unknown to Build".

## Detailed Design

### 1. Parsing `compile_commands.json`

The tool will read `compile_commands.json` to find the compilation command associated with each source file. According to the JSON Compilation Database specification, this can be provided in two formats:
- **`command`**: A single string representing the shell command.
- **`arguments`**: A list of strings representing the command and its arguments.

```json
{
  "directory": "/path/to/fuchsia/out/default",
  "arguments": ["../../prebuilt/clang/.../clang++", "-o", "obj/file.o", "-c", "../../file.cc"],
  "file": "../../path/to/file.cc"
}
```
OR
```json
{
  "directory": "/path/to/fuchsia/out/default",
  "command": "../../prebuilt/clang/.../clang++ -o obj/file.o -c ../../file.cc",
  "file": "../../path/to/file.cc"
}
```

The tool must handle both formats. If `command` is used, it will need to be parsed (split into arguments) while respecting shell escaping and quoting rules. The parser will be a simple state-machine style parser (inspired by Android's `cc_analyzer`) that:
- Iterates through the string.
- Handles double and single quotes to treat space-separated parts as a single argument.
- Correctly identifies the `-o` flag and its subsequent value.

### 2. Inferring Output Paths (`CcAnalyzer`)

The `CcAnalyzer` component will be responsible for parsing the compilation command.
- It will look for the `-o` flag to identify the primary output file.
- It handles relative paths by resolving them against the `directory` field in the compilation database.
- It will normalize paths to be relative to the Fuchsia root or absolute as required by the Build API.
- **Heuristics for Missing Files**: If a requested file `foo.h` is not found in the database, the analyzer will:
    1.  Look for other C++ files in the same source directory and "borrow" their flags.
    2.  If still not found, walk up the directory tree and look for a file with the same base name (e.g., `foo.cc`) in parent directories. This is particularly useful for Fuchsia's `include/` directory structure.
    This ensures headers typically resolve to the target of their corresponding implementation files.
- **Generated Header Detection**: (DEFERRED) The analyzer will inspect `-I` flags that point into the `BuildDir`. It will perform a lightweight scan of the source file's `#include` directives to identify headers that likely reside in the output directory.
- **C-Family Filtering**: The tool only attempts to resolve targets for files with C-family extensions (`.cc`, `.cpp`, `.cxx`, `.c`, `.h`, `.hh`, `.hpp`). Non-C++ files skip the target identification step.
- If no output path can be found after heuristics, the tool will mark the build target status as **"Unknown to Build"**.

### 3. Path Normalization

Entries in `compile_commands.json` often use relative paths.
- The `file` field is usually relative to the `directory` field.
- Output paths (from `-o`) are also typically relative to the `directory` field.

The tool must:
1.  Resolve the `file` path for each entry to a canonical absolute path to match against user-provided files.
2.  Resolve the inferred output path to a path relative to the `FuchsiaDir` (Ninja paths are usually relative to the build directory, and the Build API expects them in a specific format).

To avoid the overhead of repeated Python process spawning, the tool caches the resolution of Ninja output paths to GN targets.

The tool will:
1.  Map each source file to a Ninja output path (via the database or heuristics).
2.  Resolve each unique Ninja output path to a GN target using `./build/api/client ninja_path_to_gn_label <ninja_path>`.
3.  Cache the results of these resolutions so that multiple files belonging to the same target (e.g., in a driver or library) only trigger a single Build API call.
4.  Map the labels back to the corresponding `FileEntry` objects.

### 4. Handling Multiple Targets

A single source file can be part of multiple build targets (e.g., a shared header or a utility file). When `ide-query` encounters multiple targets for a file, it resolves all of them and selects the alphabetically first one. This heuristic provides a deterministic behavior and is sufficient for build-based IDE configuration where the primary goal is ensuring that *at least one* valid build context is prepared.

### 5. Implementation Components

- **`CompileCommands`**: A utility package for loading and querying `compile_commands.json`.
- **`CcAnalyzer`**: Logic for extracting metadata (outputs, flags) from compilation commands.
- **`BuildClient`**: A wrapper for executing `build/api/client` commands.

## Output Format

The tool will return the identified targets as part of the `WorkspaceContext` JSON output within each `FileEntry`.

```json
{
  "fuchsia_dir": "/path/to/fuchsia",
  "build_dir": "/path/to/fuchsia/out/default",
  "files": [
    {
      "abs_path": "/path/to/fuchsia/src/main.cc",
      "original_path": "src/main.cc",
      "status": "found",
      "is_directory": false,
      "build_targets": ["//src:main_target"]
    }
  ]
}
```

If a file cannot be mapped to a target, its `build_targets` list will be empty, and a status message (e.g., in a new `target_status` field) will indicate "Unknown to Build".

## Consumption of Targets

The identifying of these targets allows `ide-query` to automatically invoke the build system for precisely the artifacts needed for the current working set. This is handled internally by the tool using a shadow build directory.

See [BUILD_VERIFICATION.md](file:///google/cog/cloud/chaselatta/ide-query/fuchsia/scripts/cog/ide_query/docs/BUILD_VERIFICATION.md) for details on how these targets are built and verified.

This ensures that generated headers and other build or analysis dependencies are up-to-date before the language server processes the files.

## Assumptions

- `compile_commands.json` is present in the build directory. If missing, the tool may need to suggest running `fx gen`.
- The `build/api/client` is available and functioning correctly.
