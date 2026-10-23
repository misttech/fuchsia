// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_ARCH_INCLUDE_LIB_ARCH_INTERNAL_ASM_H_
#define ZIRCON_KERNEL_LIB_ARCH_INCLUDE_LIB_ARCH_INTERNAL_ASM_H_

// These macros are internal implementation details of <lib/arch/asm.h>.

#ifdef __ASSEMBLER__  // clang-format off

// Generic: turn commas into newlines.
.macro _.progn first, rest:vararg
  \first
  .ifnb \rest
    _.progn \rest
  .endif
.endm

// Start a named list for use with _.list.accum and _.list.use, below.
.macro _.list.start name
  .macro _.list.invoke.\name invoke, rest:vararg
    \invoke \rest
  .endm
.endm

// Append one or more arguments to the named list.
.macro _.list.accum name, args:vararg
  .macro _.list.accum.\name accum_args:vararg
    .purgem _.list.invoke.\name
    .ifnb \accum_args
      .macro _.list.invoke.\name invoke, rest:vararg
        .ifnb \rest
          \invoke \accum_args, \args, \rest
        .else
          \invoke \accum_args, \args
        .endif
      .endm
    .else
      .macro _.list.invoke.\name invoke, rest:vararg
        .ifnb \rest
          \invoke \args, \rest
        .else
          \invoke \args
        .endif
      .endm
    .endif
  .endm
  _.list.invoke.\name _.list.accum.\name
  .purgem _.list.accum.\name
.endm

// After `_.list.start <name>`, `_.list.use <name>, <invoke>, <rest>...`
// evaluates `<invoke> <accumulated>..., <rest>...` where <accumulated>... is
// the concatenation of all the argument lists passed to `_.list.accum <name>`.
.macro _.list.use name, invoke:vararg
  _.list.invoke.\name \invoke
  .purgem _.list.invoke.\name
.endm

// Set ELF symbol visibility and binding, which represent scope.
.macro _.scope name, scope
  .ifnb \scope
    .ifnc \scope, local
      .ifc \scope, weak
        .weak \name
       .else
        .globl \name
        .ifc \scope, global
#ifdef __ELF__
          .hidden \name
#endif
        .else
          .ifnc \scope, export
            .error "`scope` argument `\scope` not `local`, `global`, `export`, or `weak`"
          .endif
        .endif
      .endif
    .endif
  .endif
.endm  // _.scope

// This macro is purged and redefined to indicate whether the assembly
// is currently between an entity-defining macro and its matching `.end_*`.
.macro _.entity.assert
  // Initial state: no entity being defined.
.endm

// Subroutine of _.entity.  Redefine the `.end_*` macros as errors.
.macro _.entity.reset purge=, text:vararg
  .ifnb \purge
    .purgem .end_function
    .purgem .end_object
  .endif
  .macro .end_function
    .error "unmatched .end_function directive \text"
  .endm  // .end_function
  .macro .end_object
    .error "unmatched .end_object directive \text"
  .endm  // .end_object
.endm  // _.entity.reset

// Subroutine of entity-defining macros below.  This resets the `.end_*`
// machinery so that it does `\epilogue` and then does `.size` for `\name`,
// via `_.entity.end`.  The \name, \scope, and \nosection arguments are
// passed through from caller and validated here; the rest are provided by
// each entity-defining macro.
.macro _.entity name, scope, align, nosection, retain, entity, type, epilogue:vararg
  // First make sure that entity and `.end_*` pairs are matched (no nesting).
  _.entity.assert
  .purgem _.entity.assert
  .macro _.entity.assert
    .error "missing `.end_\entity` for `.\entity \name`"
  .endm

  // Clear old `.end_*` macros and make the one other than `.end_\entity`
  // an error until this entity is finished.
  _.entity.reset purge, inside definition of `.\entity \name`

  // Now redefine the `.end_\entity` macro to pair with this entity.
  // This definition captures the parameters to pass on to `_.entity.end`.
  .purgem .end_\entity
  .macro .end_\entity
    _.entity.end \entity, \name, \nosection, \epilogue
  .endm

  // Unless given the `nosection` flag argument, enter a per-entity section.
  .ifb \nosection
    .ifnb \retain
      .ifnc \retain,R
        .error "retain argument to \entity directive must be `R` or empty"
      .endif
    .endif
    // The specific section name and details depend on the entity type.
    _.entity.pushsection.\type \name, \retain
  .else
    .ifnc \nosection, nosection
      .error "final argument to \entity directive must be exactly `nosection`"
    .endif
  .endif

  // Align within the section.
  .ifnb \align
    .balign \align
  .endif

  // Finally, define the actual label.  The entity's own prologue comes next.
  .label \name, \scope, \entity
.endm  // _.entity

// Subroutines of _.entity selected by the \type argument.

.macro _.entity.pushsection.function name, retain
#ifdef __ELF__
  // The function goes into the .text section in its own section group.
  // This lets any metadata associated with the function travel in its
  // group by using `.pushsection .metadata-section, "...?", ...`.
  .pushsection .text, "axG\retain", %progbits, \name
#else
  .text
#endif
.endm

.macro _.entity.pushsection.bss name, retain
#ifdef __ELF__
  .pushsection .bss.\name, "aw\retain", %nobits
#else
  .bss
#endif
.endm

.macro _.entity.pushsection.data name, retain
#ifdef __ELF__
  .pushsection .data.\name, "aw\retain", %progbits
#else
  .data
#endif
.endm

.macro _.entity.pushsection.relro name, retain
#ifdef __ELF__
  .pushsection .data.rel.ro.\name, "aw\retain", %progbits
#else
  .section .rdata, "dr"
#endif
.endm

.macro _.entity.pushsection.rodata name, retain
#ifdef __ELF__
  .pushsection .rodata.\name, "a\retain", %progbits
#else
  .section .rdata, "dr"
#endif
.endm

// Subroutine of `.end_\entity` macros defined by `_.entity`, above.
.macro _.entity.end entity, name, nosection, epilogue:vararg
  // First, reset the assertion machinery.
  .purgem _.entity.assert
  .macro _.entity.assert
  .endm

  // Now redefine `.end_*` so it's an error with no matching entity.
  _.entity.reset purge

  // Do the epilogue for the entity, e.g. .cfi_endproc.
  _.progn \epilogue

#ifdef __ELF__
  // Set the ELF symbol's `st_size`.
  .size \name, . - \name

 // Leave the per-entity section, if any.
  .ifb \nosection
    .popsection
  .endif
#endif
.endm  // _.entity.end

// Subroutines of .function, start/end pairs for each `cfi` mode.

.macro _.function.cfi.start.abi
  .cfi_startproc
#ifdef __arm__
  .fnstart
#endif
.endm
.macro _.function.cfi.end.abi
#ifdef __arm__
  .fnend
#endif
  .cfi_endproc
.endm

.macro _.function.cfi.start.custom
  .cfi_startproc simple
#ifdef __arm__
  .fnstart
#endif
.endm
.macro _.function.cfi.end.custom
#ifdef __arm__
  .fnend
#endif
  .cfi_endproc
.endm

.macro _.function.cfi.start.none
.endm
.macro _.function.cfi.end.none
.endm

.macro _.function.callgraph.start callgraph
  .ifb \callgraph
    .L._.llvm.callgraph = 0
  .else
    .L._.llvm.callgraph = 1
#ifdef __ELF__
    _.list.start llvm.callgraph.callee
    _.list.start llvm.callgraph.indirect_callee_type
    .ifc direct,\callgraph
      .L._.llvm.callgraph.type_id = 0
    .else
      .L._.llvm.callgraph.type_id = \callgraph
    .endif
    .L._.llvm.callgraph.callee.count = 0
    .L._.llvm.callgraph.indirect_callee_type.count = 0
#endif
  .endif
.endm

.macro _.llvm.callgraph.callee targets:vararg
#ifdef __ELF__
  .irp target,\targets
    _.list.accum llvm.callgraph.callee, \target
    .L._.llvm.callgraph.callee.count = .L._.llvm.callgraph.callee.count + 1
  .endr
#endif
.endm

.macro _.llvm.callgraph.indirect_callee_type types:vararg
#ifdef __ELF__
  .irp type,\types
    _.list.accum llvm.callgraph.indirect_callee_type, \type
    .L._.llvm.callgraph.indirect_callee_type.count = .L._.llvm.callgraph.indirect_callee_type.count + 1
  .endr
#endif
.endm

.macro _.function.callgraph.end
#ifdef __ELF__
  .ifne .L._.llvm.callgraph
    // Emit https://llvm.org/docs/CallGraphSection.html format.  Clang's
    // assembler will deduce the sh_type from the name (except that is broken
    // as of https://fxbug.dev/503904133) and will also accept a symbolic name
    // like %llvm_call_graph for it.  But GAS doesn't know this sh_type at all,
    // so encode it explicitly.
    .pushsection .llvm.callgraph, "?", %0x6fff4c0f // SHT_LLVM_CALL_GRAPH
    .byte 0  // Format version.
    .byte (((.L._.llvm.callgraph.type_id != 0) & 1) << 0) | \
          (((.L._.llvm.callgraph.callee.count != 0) & 1) << 1) | \
          (((.L._.llvm.callgraph.indirect_callee_type.count != 0) & 1) << 2)
    .dc.a .L._.function.entry
    .quad .L._.llvm.callgraph.type_id
    .ifne .L._.llvm.callgraph.callee.count
      .uleb128 .L._.llvm.callgraph.callee.count
      _.list.use llvm.callgraph.callee, .dc.a
    .else
      // Just consume the empty list.
      _.list.use llvm.callgraph.callee, _.progn
    .endif
    .ifne .L._.llvm.callgraph.indirect_callee_type.count
      .uleb128 .L._.llvm.callgraph.indirect_callee_type.count
      _.list.use llvm.callgraph.indirect_callee_type, .quad
    .else
      // Just consume the empty list.
      _.list.use llvm.callgraph.indirect_callee_type, _.progn
    .endif
    .popsection
  .endif
#endif
.endm

// Catch stray `.end_*` before any entity-defining macros.
_.entity.reset

#endif  // clang-format on

#endif  // ZIRCON_KERNEL_LIB_ARCH_INCLUDE_LIB_ARCH_INTERNAL_ASM_H_
