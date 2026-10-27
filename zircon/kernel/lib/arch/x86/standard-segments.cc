// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/x86/descriptor-regs.h>
#include <lib/arch/x86/standard-segments.h>

namespace arch {

void X86StandardSegments::Init() {
  gdt_.code64.MakeCode64();  // Initialize code segment.

  gdt_.tss64  // Initialize TSS.
      .set_present(true)
      .set_type(arch::SystemSegmentDesc64::SegmentType::TSS_AVAILABLE)
      .set_base(reinterpret_cast<uintptr_t>(&tss_))
      .set_limit(sizeof(tss_) - 1);
}

arch::GdtRegister64 X86StandardSegments::gdt_pointer() {
  return GdtRegister64::Make(std::span{&gdt_, 1});
}

void X86StandardSegments::Load() {
  // Initialize the tables.
  Init();

  // Clear to a safely invalid LDT so any segment selectors with the LDT bit
  // set will cause a GPF rather than looking at any bogus table contents.
  arch::DisableLdt();

  // Install the new GDT.
  arch::LoadGdt({gdt_pointer()});

  // Switch to the code segment descriptor in the new GDT.
  arch::LoadCodeSegmentSelector(kCs64);

  // Install the new Task State Segment.
  arch::LoadTaskRegister64(kTr64);
}

void X86StandardSegments::Load(uintptr_t entry, uintptr_t arg) {
  // Initialize the tables.
  Init();

  // Clear to a safely invalid LDT so any segment selectors with the LDT bit
  // set will cause a GPF rather than looking at any bogus table contents.
  arch::DisableLdt();

  // Install the new GDT.
  arch::LoadGdt({gdt_pointer()});

  // Install the new Task State Segment.
  arch::LoadTaskRegister64(kTr64);

  // Clear %ss in case it refers to an old segment that's now invalid.
  __asm__ volatile("mov %0, %%ss" : : [zero] "r"(0));

  // Do a far jump via far return since AMD processors don't handle 64-bit
  // offsets in ljmpq.  It's OK that this clobbers the stack because it never
  // returns anyway.  The frame pointer is cleared to avoid leaving any
  // misleading breadcrumbs for the new code.
  __asm__ volatile(
#ifdef __ELF__
      R"""(
      push %[cs]
      .cfi_adjust_cfa_offset 8
      push %[pc]
      .cfi_adjust_cfa_offset 8
      xor %%ebp, %%ebp
      lretq
      .cfi_adjust_cfa_offset -16
      )"""
#else
      R"""(
      push %[cs]
      push %[pc]
      xor %%ebp, %%ebp
      lretq
      )"""
#endif
      :
      : [cs] "ir"(kCs64), [pc] "ir"(entry),  // lretq
        "S"(arg)                             // %rsi
      : "cc", "memory");

  __builtin_unreachable();
}

}  // namespace arch
