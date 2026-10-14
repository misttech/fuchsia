<!--* freshness: { reviewed: '2026-10-12' } *-->

# POSIX Utilities (`posix`)

## Overview

`//src/developer/debug/debug_agent/posix` provides low-level, allocation-free
POSIX and Linux system call utilities for `debug_agent`. It includes signal-safe
`EINTR` retry macros, a non-allocating Linux directory reader (`getdents64`),
and an injective file-descriptor table shuffler safe for post-`fork` execution.

## Interface

Public headers expose these types, macros, and functions:

- `linux::linux_dirent`: 64-bit directory entry structure for `getdents64`
  (`dir_reader_linux.h`).
- `linux::DirReaderLinux`: Non-copyable, fixed-buffer Linux directory reader
  (`DirReaderLinux(const char* directory_path)`, `IsValid()`, `Next()`,
  `name()`, `fd()`, `IsFallback()`) (`dir_reader_linux.h`).
- `HANDLE_EINTR(x)`: Macro that retries POSIX expression `x` while it returns
  `-1` with `errno == EINTR` (`eintr_wrapper.h`).
- `IGNORE_EINTR(x)`: Macro that evaluates POSIX expression `x` once and converts
  a `-1` return with `errno == EINTR` to `0` (`eintr_wrapper.h`).
- `posix::InjectionDelegate`: Abstract interface (`Duplicate`, `Move`, `Close`)
  for file-descriptor table operations (`file_descriptor_shuffle.h`).
- `posix::FileDescriptorTableInjection`: Default `InjectionDelegate`
  implementation backed by `dup`, `dup2`, and `close`
  (`file_descriptor_shuffle.h`).
- `posix::InjectionArc` / `posix::InjectiveMultimap`: Mapping element (`source`,
  `dest`, `close_source`) and vector defining an injective file-descriptor
  remapping (`file_descriptor_shuffle.h`).
- `posix::PerformInjectiveMultimap(const InjectiveMultimap& map,
  InjectionDelegate* delegate)`: Non-destructive wrapper that copies `map` and
  executes the descriptor remapping (`file_descriptor_shuffle.h`).
- `posix::PerformInjectiveMultimapDestructive(InjectiveMultimap* map,
  InjectionDelegate* delegate)`: Allocation-free, lock-free file-descriptor
  remapping algorithm (`file_descriptor_shuffle.h`).
- `posix::ShuffleFileDescriptors(InjectiveMultimap* map)`: Entry point that
  applies `PerformInjectiveMultimapDestructive` with
  `FileDescriptorTableInjection` (`file_descriptor_shuffle.h`).

## Invariants

Callers and implementations must preserve these invariants:

- `linux::DirReaderLinux` is non-copyable and performs no dynamic memory
  allocations. It uses a fixed 512-byte buffer aligned to `linux_dirent` so it
  remains safe in post-`fork` child processes.
- `linux::DirReaderLinux::name()` returns `nullptr` before the first successful
  call to `Next()` (when `size_ == 0`).
- `HANDLE_EINTR(x)` retries `x` while `x` returns `-1` with `errno == EINTR`. In
  non-`NDEBUG` builds, `HANDLE_EINTR(x)` caps retries at 100 iterations to catch
  stuck signal loops.
- `IGNORE_EINTR(x)` evaluates `x` once without retrying and converts a `-1`
  return with `errno == EINTR` into `0`.
- `posix::PerformInjectiveMultimapDestructive` and
  `posix::ShuffleFileDescriptors` never allocate memory or acquire locks, making
  them safe in post-`fork` contexts.
- `posix::PerformInjectiveMultimapDestructive` requires unique destination
  descriptors in `InjectiveMultimap` (`FX_DCHECK(i->dest != j->dest)`).
- `posix::FileDescriptorTableInjection::Close` asserts via `FX_DCHECK` that
  `IGNORE_EINTR(close(fd))` returns `0`.

## Side Effects

These utilities produce the following OS and memory side effects:

- `linux::DirReaderLinux` opens `directory_path` with `O_RDONLY | O_DIRECTORY`
  on construction, calls `syscall(__NR_getdents64, ...)` in `Next()`, and closes
  `fd_` on destruction.
- `posix::PerformInjectiveMultimapDestructive` and
  `posix::ShuffleFileDescriptors` mutate the passed `InjectiveMultimap` in place
  as they update temporary descriptors and close flags.
- `posix::FileDescriptorTableInjection` mutates the process file-descriptor
  table via `dup`, `dup2`, and `close` system calls.

## Verification

Run these commands to build and test `posix`:

- Build: `fx build //src/developer/debug/debug_agent:lib`
- Test: `fx test --host debug_agent_unit_tests`
