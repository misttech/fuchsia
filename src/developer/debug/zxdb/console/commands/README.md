<!--* freshness: { reviewed: '2026-10-12' } *-->

# Zxdb Console Commands (`zxdb/console/commands`)

## Overview

`//src/developer/debug/zxdb/console/commands` implements individual console verb
commands for `zxdb`. Each `verb_*.h` header exposes a `Get*VerbRecord()` factory
returning a `VerbRecord` with command aliases, help text, switches, and
execution callbacks.

## Interface

Public headers group verb records and helpers into these functional areas:

- Target, connection, and status verbs (`verb_attach.h`, `verb_connect.h`,
  `verb_detach.h`, `verb_new.h`, `verb_rm.h`, `verb_run.h`, `verb_run_test.h`,
  `verb_status.h`, `verb_help.h`): `zxdb::kZirconMaxNameLength`,
  `zxdb::GetAttachVerbRecord()`, `zxdb::GetConnectVerbRecord()`,
  `zxdb::GetDetachVerbRecord()`, and `zxdb::GetNewVerbRecord()`. Also exposes
  `zxdb::GetRmVerbRecord()`, `zxdb::GetRunVerbRecord()`,
  `zxdb::GetRunTestVerbRecord()`, `zxdb::GetHelpVerbRecord()`, and
  `zxdb::GetStatusVerbRecord()`. Also exposes `zxdb::GetConnectionStatus`,
  `zxdb::GetFilterStatus`, `zxdb::GetJobStatus`, `zxdb::GetProcessStatus`, and
  `zxdb::GetLimboStatus`.
- Execution control and stepping verbs (`verb_continue.h`, `verb_pause.h`,
  `verb_step.h`, `verb_stepi.h`, `verb_next.h`, `verb_nexti.h`, `verb_finish.h`,
  `verb_until.h`): `zxdb::GetContinueVerbRecord()`,
  `zxdb::GetPauseVerbRecord()`, `zxdb::GetStepVerbRecord()`,
  `zxdb::GetStepiVerbRecord()`, `zxdb::GetNextVerbRecord()`,
  `zxdb::GetNextiVerbRecord()`, `zxdb::GetFinishVerbRecord()`, and
  `zxdb::GetUntilVerbRecord()`.
- Breakpoint and watchpoint verbs (`verb_break.h`, `verb_watch.h`):
  `zxdb::GetBreakVerbRecord()` and `zxdb::GetWatchVerbRecord()`.
- Stack, frame, and async backtrace verbs (`verb_backtrace.h`,
  `verb_async_backtrace.h`, `verb_down.h`, `verb_stack_data.h`,
  `verb_stack_usage.h`): `zxdb::GetBacktraceVerbRecord()`,
  `zxdb::GetAsyncBacktraceVerbRecord()`, `zxdb::GetDownVerbRecord()`,
  `zxdb::OutputFrameInfoForChange`, `zxdb::GetStackDataVerbRecord()`, and
  `zxdb::GetStackUsageVerbRecord()`. Also exposes `zxdb::OneStackUsage`,
  `zxdb::ThreadStackUsage`, and `zxdb::GetThreadStackUsage`.
- Memory, disassembly, and register verbs (`verb_disassemble.h`,
  `verb_mem_analyze.h`, `verb_mem_read.h`, `verb_regs.h`):
  `zxdb::GetDisassembleVerbRecord()`, `zxdb::GetMemAnalyzeVerbRecord()`,
  `zxdb::kMemAnalyzeSizeSwitch`, `zxdb::kMemAnalyzeNumSwitch`,
  `zxdb::ReadAnalyzeNumAndSizeSwitches(const Command&,
  std::optional<uint32_t>*)`, `zxdb::kDefaultAnalyzeByteSize`,
  `zxdb::GetMemReadVerbRecord()`, and `zxdb::GetRegsVerbRecord()`.
- Expression, variable, and symbol inspection verbs (`verb_locals.h`,
  `verb_print.h`, `verb_libs.h`, `verb_sym_info.h`, `verb_sym_near.h`,
  `verb_sym_stat.h`): `zxdb::GetLocalsVerbRecord()`,
  `zxdb::GetPrintVerbRecord()`, `zxdb::GetLibsVerbRecord()`,
  `zxdb::GetSymInfoVerbRecord()`, `zxdb::GetSymNearVerbRecord()`, and
  `zxdb::GetSymStatVerbRecord()`.

## Invariants

Verb implementations must preserve these invariants:

- `kZirconMaxNameLength` is `31` (matching `ZX_MAX_NAME_LEN - 1`).
- Verb callbacks check allowed `Noun` combinations via `Command::ValidateNouns`
  before acting on targets, threads, frames, or breakpoints.
- Thread stepping verbs (such as `step`) require a stopped thread with a valid
  stack frame via `AssertStoppedThreadWithFrameCommand`.
- The `run` verb supports only Linux targets (`debug::Platform::kLinux`) and
  returns an error on Fuchsia targets.
- Attaching by numeric process KOID rejects duplicate KOIDs in
  `System::ProcessFromKoid` and rejects filter switches (`--job`, `--exact`).
  Attaching via filters forbids the `process` noun.
- Hardware write and read-write watchpoint addresses must align to an even
  multiple of their byte size.

## Side Effects

Executing console verbs produces these session and console side effects:

- Mutates debugger session and target state by attaching or detaching processes,
  starting binaries, updating filters and breakpoints, and stepping threads via
  `ThreadController` instances.
- Evaluates target expressions asynchronously and writes output or errors
  through `CommandContext::Output` and `CommandContext::ReportError`.
- Updates active `ConsoleContext` selections (such as selecting newly created
  breakpoints or active stack frames).

## Verification

Run these commands to build and test `zxdb/console/commands`:

- Build: `fx build //src/developer/debug/zxdb/console`
- Test: `fx test zxdb_tests`
