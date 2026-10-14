<!--* freshness: { reviewed: '2026-10-12' } *-->

# Expression Evaluation (`zxdb/expr`)

`//src/developer/debug/zxdb/expr` parses and evaluates C, C++, and Rust console
expressions (Layer 3), such as expressions passed to `print`. Do not confuse
these high-level console expressions with low-level DWARF location expressions
in `zxdb/symbols`.

## Main Components

The expression evaluator consists of four stages:

- **Tokenizer** (`expr_tokenizer.cc`): Splits input text into a stream of
  `ExprToken` objects for the parser.
- **Parser** (`expr_parser.cc`): Parses the token stream into an abstract syntax
  tree (AST) of `ExprNode` objects.
- **Abstract Syntax Tree & Bytecode VM** (`expr_node.cc`, `vm_exec.cc`): Each
  `ExprNode` subclass emits bytecode (`EmitBytecode`) into a `VmStream`, which
  the stack-based virtual machine executes.
- **Expression Value** (`expr_value.cc`): `ExprValue` pairs a DWARF `Type`
  reference with raw bytes and an `ExprValueSource` origin descriptor.

## Integration with Symbols and Process State

AST nodes and the bytecode VM do not reference `ProcessSymbols` or `Process`
directly, which keeps expression evaluation unit-testable in isolation.

Instead, AST and VM operations query the abstract `EvalContext` interface. In
live sessions, `EvalContextImpl` implements `EvalContext` using `ProcessSymbols`
and a `SymbolDataProvider` backed by the target process.

## Helper Components

Additional files in this directory resolve names and types during evaluation:

- Name lookup (`find_name.*`, `found_*`): Resolves identifiers to variables,
  members, types, and functions across lexical blocks and module indices.
- Data resolution (`resolve_*`): Reads arrays, base and derived classes, struct
  members, pointers, references, and Rust enums/tuples.
- Identifier parsing (`parsed_identifier.*`, `template_type_extractor.*`):
  Parses qualified identifiers and extracts template parameter lists.

## Interface

Public headers expose these evaluation, parsing, VM, and formatting APIs:

- `zxdb::EvalExpression`, `zxdb::EvalExpressions`,
  `zxdb::ValueToAddressAndSize`, `zxdb::EvalCallback`: Top-level asynchronous
  expression evaluation entry points (`expr.h`, `eval_callback.h`).
- `zxdb::ExprTokenizer`, `zxdb::ExprToken`, `zxdb::ExprParser`,
  `zxdb::ExprLanguage`, `zxdb::DwarfLangToExprLanguage`,
  `zxdb::FileNameToLanguage`: Lexical tokenizer and recursive-descent parser for
  C/C++ (`ExprLanguage::kC`) and Rust (`ExprLanguage::kRust`)
  (`expr_tokenizer.h`, `expr_parser.h`, `expr_language.h`).
- `zxdb::ExprNode` (`expr_node.h`): Base AST node that emits `VmStream`
  bytecode. Subclasses include `AddressOfExprNode`, `ArrayAccessExprNode`,
  `BinaryOpExprNode`, `BlockExprNode`, `BreakExprNode`, `CastExprNode`,
  `ConditionExprNode`, `DereferenceExprNode`, `FunctionCallExprNode`,
  `IdentifierExprNode`, `LiteralExprNode`, `LocalVarExprNode`, `LoopExprNode`,
  `MemberAccessExprNode`, `SizeofExprNode`, `TypeExprNode`, `UnaryOpExprNode`,
  and `VariableDeclExprNode`.
- `zxdb::VmOp`, `zxdb::VmStream`, `zxdb::VmExec`, `zxdb::VmStreamToString`,
  `zxdb::VmBytecodeForwardJumper`, `zxdb::VmBytecodeForwardJump`,
  `zxdb::VmBytecodeForwardJumpIfFalse`, `zxdb::VmBytecodePushBreak`: Stack-based
  bytecode representation and asynchronous execution engine (`vm_op.h`,
  `vm_stream.h`, `vm_exec.h`).
- `zxdb::ExprValue`, `zxdb::ErrOrValue`, `zxdb::ErrOrValueVector`,
  `zxdb::ExprValueSource`: Typed value containers and origin descriptors
  (`kTemporary`, `kMemory`, `kRegister`, `kConstant`, `kComposite`, `kLocal`)
  (`expr_value.h`, `expr_value_source.h`).
- `zxdb::EvalContext`, `zxdb::EvalContextImpl`, `zxdb::Abi`, `zxdb::AbiX64`,
  `zxdb::AbiArm64`, `zxdb::AbiRiscv64`: Evaluation context and target ABI
  implementations (`eval_context.h`, `eval_context_impl.h`, `abi.h`).
- `zxdb::ParsedIdentifier`, `zxdb::ParsedIdentifierComponent`,
  `zxdb::FoundName`, `zxdb::FindNameContext`, `zxdb::FindNameOptions`,
  `zxdb::FindName`, `zxdb::VisitLocalVariables`, `zxdb::FindLocalVariable`,
  `zxdb::FindMember`, `zxdb::FindMemberOnThis`, `zxdb::FindIndexedName`,
  `zxdb::FindIndexedNameInModule`: Identifier types and symbol lookup routines
  (`parsed_identifier.h`, `found_name.h`, `find_name.h`).
- Data resolution and operator helpers (`resolve_*.h`, `eval_dwarf_expr.h`,
  `eval_operators.h`): `zxdb::ResolveArray`, `zxdb::ResolveArrayItem`,
  `zxdb::PromoteToDerived`, `zxdb::PromotePtrRefToDerived`,
  `zxdb::ResolveMember`, `zxdb::ResolveMemberByPointer`,
  `zxdb::ResolveInherited`, `zxdb::ResolvePointer`, and
  `zxdb::EnsureResolveReference`. Also exposes `zxdb::GetConcreteType`,
  `zxdb::FindTypeDefinition`, `zxdb::ResolveVariant`, `zxdb::DwarfExprToValue`,
  `zxdb::AsyncDwarfExprEval`, `zxdb::EvalBinaryOperator`, and
  `zxdb::EvalUnaryOperator`.
- `zxdb::FormatNode`, `zxdb::FormatOptions`, `zxdb::FillFormatNodeValue`,
  `zxdb::FillFormatNodeDescription`, `zxdb::PrettyType`,
  `zxdb::PrettyTypeManager`, `zxdb::PrettyStdString`, `zxdb::PrettyRustTuple`:
  Value formatting tree nodes and type pretty-printers (`format_node.h`,
  `format.h`, `pretty_type.h`, `pretty_type_manager.h`).

## Invariants

Callers and implementations must preserve these invariants:

- Every `ExprNode::EmitBytecode()` method must pop any temporary values it
  pushes and leave one `ExprValue` on the VM stack (statements push an empty
  `ExprValue`).
- When `VmStream` execution finishes without error, the VM stack holds one
  `ExprValue` containing the final result.
- `VmBytecodeForwardJumper` (and subclasses `VmBytecodeForwardJump`,
  `VmBytecodeForwardJumpIfFalse`, `VmBytecodePushBreak`) asserts on destruction
  if the caller registered a jump source without calling `JumpToHere()`.
- `VmOp::SetJumpDest()` asserts unless the opcode is `kJump`, `kJumpIfFalse`, or
  `kPushBreak`.
- `ExprValue::GetAs<T>()` asserts if the internal byte buffer size does not
  equal `sizeof(T)`.
- `ExprValueSource::SetBits()` requires `is_bitfield()` (`bit_size() != 0`) to
  be `true`.
- `TypeExprNode::concrete_type()` is never null. `GetConcreteType()` and
  `FindTypeDefinition()` return null only when the input type is null or does
  not resolve to a `Type`.
- Call `AsyncDwarfExprEval::Eval()` at most once per instance. The evaluator
  holds a self-reference until asynchronous execution finishes.
- `DwarfLangToExprLanguage()` maps all non-Rust DWARF languages to
  `ExprLanguage::kC`.

## Side Effects

Evaluating expressions produces these target and tree side effects:

- Reads target memory and registers asynchronously via `SymbolDataProvider`
  during bytecode execution, pointer dereferencing, member resolution, vtable
  lookup, and `FormatNode` evaluation.
- Writes target memory, CPU registers, or VM-local variables when evaluating
  assignments against an `ExprValue` with source `kMemory`, `kRegister`, or
  `kLocal`.
- Calls completion callbacks (`EvalCallback`, `fit::deferred_callback`)
  synchronously or asynchronously. `FillFormatNodeValue` and
  `FillFormatNodeDescription` always run their callback even if `FormatNode`
  destructs, whereas `EvalContext::GetNamedValue` drops its callback if
  `EvalContext` destructs first.
- Updates `FormatNode` state, value, description, error status, and child trees
  in place during `FillFormatNodeValue`, `FillFormatNodeDescription`, and
  `PrettyType::Format`.

## Verification

Run these commands to build and test `zxdb/expr`:

- Build: `fx build //src/developer/debug/zxdb/expr`
- Test: `fx test zxdb_tests`
