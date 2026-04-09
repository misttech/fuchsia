# `tools/check-licenses/project`

The `project` package is the core compliance engine for the `check-licenses`
tool. It is responsible for defining the boundaries of third-party open-source
projects, filtering out unused code using the Fuchsia build graph, orchestrating
highly concurrent license analysis, and exporting compliance metadata.

## Core Architecture

*(Note: The logic for parsing project boundaries currently lives in `directory/readme.go`. It is scheduled to be refactored and moved directly into the `project` package in a future CL to better separate filesystem crawling from project boundary definition.)*

### `Project` (`project.go`)
The foundational data model representing a distinct open-source project or
first-party codebase boundary.
*   **Ownership:** It maintains slices of `RegularFiles` and `LicenseFiles`
    that the `directory` crawler has attributed to it.
*   **Thread Safety:** As the `directory` crawler operates, it appends files
    to these slices. A dedicated `sync.Mutex` (`p.mu`) protects the file arrays
    from concurrent mutations, ensuring thread safety during aggressive repo
    traversals.
*   **Metadata:** It holds a pointer to a `readme.Readme` object, which contains
    upstream URLs, version information, and explicit license declarations.

### Global State (`config.go`)
Because projects are cross-referenced across the entire workspace, the package
maintains thread-safe global maps:
*   `allProjects`: A cache of every project discovered in the repository.
*   `filteredProjects`: A subset of projects that are actively compiled into
    the current build target.
These maps are protected by `sync.RWMutex` locks, and must be accessed using
their respective `Get*` and `Add*` accessor functions.

## The Filtering Engine (`filter.go`)

Fuchsia's repository contains millions of files, but a given product build only
actually compiles a tiny fraction of them. The `filter` engine is responsible
for dropping unused projects to minimize the final compliance report.

1.  **GN Integration:** It shells out to `gn gen` to produce a `project.json`
    dependency graph containing every target required for the current build.
2.  **Reverse Lookup:** It builds a massive `fileMap` linking every single file
    path discovered by the crawler to its owning `Project`.
3.  **Graph Traversal:** It walks the GN dependency tree. For every required
    source file, it finds the owning `Project` and moves it into the
    `filteredProjects` map.
4.  **Deduplication:** It aggressively deduplicates identical license texts
    (e.g., standard MIT licenses) across all filtered projects to shrink the
    final `NOTICE` file output. It uses a high-performance hash-key map to
    avoid unnecessary string allocations.

## The Concurrency Engine (`analyze.go`)

Once the projects are filtered, `AnalyzeLicenses()` orchestrates a massive
parallel workload to identify copyright texts.
*   It spins up goroutines bounded by `runtime.NumCPU()`.
*   It invokes `file.Search()` on every source file and license file across all
    filtered projects, triggering the Google License Classifier (`v2`) to
    execute concurrently.

## SPDX Exporter (`spdx.go`)

The package includes an exporter to map the internal `Project` struct into an
official SPDX v2.2 `spdx.Package` struct.
*   **Identity:** It generates deterministic `SPDXID` strings based on the
    project's root path.
*   **License Expressions:** It constructs mathematically valid boolean
    expressions (e.g., `(LicenseRef-A AND LicenseRef-B)`) required by the
    SPDX specification to describe how multiple license texts apply to the
    project.
*   **Validation:** It strictly initializes empty arrays to satisfy the online
    SPDX validator's syntax requirements.
