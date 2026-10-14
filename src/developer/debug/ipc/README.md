<!--* freshness: { reviewed: '2026-10-12' } *-->

# Debug IPC (`ipc`)

This directory contains the shared wire protocol (`debug_ipc`) between
`debug_agent` ([../debug_agent](../debug_agent/README.md)) and `zxdb`
([../zxdb](../zxdb/README.md)). External programs should not depend on this
directory directly. Debugger clients should use the client library in
[../zxdb/client](../zxdb/client/README.md) instead.

`debug_ipc` uses a self-contained binary format so `debug_agent` can talk to a
host machine without high-level runtime dependencies while debugging low-level
system state.

## Protocol Format

`MessageWriter` and `MessageReader` encode wire primitives with these rules:

- Structs use fixed-size fields in little-endian byte order so little-endian
  hosts and targets can copy them directly with `memcpy`.
- Vectors encode a 32-bit element count followed by each serialized element.
- Strings encode a 32-bit byte length followed by raw 8-bit characters without a
  null terminator.

## Interface

Public headers expose these protocol constants, records, and helpers:

- `debug_ipc::kCurrentProtocolVersion`, `debug_ipc::kMinimumProtocolVersion`,
  `debug_ipc::kTimestampDefault`: Protocol version bounds and default timestamp
  constants (`protocol.h`).
- `debug_ipc::MsgHeader`, `debug_ipc::IsDebugIpcMessageType`,
  `debug_ipc::IsDebugIpcNotificationType`: Wire message header
  (`MsgHeader::Type`) and C++20 concepts for request/reply and notification
  types (`protocol.h`).
- Request and Reply messages (`protocol.h`): `HelloRequest`, `StatusRequest`,
  `KillRequest`, `AttachRequest`, `DetachRequest`, `PauseRequest`,
  `ResumeRequest`, `ProcessTreeRequest`, `ThreadsRequest`, `ReadMemoryRequest`,
  and their matching `*Reply` structs. Also defines
  `AddOrChangeBreakpointRequest`, `RemoveBreakpointRequest`, `SysInfoRequest`,
  `ThreadStatusRequest`, `AddressSpaceRequest`, `ModulesRequest`,
  `UpdateFilterRequest`, `WriteMemoryRequest`, and `LoadInfoHandleTableRequest`.
  Also defines `UpdateGlobalSettingsRequest`, `SaveMinidumpRequest`,
  `ReadRegistersRequest`, `WriteRegistersRequest`, `RunBinaryRequest`,
  `RunComponentRequest`, and `RunTestRequest`.
- Notification messages (`protocol.h`): `NotifyProcessStarting`,
  `NotifyProcessExiting`, `NotifyThreadStarting`, `NotifyThreadExiting`,
  `NotifyException`, `NotifyModules`, `NotifyIO`, `NotifyLog`,
  `NotifyComponentDiscovered`, `NotifyComponentStarting`,
  `NotifyComponentExiting`, `NotifyTestExited`, `NotifyFilterCreated`.
- Protocol records (`records.h`): `ExceptionType`, `ExceptionStrategy`,
  `ProcessThreadId`, `ExceptionRecord`, `ComponentInfo`, `TaskType`,
  `ProcessTreeRecord`, `StackFrame`, `ThreadRecord`, `AddressRegion`,
  `ProcessRecord`, `MemoryBlock`, `ProcessBreakpointSettings`, and `Stop`. Also
  defines `BreakpointType`, `BreakpointSettings`, `BreakpointStats`, `Module`,
  `AttachConfig`, `InfoHandleVmo`, `InfoHandle`, `FilterConfig`, `Filter`
  (`Filter::Identifier`, `Filter::Originator`), `MatchedTask`, and
  `FilterMatch`.
- Serialization (`message_reader.h`, `message_writer.h`):
  `debug_ipc::MessageReader`, `debug_ipc::MessageWriter`,
  `debug_ipc::Serialize`, `debug_ipc::Deserialize`.
- Filter utilities (`filter_utils.h`): `debug_ipc::FilterMatches`,
  `debug_ipc::ResolveFilterType`, `debug_ipc::GetFilterForId`,
  `debug_ipc::GenerateFilterIdValue`,
  `debug_ipc::GetAttachConfigsForFilterMatches`.
- Exception decoding (`decode_exception.h`): `debug_ipc::X64DebugRegs`,
  `debug_ipc::DecodeX64Exception`, `debug_ipc::DecodeArm64Exception`,
  `debug_ipc::DecodeRiscv64Exception`.
- Breakpoint automation (`automation_instruction.h`):
  `debug_ipc::AutomationOperandKind`, `debug_ipc::AutomationOperand`,
  `debug_ipc::AutomationConditionKind`, `debug_ipc::AutomationCondition`,
  `debug_ipc::AutomationInstructionKind`, `debug_ipc::AutomationInstruction`.
- Unwinder conversions (`unwinder_support.h`): `debug_ipc::ConvertRegisters`,
  `debug_ipc::ConvertFrames`, `debug_ipc::ConvertTrust`.

## Invariants

Callers and protocol definitions must preserve these invariants:

- `protocol.h` and `records.h` pack all wire structs to 8-byte alignment via
  `#pragma pack(push, 8)`.
- The ABI compatibility window spans `[kMinimumProtocolVersion,
  kCurrentProtocolVersion]`. Version-gated fields serialize conditionally (`if
  (ver >= ...)`), and messages with `kSupportedSinceVersion` return an empty
  byte vector on older versions.
- The first 4 bytes of every serialized message store the total byte size
  (including `MsgHeader`), which `MessageWriter::MessageComplete()` writes on
  completion.
- `HelloReply` starts with a fixed 64-bit signature (`0x7a7864624950433e`, ASCII
  `"zxdbIPC>"`). Readers deserialize `HelloReply` at version `0` first to read
  the version, and then deserialize again at the negotiated version.
- Notification messages always set `transaction_id` to `0` in `MsgHeader` and
  satisfy `IsDebugIpcNotificationType` by providing a `timestamp` field.
- `Filter::Identifier` stores `Filter::Originator` in bits 24 to 31 and a
  client-assigned 24-bit ID in bits 0 to 23. Both `kInvalidFilterId` and
  `kDebugAgentInternalBreakpointId` equal `static_cast<uint32_t>(-1)`.
- When multiple filters match one task in `GetAttachConfigsForFilterMatches`,
  the highest `AttachConfig::Priority` (`kStrong > kWeak > kMinimal`) wins.
- In `BreakpointSettings::locations`, if any `ProcessBreakpointSettings` entry
  specifies a non-zero thread KOID, it must be the only entry.

## Side Effects

These functions produce the following state and callback side effects:

- `ResolveFilterType` truncates its `pattern` string argument in place to 31
  characters (Zircon's process name limit) for `kProcessName` and
  `kProcessNameSubstr`.
- `GenerateFilterIdValue` advances a function-local static random number engine
  (`std::independent_bits_engine`) to generate 24-bit IDs and asserts that the
  upper 8 bits are zero.
- `MessageWriter::MessageComplete()` writes the buffer size into the first 4
  bytes and moves the internal buffer out, leaving the writer empty.
- `MessageReader::SerializeBytes` advances the read offset or sets the sticky
  `has_error_` flag when fewer bytes remain than requested.
- `DecodeX64Exception` and `DecodeArm64Exception` call `fetch_debug_regs` or
  `fetch_esr` callbacks on demand to disambiguate hardware breakpoints,
  watchpoints, and single-step traps.

## Verification

Run these commands to build and test `ipc`:

- Build: `fx build //src/developer/debug/ipc`
- Test: `fx test zxdb_tests`
