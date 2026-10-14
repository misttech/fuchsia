<!--* freshness: { reviewed: '2026-10-12' } *-->

# Zxdb Console (`zxdb/console`)

## Overview

`//src/developer/debug/zxdb/console` implements the interactive and
non-interactive command-line interface (Layer 5) for `zxdb`. It handles CLI flag
parsing, noun/verb tokenization and dispatch, active object tracking via
`ConsoleContext`, input location resolution, and terminal formatting for frames,
registers, and tables.

## Interface

Public headers and subdirectories expose these console components:

- Console frontends and session context (`console.h`, `console_impl.h`,
  `console_noninteractive.h`, `console_context.h`): `zxdb::Console`,
  `zxdb::Console::OutputObserver`, `zxdb::ConsoleImpl`,
  `zxdb::ConsoleNoninteractive`, and `zxdb::ConsoleContext` (tracks active
  1-based target/thread/breakpoint IDs and 0-based frame indices).
- Command parsing, completion, and execution (`command.h`, `command_parser.h`,
  `command_context.h`, `command_sequence.h`, `command_line_options.h`):
  `zxdb::Command`, `zxdb::CommandCallback`, `zxdb::DispatchCommand`,
  `zxdb::CommandContext`, `zxdb::ConsoleCommandContext`,
  `zxdb::OfflineCommandContext`, and `zxdb::NestedCommandContext`. Also exposes
  `zxdb::CommandToken`, `zxdb::TokenizeCommand`, `zxdb::ParseCommand`,
  `zxdb::FillCommandContextCallback`, `zxdb::GetCommandCompletions`,
  `zxdb::RunCommandSequence`, `zxdb::ReadCommandsFromFile`,
  `zxdb::CommandLineOptions`, and `zxdb::ParseCommandLine`.
- Noun, verb, and switch registries (`nouns.h`, `verbs.h`, `switch_record.h`):
  `zxdb::Noun`, `zxdb::NounRecord`, `zxdb::GetNouns`, `zxdb::NounToString`,
  `zxdb::NounToRecord`, `zxdb::GetStringNounMap`, `zxdb::ExecuteNoun`,
  `zxdb::AppendNouns`, and `zxdb::GetNounSwitches`. Also exposes
  `zxdb::SourceAffinity`, `zxdb::Verb`, `zxdb::VerbRecord`, `zxdb::GetVerbs`,
  `zxdb::VerbToString`, `zxdb::GetVerbRecord`, `zxdb::GetStringVerbMap`,
  `zxdb::AppendSettingsVerbs`, and `zxdb::SwitchRecord`.
- Command validation and evaluation helpers (`command_utils.h`):
  `zxdb::AssertRunningTarget`, `zxdb::AssertStoppedThreadWithFrameCommand`,
  `zxdb::AssertAllStoppedThreadsCommand`, `zxdb::CheckHexPrefix`,
  `zxdb::ReadUint64Arg`, `zxdb::ThreadStateToString`,
  `zxdb::ExecutionScopeToString`, `zxdb::ExecutionScopeForCommand`,
  `zxdb::ResolveBreakpointsForModification`, `zxdb::FormatBreakpoint`,
  `zxdb::FormatInputLocation`, and `zxdb::FormatInputLocations`. Also exposes
  `zxdb::GetEvalContextForCommand`, `zxdb::EvalCommandExpression`,
  `zxdb::EvalCommandAddressExpression`, `zxdb::RewriteCommandExpressionError`,
  `zxdb::FormatConsoleString`, `zxdb::GetNumberFormatForTarget`,
  `zxdb::GetRunnableTarget`, `zxdb::GetRunningProcesses`,
  `zxdb::VerifySystemHasRunningProcess`, `zxdb::ProcessCommandCallback`, and
  `zxdb::ScheduleAsyncPrintReturnValue`.
- Input location parsing and resolution (`input_location_parser.h`):
  `zxdb::ParseGlobalInputLocation`, `zxdb::EvalGlobalInputLocation`,
  `zxdb::ParseLocalInputLocation`, `zxdb::EvalLocalInputLocation`,
  `zxdb::ResolveInputLocations`, `zxdb::ResolveUniqueInputLocation`, and
  `zxdb::CompleteInputLocation`.
- Stack, location, register, target, and table formatters (`format_frame.h`,
  `format_location.h`, `format_register.h`, `format_table.h`,
  `format_target.h`): `zxdb::FormatFrameOptions`, `zxdb::FormatStackOptions`,
  `zxdb::FormatStack`, `zxdb::FormatFrame`, `zxdb::FormatAllThreadStacks`,
  `zxdb::FormatLocationOptions`, `zxdb::FormatLocation`, `zxdb::FormatFileName`,
  and `zxdb::FormatFileLine`. Also exposes `zxdb::FormatRegisterOptions`,
  `zxdb::FormatRegisters`, `zxdb::FormatGeneralRegisters`,
  `zxdb::FormatGeneralVectorRegisters`, `zxdb::DescribeRegister`, `zxdb::Align`,
  `zxdb::ColSpec`, `zxdb::FormatTable`, `zxdb::FormatTarget`,
  `zxdb::FormatTargetList`, and `zxdb::TargetStateToString`.
- [commands](./commands/README.md): Individual console verb implementations
  (`attach`, `break`, `step`, `run`, `print`, `backtrace`, and memory/symbol
  inspection) exposing `VerbRecord` descriptors.

## Invariants

Callers and implementations must preserve these invariants:

- `Console` maintains a global singleton pointer accessible via
  `Console::get()`.
- `Console::output_enabled_`, `Console::streaming_enabled_`, and
  `ConsoleImpl::input_enabled_` act as balanced reference counters. Each feature
  is active only when its counter is greater than `0`.
- `Command::targets_` is never empty for valid commands, and `Command::target()`
  asserts `!targets_.empty()`.
- Single-object getters on `Command` (`frame()`, `thread()`, `breakpoint()`,
  `filter()`, `sym_server()`) return non-null only when the command has exactly
  one instance of that noun.
- `CommandContext` is reference-counted and must stay alive across asynchronous
  command execution. Pending `AsyncOutputBuffer` instances hold a
  `RefPtr<CommandContext>`, and the completion callback runs when
  `CommandContext` destructs.
- `ConsoleContext` assigns 1-based user IDs to targets, threads, breakpoints,
  filters, and symbol servers (`0` means none or invalid). Frame IDs use 0-based
  indices into the thread's backtrace.
- The active target in `ConsoleContext` always exists outside setup and
  teardown, whereas the active thread, breakpoint, filter, and symbol server may
  be null or `0`.
- `GetNouns()` and `GetVerbs()` return static maps that never change after
  initialization.

## Side Effects

`zxdb/console` produces these terminal, file I/O, and session side effects:

- `ConsoleImpl` watches `stdin` via `debug::MessageLoop`, writes formatted
  output to `stdout`, and reads and writes `$HOME/.zxdb_history`.
- `Console::Output` and `CommandContext::Output` register completion callbacks
  and hold `RefPtr` references to pending `AsyncOutputBuffer` instances until
  they finish.
- `ConsoleContext` observes `Session`, `System`, `Target`, `Process`, `Thread`,
  `Breakpoint`, `Download`, `Component`, and `SettingStore`. It updates active
  selections, evaluates display expressions on thread stop, and switches between
  `Embedded` and `EmbeddedInteractive` console modes.
- `RunCommandSequence` reads script files from disk and dispatches commands on
  `Console` in order until completion or the first error.
- `ScheduleAsyncPrintReturnValue` registers a `PostStopTask` on a stopped thread
  to evaluate and print function return values before normal stop handling.

## Verification

Run these commands to build and test `zxdb/console`:

- Build: `fx build //src/developer/debug/zxdb/console`
- Test: `fx test zxdb_tests`
