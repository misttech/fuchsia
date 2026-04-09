# `tools/check-licenses/directory`

The `directory` package is the core filesystem crawler for the
`check-licenses` tool. It is responsible for recursively traversing the
Fuchsia source tree, identifying project boundaries, maintaining license
inheritance, and bucketing files for the downstream `file` and `project`
packages to process.

## Core Architecture

The package revolves around a single core data structure and its associated
traversal engine.

### `Directory` (`directory.go`)
Represents an in-memory node in the repository's filesystem tree.

*   **Tree Structure:** Every `Directory` maintains a `Parent` pointer and a
    slice of `Children` directories, effectively mirroring the actual layout of
    the Fuchsia repository.
*   **File Bucketing:** As the crawler encounters files, it asks the `file`
    package to classify them. It then populates the `Files` slice with the
    resulting `*file.File` objects.
*   **Project Inheritance:** The most critical job of the `Directory` struct
    is maintaining a pointer to a `*project.Project`. When the crawler creates
    a new `Directory` node, it defaults to inheriting its parent's `Project`.
    This ensures that all files deeply nested in a project's folder structure
    are accurately attributed to the correct open-source author.

## The Traversal Engine

The `newDirectoryWithConfig()` function is the recursive engine that builds
the directory tree. At each step, it performs the following evaluations:

### 1. Barrier Detection
If the crawler hits a known "barrier" directory (e.g., `third_party` or
`prebuilt`), it forcefully breaks project inheritance. The `Directory`'s
Project pointer is reset to `project.UnknownProject`, forcing any code within
that barrier to explicitly establish its own compliance identity.

### 2. Project Boundary Detection (`readme.go`)
*(Note: The logic inside `readme.go` is scheduled to be refactored and moved
to the `project` package in a future CL to better separate concerns.)*

If a directory contains a `README.fuchsia` (or `.chromium`/`.crashpad`), the
crawler parses the file to bootstrap a completely new `*project.Project`.
For ecosystems that do not use `README` files (like vendored Go, Rust, or
Dart dependencies), `readme.go` uses hardcoded path matching rules to identify
their roots and dynamically generate custom in-memory `Readme` objects.

### 3. File and Symlink Processing
The engine evaluates the contents of the folder.
*   **Skips:** It checks `Config.Skips` (populated by `_config.json` and
    `project.Config.Readmes`) to see if the current item should be entirely
    ignored.
*   **Symlinks:** It contains explicit edge-case handling for symlinks.
    Symlinks pointing to directories are aggressively skipped to prevent
    infinite recursive loops. Symlinks pointing to files are evaluated normally.
*   **File Handoff:** Files are passed to `file.LoadFile()` and appended to
    both the `Directory.Files` slice and the active `Project.RegularFiles`
    slice.

## Thread Safety and Global State (`config.go`)

Because the final compliance report needs to iterate over the entire directory
structure independently of the tree hierarchy, the package maintains a global
cache of all instantiated directories.

*   **`AddDirectory()`**: A thread-safe setter that registers a `Directory`
    into the private `allDirectories` map using a `sync.RWMutex`.
*   **`GetAllDirectories()`**: A thread-safe getter that returns a shallow
    copy of the global map, preventing concurrent iteration panics while the
    crawler is still running in the background.
