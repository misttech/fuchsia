<!--* freshness: { reviewed: '2026-10-12' } *-->

# Symbol Library (`zxdb/symbols`)

## Overview

`//src/developer/debug/zxdb/symbols` is the DWARF and ELF symbol indexing and
lookup subsystem (Layer 2) for `zxdb`. It decodes DWARF Debug Information
Entries (DIEs), line tables, and ELF symbols into a reference-counted object
graph, and evaluates DWARF location expressions (`DwarfExprEval`).

## Interface

Public headers expose these symbol classes, factories, and evaluators:

- `zxdb::Symbol` (`symbol.h`): Reference-counted base class
  (`fxl::RefCountedThreadSafe<Symbol>`) for DWARF DIE symbols with manual RTTI
  (`As<T>()`). Exposes `GetLazySymbol`, `GetDieRef`, `parent`,
  `GetAssignedName`, `GetFullName`, `GetIdentifier`, `GetCompileUnit`,
  `GetDwarfUnit`, `GetModuleSymbols`, `GetSymbolContext`, and `GetLanguage`.
- `zxdb::Type` (`type.h`, `collection.h`): Base type symbol (`StripCV`,
  `StripCVT`, `is_declaration`, `byte_size`). Subclasses include `BaseType`,
  `ModifiedType`, `Collection` (struct, class, union, and Rust enum or tuple),
  `ArrayType`, `FunctionType`, `Enumeration`, `MemberPtr`, `VariantPart`, and
  `Variant`.
- `zxdb::Value`, `zxdb::Variable`, `zxdb::DataMember`: Named data symbols with
  `type`, `is_external`, `artificial`, `const_value`, and `VariableLocation`
  metadata (`value.h`, `variable.h`, `data_member.h`).
- `zxdb::CodeBlock`, `zxdb::Function`, `zxdb::CompileUnit`: Lexical scopes,
  functions, inlined routines, and compilation units (`code_block.h`,
  `function.h`, `compile_unit.h`). Exposes address range queries (`code_ranges`,
  `GetAbsoluteCodeRanges`, `ContainsAddress`) and scope traversal
  (`GetMostSpecificChild`, `GetContainingFunction`, `GetInlineChain`,
  `GetAmbiguousInlineChain`).
- `zxdb::LazySymbolBase`, `zxdb::LazySymbol`, `zxdb::UncachedLazySymbol`,
  `zxdb::SymbolFactory`, `zxdb::DwarfSymbolFactory`: Lazy DIE reference wrappers
  and factories that decode `DwarfDieRef` entries into `Symbol` objects on
  demand (`lazy_symbol.h`, `symbol_factory.h`, `dwarf_symbol_factory.h`).
- `zxdb::SystemSymbols`, `zxdb::TargetSymbols`, `zxdb::ProcessSymbols`,
  `zxdb::ModuleSymbols`, `zxdb::ModuleSymbolsImpl`, `zxdb::LoadedModuleSymbols`:
  Symbol containers from global `BuildIDIndex` caches down to loaded modules
  with `SymbolContext` address translation (`system_symbols.h`,
  `target_symbols.h`, `process_symbols.h`, `module_symbols.h`,
  `loaded_module_symbols.h`).
- `zxdb::DwarfExpr`, `zxdb::DwarfExprEval`, `zxdb::SymbolDataProvider`:
  Asynchronous stack-based DWARF expression evaluator and target memory/register
  provider interface (`dwarf_expr.h`, `dwarf_expr_eval.h`,
  `symbol_data_provider.h`).
- Binary readers, symbol indices, line tables, and scope visitors:
  `zxdb::DwarfBinary`, `zxdb::DwarfUnit`, `zxdb::DwarfDieDecoder`,
  `zxdb::DwarfDieScanner`, `zxdb::ElfSymbol`, `zxdb::Index`, and
  `zxdb::IndexNode`. Also exposes `zxdb::Identifier`, `zxdb::Location`,
  `zxdb::InputLocation`, `zxdb::LineTable`, `zxdb::LineDetails`,
  `zxdb::VariableLocation`, and `zxdb::VisitScopes`.

## Invariants

Callers and implementations must preserve these invariants:

- `Symbol` objects use `fxl::RefCountedThreadSafe<Symbol>` and do not globally
  cache or deduplicate instances by DIE offset. Never compare `Symbol*` pointers
  to test semantic symbol equality.
- Downward edges in the symbol graph (parent to child) must use cached
  `LazySymbol` instances. Upward edges (such as `Symbol::parent()` and
  `Function::containing_block()`) must use `UncachedLazySymbol` to prevent
  reference-counting cycles and memory leaks.
- `SymbolFactory::CreateSymbol()`, `LazySymbol::Get()`, and
  `UncachedLazySymbol::Get()` never return `nullptr`. On failure or unloaded
  modules, they return an empty `Symbol` with `DwarfTag::kNone`.
- `Symbol`, `CodeBlock`, and `ModuleSymbols` store module-relative addresses.
  Pass a `SymbolContext` bound to a loaded module's base address to convert to
  or from absolute process addresses.
- `TargetSymbols::ResolveInputLocation` rejects address-based locations
  (`InputLocation::Type::kAddress`) because address resolution requires a
  running process via `ProcessSymbols`.
- Use `Symbol::As<Derived>()` instead of C++ `dynamic_cast` because the build
  disables compiler RTTI.
- DWARF unit offsets and type signatures of `0` are valid DWARF values
  (`DwarfDieRef`, `UnitSymbolFactory`, `DwarfSymbolFactory`). Never use `0` as
  an invalid sentinel. Use `std::optional<uint64_t>` and guard accessors with
  `DCHECK(is_valid())`.
- `DwarfSymbolFactory` handles `DwarfTag::kTypeUnit` alongside
  `DwarfTag::kCompileUnit` so child DIEs can reach `llvm::DWARFUnit` via
  `CompileUnit`. `DwarfBinaryImpl` queries both normal compilation units
  (`getNormalUnitsVector()`) and split-DWARF DWO units (`getDWOUnitsVector()`).
- When resolving a DWARF type signature whose target `DwarfDieRef` is invalid
  (`!sig_ref.is_valid()`), return an empty `fxl::RefPtr<Symbol>()` instead of
  crashing with `FX_CHECK`.

## Side Effects

Using `zxdb/symbols` produces these disk, cache, and IPC side effects:

- Calling `Get()` on a `LazySymbol` or `UncachedLazySymbol` decodes DWARF DIE
  records from the binary or `.dwo` file via `SymbolFactory` and caches
  `LazySymbol::symbol_`.
- Calling `Symbol::GetFullName()`, `Symbol::GetIdentifier()`, or
  `Collection::GetSpecialType()` lazily computes and caches the formatted name,
  `Identifier`, or `SpecialType` on the `Symbol`.
- `SystemSymbols::GetModule()` loads ELF and DWARF files from disk via
  `BuildIDIndex` and builds the symbol `Index`, or calls
  `RequestDownloadFunction` when local symbols are missing.
- `ProcessSymbols::SetModules()` and `RetryLoadBuildID()` load and unload module
  symbols, update `TargetSymbols`, and invoke `ProcessSymbols::Notifications`
  callbacks.
- `DwarfExprEval::Eval()` and `SymbolDataProvider` methods issue asynchronous
  register and memory reads and writes against the target process.

## Verification

Run these commands to build and test `zxdb/symbols`:

- Build: `fx build //src/developer/debug/zxdb/symbols`
- Test: `fx test zxdb_tests`
