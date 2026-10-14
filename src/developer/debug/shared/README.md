<!--* freshness: { reviewed: '2026-10-12' } *-->

# Shared Debugger Utilities (`shared`)

## Overview

`//src/developer/debug/shared` provides cross-platform abstractions and data
types shared between the host debugger (`zxdb`) and the target stub
(`debug_agent`). It includes the `fpromise`-integrated `MessageLoop`, buffered
I/O streams, CPU register metadata, `AddressRange`, binary `Serializer`,
`Status`/`Result` wrappers, RE2 `Regex`, and logging utilities.

## Interface

Public headers and subdirectories expose these components:

- [logging](./logging/README.md): Severity-based and category-filtered
  hierarchical debug logging (`LOGS`, `DEBUG_LOG`, `FROM_HERE`,
  `debug::LogSeverity`, `debug::LogStatement`, `debug::LogBackend`,
  `debug::LogCategory`, `debug::SetDebugLogging`, `debug::DebugLogStatement`,
  `debug::FileLineFunction`).
- Message loop (`message_loop.h`, `message_loop_fuchsia.h`,
  `message_loop_linux.h`, `message_loop_poll.h`, `platform_message_loop.h`):
  `debug::MessageLoop` (`WatchMode`, `WatchHandle`, `FDWatcher`),
  `debug::MessageLoopContext`, `debug::MessageLoopFuchsia` (`WatchInfo`,
  `WatchProcessConfig`, `WatchJobConfig`, `WatchType`, `WatchTypeToString`),
  `debug::MessageLoopLinux` (`SignalWatcher`), `debug::MessageLoopPoll`,
  `debug::PlatformMessageLoop`.
- Buffered streams (`stream_buffer.h`, `buffered_stream.h`, `buffered_fd.h`,
  `buffered_zx_socket.h`, `buffered_bidi_pipe.h`): `debug::StreamBuffer`
  (`StreamBuffer::Writer`), `debug::BufferedStream`, `debug::BufferedFD`,
  `debug::BufferedZxSocket`, `debug::BufferedBidiPipe`.
- Address ranges (`address_range.h`): `debug::AddressRange`,
  `debug::AddressRangeBeginCmp`, `debug::AddressRangeEndAddrCmp`,
  `debug::AddressRangeEqualityCmp`.
- Architecture and registers (`arch.h`, `register_id.h`, `register_info.h`,
  `register_value.h`): `debug::Arch`, `debug::ArchToString`,
  `debug::RegisterID`, `debug::SpecialRegisterType`, `debug::RegisterFormat`,
  `debug::RegisterCategory`, `debug::RegisterInfo`, `debug::InfoForRegister`,
  `debug::RegisterIDToString`, and `debug::StringToRegisterID`. Also exposes
  `debug::GetSpecialRegisterID`, `debug::GetSpecialRegisterType`,
  `debug::DWARFToRegisterInfo`, `debug::GetArchForRegisterID`,
  `debug::IsGeneralRegister`, `debug::GetRegisterData`,
  `debug::RegisterCategoryToString`, `debug::RegisterIDToCategory`, and
  `debug::RegisterValue`.
- Error handling and results (`status.h`, `result.h`, `zx_status.h`):
  `debug::Status` (`Status::Type`), `debug::ZxStatus`, `debug::ErrnoStatus`,
  `debug::Result`, `debug::ZxStatusToString`.
- Serialization and utilities (`serialization.h`, `regex.h`, `handle_info.h`):
  `debug::Serializer` (`debug_ipc::Serializer`), `debug::Regex`
  (`Regex::CompareType`), `debug::HandleTypeToString`,
  `debug::CachePolicyToString`, `debug::HandleRightsToStrings`,
  `debug::VmoFlagsToStrings`.

## Invariants

Callers and implementations must preserve these invariants:

- Only one `MessageLoop` may be active per thread between `Init()` and
  `Cleanup()`, which must run on the same thread as `Run()`. Call `QuitNow()`,
  `WatchFD()`, Zircon watch methods, and `WatchHandle` destructors only on the
  `MessageLoop` thread.
- Watch each file descriptor at most once per `MessageLoop`. Do not unregister a
  `MessageLoopFuchsia::WatchSocket` `WatchHandle` inside its callback because
  the loop may invoke read and write callbacks back-to-back.
- Call `StreamBuffer::set_writer()` before using a `StreamBuffer`. Initialize a
  `MessageLoop` on the current thread and pass valid handles or FDs before
  calling `Start()` on `BufferedFD`, `BufferedZxSocket`, or `BufferedBidiPipe`.
- `AddressRange` enforces `end >= begin` via `FX_DCHECK` and uses half-open
  `[begin, end)` bounds, so it cannot represent `UINT64_MAX`.
- `Arch` enum values appear in the wire protocol, so only append new values.
  `RegisterID` partitions values into fixed ranges by architecture (`1000` to
  `1399` for ARMv8, `2000` to `2699` for x64, `3000` to `3699` for riscv64).
- `RegisterInfo::bits` and `RegisterInfo::shift` for sub-register aliases must
  be multiples of 8 for `GetRegisterData()`. `RegisterValue::data` stores bytes
  in the target architecture's native endianness.
- `Status(Type, ...)` asserts if called with `kSuccess` or `kPlatformError`. Use
  the default constructor or `ZxStatus`/`ErrnoStatus` helpers instead.
- `Result<T, E>::value()` and `take_value()` assert `!has_error()` via
  `FX_DCHECK`, whereas `Result<T, E>::err()` asserts `has_error()`.
- On stream EOF or disconnect in `BufferedFD` and `BufferedStream`, check
  `stream().IsAvailable(1)` and drain buffered bytes via the read callback
  first. Draining before calling the error callback prevents dropping trailing
  messages sent right before socket closure.
- To stop a `MessageLoop` watch, call `loop.StopWatching(watch_handle)`
  explicitly instead of reassigning `watch_handle = WatchHandle()`.

## Side Effects

These utilities produce the following OS, thread, and I/O side effects:

- `MessageLoop` and its subclasses set thread-local loop pointers, lock task
  queues, create OS wakeup pipes or Zircon `async::Loop` channels, and run
  callbacks.
- `BufferedFD`, `BufferedZxSocket`, and `BufferedBidiPipe` own `fbl::unique_fd`
  or `zx::socket` handles, register `MessageLoop` watches, perform non-blocking
  I/O, and invoke stream callbacks.
- `Serializer::operator|` mutates arguments in place during deserialization
  (resizing `std::vector` and updating `std::optional`) or writes bytes via
  `SerializeBytes()`.
- Logging utilities (`LOGS`, `DEBUG_LOG`) record source locations and timing
  metrics, and write formatted logs to `LogBackend` sinks or standard streams.

## Verification

Run these commands to build and test `shared`:

- Build: `fx build //src/developer/debug/shared`
- Test: `fx test zxdb_tests`
