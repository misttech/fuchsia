<!--* freshness: { reviewed: '2026-10-12' } *-->

# Zxdb Common Library (`zxdb/common`)

## Overview

`//src/developer/debug/zxdb/common` is the lowest layer (Layer 1) of `zxdb`. It
provides foundational error types (`Err`, `ErrOr`), address ranges, binary/LEB
extractors, filesystem/network helpers, `Curl`, and callback joiners without
depending on any other `zxdb` layer.

## Interface

Public headers expose these foundational types and utilities:

- `zxdb::Err`, `zxdb::ErrType`, `zxdb::ErrOr<T>`, `zxdb::ErrTypeToString`,
  `zxdb::ErrToUnwinderError`: Error status and value-or-error wrappers (`err.h`,
  `err_or.h`).
- `zxdb::AddressRange`, `zxdb::AddressRangeBeginCmp`,
  `zxdb::AddressRangeEndAddrCmp`, `zxdb::AddressRanges`: Half-open memory
  address interval and canonical non-overlapping range set (`address_range.h`,
  `address_ranges.h`).
- `zxdb::TaggedData`, `zxdb::TaggedDataBuilder`: Byte buffers paired with
  per-byte validity tags (`Tag::kValid`, `Tag::kUnknown`) for memory dumps with
  unreadable regions (`tagged_data.h`, `tagged_data_builder.h`).
- `zxdb::DataExtractor`: Bounds-checked cursor for reading primitive types, byte
  slices, and SLEB128/ULEB128 integers from binary buffers (`data_extractor.h`).
- `zxdb::AppendULeb`: Appends ULEB128-encoded integers to a byte vector
  (`leb128.h`).
- `zxdb::JoinCallbacksBase`, `zxdb::JoinCallbacks<T>`,
  `zxdb::JoinCallbacks<void>`, `zxdb::JoinErrCallbacks`: Fan-in primitives that
  aggregate multiple asynchronous callbacks into one completion callback
  (`join_callbacks.h`).
- `zxdb::MakeErrPromise<ResultType>`: Builds a `fpromise::promise` that resolves
  immediately to a `zxdb::Err` (`promise.h`).
- `zxdb::CacheDir`: Size-bounded directory manager that prunes
  least-recently-used files on access notifications (`cache_dir.h`).
- `zxdb::Curl`: Reference-counted `libcurl` wrapper for synchronous and
  asynchronous HTTP/HTTPS transfers (`curl.h`).
- `zxdb::ScopedTempFile`: RAII temporary file creator and deleter
  (`scoped_temp_file.h`).
- `zxdb::FuzzyMatcher`: Subsequence line matcher over text streams
  (`fuzzy_matcher.h`).
- `WorkerPool`: Multi-threaded task queue (`PostTask`) backed by `std::thread`
  workers (`worker_pool.h`).
- `zxdb::int128_t`, `zxdb::uint128_t`, `zxdb::to_string`, `zxdb::to_hex_string`,
  `zxdb::to_bin_string<T>`: 128-bit integer types and numeric string formatters
  (`int128_t.h`, `string_util.h`).
- `zxdb::CheckedAdd`: Overflow-checked integer addition (`checked_math.h`).
- `zxdb::Reversed`, `zxdb::RefPtrTo`: Range-for reverse iterator adapter and
  `fxl::RefPtr<T>` conversion helper (`adapters.h`, `ref_ptr_to.h`).
- `zxdb::ExtractLastFileComponent`, `zxdb::IsPathAbsolute`,
  `zxdb::PathEndsWith`, `zxdb::CatPathComponents`, `zxdb::NormalizePath`,
  `zxdb::GetFileModificationTime`, `zxdb::PathStartsWith`,
  `zxdb::PathRelativeTo`, `zxdb::ExpandAndNormalizePath`: Host filesystem path
  manipulation and metadata helpers (`file_util.h`).
- `zxdb::GetSelfPath`, `zxdb::GetEnviron`: Host executable path and process
  environment accessors (`host_util.h`).
- `zxdb::ParseHostPort`, `zxdb::Ipv6HostPortIsMissingBrackets`: Host and port
  string parsers (`inet_util.h`).

## Invariants

Callers and implementations must preserve these invariants:

- `zxdb::AddressRanges` stores ranges in canonical form: sorted by start
  address, non-overlapping (adjacent ranges stay separate), and non-empty.
- `zxdb::ErrOr<T>` requires that any `Err` passed to its error constructor or
  returned by `err()` has `has_error() == true` (`ErrType != kNone`).
- Create `zxdb::Curl` instances with `fxl::MakeRefCounted<Curl>()`, and bracket
  all `Curl` usage with `Curl::GlobalInit()` and `Curl::GlobalCleanup()`.
- `zxdb::DataExtractor` never advances its cursor past the buffer end. Failed
  reads leave the cursor unchanged, while `Advance` and `Seek` clamp to the
  buffer end.
- `zxdb::TaggedData` leaves `tags_` empty when all bytes are valid. Otherwise,
  `tags_.size()` equals `bytes_.size()`.
- Create all `zxdb::JoinCallbacksBase` sub-callbacks via `AddCallback()` before
  calling `Ready()` or `Abandon()`. `JoinCallbacks<T>` preserves `AddCallback()`
  creation order in the result vector.
- `zxdb::PathRelativeTo` requires both input paths to be absolute, and
  `zxdb::PathStartsWith` returns `false` if either input path is relative.

## Side Effects

These utilities produce the following disk, network, and thread side effects:

- `zxdb::CacheDir::NotifyFileAccess` updates file modification times and deletes
  least-recently-used files when directory size exceeds `max_size_`.
- `zxdb::Curl::GlobalInit` and `GlobalCleanup` mutate thread-unsafe global
  `libcurl` state. `Curl::Perform` runs synchronous or message-loop-driven
  HTTP/HTTPS transfers.
- `zxdb::ScopedTempFile` creates and opens a temporary file in `/tmp` on
  construction and unlinks it on destruction.
- `zxdb::FuzzyMatcher::MatchesLine` consumes lines from its internal
  `std::stringstream`, advancing matcher state across calls.
- `zxdb::TaggedDataBuilder::TakeData` and `zxdb::ErrOr<T>::take_value` move
  internal data out of the object.
- `WorkerPool` spawns OS worker threads on construction and blocks to join them
  on destruction.
- `zxdb::GetFileModificationTime`, `zxdb::ExpandAndNormalizePath`,
  `zxdb::GetSelfPath`, and `zxdb::GetEnviron` query the host filesystem and
  process environment.

## Verification

Run these commands to build and test `zxdb/common`:

- Build: `fx build //src/developer/debug/zxdb/common`
- Test: `fx test zxdb_tests`
