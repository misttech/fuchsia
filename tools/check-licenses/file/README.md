# `tools/check-licenses/file`

The `file` package is the core data ingestion engine for the `check-licenses`
tool. It is responsible for efficiently traversing the Fuchsia source tree,
lazy-loading files, normalizing legacy character encodings, and invoking the
Google License Classifier (`v2`) to accurately detect copyright and license
information.

## Core Architecture

This package operates around a strict two-tier data structure:

### 1. `File` (`file.go`)
Represents a physical file on the disk (e.g., `src/main.cc` or
`vendor/google/LICENSE`).
*   **Memory Efficiency:** Files are not immediately read into memory when
    `LoadFile` is called. The tool uses a lazy-loading pattern
    (`f.LoadContent()`) that defers disk I/O until the specific file's content
    or classifier results are actually requested.
*   **Truncation (`CopyrightSize`):** To prevent Out-Of-Memory (OOM) crashes
    when traversing the tens of thousands of regular source code files in the
    Fuchsia monorepo, the tool only reads the first *N* bytes of a `RegularFile`
    (configured via `Config.CopyrightSize` in `_config.json`). This works safely
    because copyright headers are conventionally placed at the absolute top of
    a source file.
*   **Global Deduplication Cache:** A thread-safe global map (`AllFiles`) acts
    as a singleton cache. If multiple projects or traversals attempt to load
    the exact same absolute path, the system immediately returns the cached
    `*File` pointer, saving massive amounts of redundant disk I/O.

### 2. `FileData` (`filedata.go`)
Represents an isolated, distinct block of text containing a single license or
copyright.
*   **1-to-1 Mapping:** For standard source code files (`RegularFile`) or
    single-license files (`SingleLicense`), a `File` contains exactly one
    `FileData` object.
*   **1-to-N Mapping:** Third-party prebuilt libraries often bundle massive
    `NOTICE` files that contain dozens of concatenated licenses. For these
    (`MultiLicense`, `MultiLicenseChromium`, etc.), a single `File` object acts
    as the parent container holding an array of multiple distinct `FileData`
    objects—one for each license text segment.

## The Parser Engine (`notice.go`)

Because large upstream projects (like Android, Chromium, or Flutter) have
entirely different standard formats for how they bundle their aggregated
`NOTICE` files, `notice.go` provides custom parsing functions (e.g.,
`ParseAndroid`, `ParseChromium`).

These parsers take the raw `NOTICE` bytes, identify the specific delimiters
(like `=================`), and cleanly slice the text into individual
`FileData` chunks.
*   **Memory-Optimized Deduplication:** Upstream `NOTICE` files frequently
    repeat the exact same license text (e.g., the MIT license) for hundreds of
    different sub-libraries. The `mergeDuplicates` function uses a nested
    hashmap (`map[LibraryName]map[StringText]bool`) to deduplicate these
    segments. Go's compiler natively optimizes `map[string(byteSlice)]` lookups
    to prevent heap allocations, making this extremely performant.

## The UTF-8 Transliterator (`encoding.go`)

Many legacy open-source libraries use files encoded in `Windows-1252` or
`ISO-8859-1` instead of standard `UTF-8`. Historically, this caused downstream
HTML templates or the SPDX JSON marshaler to output garbled "Mojibake"
characters (like `â€œ` instead of `“`).

To resolve this, the tool passes all files through a zero-dependency
transliterator (`forceUTF8`) immediately upon reading from disk.
*   It checks the file against Go's fast-path `utf8.Valid(bytes)`.
*   If the file contains invalid UTF-8 bytes (like `0x93` for a smart-quote or
    `0xA9` for a copyright symbol in Windows-1252), it intercepts them and
    safely translates them into proper multi-byte UTF-8 sequences.
*   This guarantees that the License Classifier and all downstream reporting
    systems *only* ever see pristine UTF-8.

## Concurrency and Thread Safety

Because `check-licenses` uses an aggressive multi-threaded traversal system,
the `file` package is heavily fortified against data races.

*   **`sync.RWMutex`:** Every mutable state object (`*File`, `*FileData`,
    `Metrics.counts`) is protected by read-write mutexes.
*   **Double-Checked Locking:** Computationally expensive operations, like
    `classifier.Match(bytes)`, use the Double-Checked Locking (DCL) pattern.
    The struct grabs a read-lock to check if the results are already cached.
    If not, it upgrades to a write-lock, checks the cache *again* to prevent
    a TOCTOU (Time-Of-Check to Time-Of-Use) race, and then executes the
    classifier precisely once.

## SPDX ID Integrity

Each `FileData` generates a completely unique `SPDXID` string
(`LicenseRef-filedata-<hash>`) based on an FNV hash of its source library name
and text content.

When the downstream `check-licenses/copyright` package isolates the exact
copyright string within a massive block of source code, it invokes
`FileData.SetData()` to truncate the text. The `SetData()` function safely
locks the struct, updates the bytes, and **regenerates the SPDXID hash
dynamically**, ensuring that the final published compliance document hashes
match perfectly with the provided source code blocks.
