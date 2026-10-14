<!--* freshness: { reviewed: '2026-10-12' } *-->

# Output Formatting (`zxdb/format`)

## Overview

`//src/developer/debug/zxdb/format` provides synchronous and asynchronous text
formatting, syntax highlighting, identifier/function formatting, and
`FormatNode` tree rendering (Layer 4). It decouples styled output buffers
(`OutputBuffer` and `AsyncOutputBuffer`) from interactive `zxdb/console` session
state.

## Interface

Public headers expose these buffer and formatting primitives:

- `zxdb::OutputBuffer`, `zxdb::OutputBuffer::Span`: Styled text buffer of spans
  annotated with `Syntax`, `TextForegroundColor`, and `TextBackgroundColor`
  (`Append`, `FormatHelp`, `WriteToStdout`, `AsString`, `UnicodeCharWidth`,
  `TrimTrailingNewlines`, `Clear`, `GetDebugString`) (`output_buffer.h`).
- `zxdb::AsyncOutputBuffer`: Ref-counted hierarchical output buffer
  (`fxl::RefCountedThreadSafe<AsyncOutputBuffer>`) that coordinates asynchronous
  formatting (`Append`, `Complete`, `SetCompletionCallback`, `is_complete`,
  `DestructiveFlatten`) (`async_output_buffer.h`).
- `zxdb::FormatBufferOptions`, `zxdb::DescribeFormatTreeNode`,
  `zxdb::FormatTreeNode`, `zxdb::FormatValue`, `zxdb::FormatVariable`,
  `zxdb::FormatExpressions`: Value and expression formatters that render
  `FormatNode` trees into `OutputBuffer` or `AsyncOutputBuffer` instances
  (`format.h`).
- `zxdb::FormatIdentifierOptions`, `zxdb::FormatIdentifier`,
  `zxdb::FormatFunctionNameOptions`, `zxdb::FormatFunctionName`: Formatters for
  `Identifier`, `ParsedIdentifier`, and `Function` symbols (`format_name.h`).
- `zxdb::Syntax`, `zxdb::TextForegroundColor`, `zxdb::TextBackgroundColor`,
  `zxdb::SyntaxToString`, `zxdb::TextForegroundColorToString`,
  `zxdb::TextBackgroundColorToString`: Syntax and terminal color enums and
  string converters (`output_buffer.h`).
- `zxdb::GetCurrentRowMarker`, `zxdb::GetBreakpointMarker`,
  `zxdb::GetDisabledBreakpointMarker`, `zxdb::GetBullet`,
  `zxdb::GetExclamation`, `zxdb::GetRightArrow`, `zxdb::StringToBool`,
  `zxdb::BoolToString`, `zxdb::UnicodeCharWidth`: Console glyph helpers, boolean
  parsers, and UTF-8 display width calculators (`string_util.h`).

## Invariants

Callers and implementations must preserve these invariants:

- In `OutputBuffer::Span`, explicit `TextForegroundColor` and
  `TextBackgroundColor` values apply only when `syntax == Syntax::kNormal` and
  do not override `Syntax` styling.
- `OutputBuffer::operator==` and `OutputBuffer::GetDebugString` normalize spans
  by merging adjacent spans that share identical syntax, foreground, and
  background attributes.
- `AsyncOutputBuffer::is_complete()` returns `true` only after the caller calls
  `Complete()` (`marked_complete_ == true`) and all child buffers complete
  (`pending_resolution_ == 0`).
- `AsyncOutputBuffer` enforces `FX_DCHECK(!marked_complete_)` on every
  `Append()` and `Complete()` call. Callers must not append or complete again
  after calling `Complete()`.
- `AsyncOutputBuffer::SetCompletionCallback` enforces
  `FX_DCHECK(!is_complete())` and `FX_DCHECK(!cb || !completion_callback_)`.
  Callers can register a non-empty callback at most once, and the buffer clears
  `completion_callback_` before invoking it.
- `AsyncOutputBuffer` owns child buffers via `fxl::RefPtr` in a top-down tree,
  while child callbacks hold raw parent pointers to avoid cycles. Callers must
  keep the root `AsyncOutputBuffer` alive until completion.
- `AsyncOutputBuffer::DestructiveFlatten()` requires `FX_DCHECK(is_complete())`.
  Call it at most once because it moves spans out of the buffer tree.
- `DescribeFormatTreeNode` and `FormatTreeNode` cap tree depth at
  `FormatBufferOptions::max_depth` (default `16`, eliding deeper nodes with an
  ellipsis). They also cap pointer dereferences along any path at
  `FormatBufferOptions::pointer_expand_depth` (default `1`) to break pointer
  cycles.
- `FormatTreeNode`, `FormatValue`, `FormatVariable`, and `FormatExpressions`
  return formatted output without a trailing newline.
- `FormatIdentifier` wraps non-standard identifier components (those failing
  `ExprTokenizer::IsNameToken` for C and not starting with `operator`) in
  `$(...)`. It always escapes `$` and `\\`, and escapes `(` and `)` when
  parentheses inside the component do not balance.

## Side Effects

These formatting functions produce the following I/O and tree side effects:

- `OutputBuffer::WriteToStdout` writes ANSI escape sequences and span text to
  `stdout` via `fwrite` and flushes `stdout` via `std::fflush`.
- `DescribeFormatTreeNode`, `FormatValue`, `FormatVariable`, and
  `FormatExpressions` start asynchronous symbol and expression evaluation via
  `FillFormatNodeDescription` and `EvalContext::GetVariableValue`. They update
  `FormatNode` trees in place and complete the returned `AsyncOutputBuffer` when
  evaluation finishes.
- `AsyncOutputBuffer::DestructiveFlatten` moves `Span` elements out of the root
  `AsyncOutputBuffer` and all descendant buffers into the returned
  `OutputBuffer`.

## Verification

Run these commands to build and test `zxdb/format`:

- Build: `fx build //src/developer/debug/zxdb/format`
- Test: `fx test zxdb_tests`
