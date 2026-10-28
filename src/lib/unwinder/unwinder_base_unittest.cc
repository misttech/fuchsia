// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/unwinder_base.h"

#include <elf.h>

#include <cstring>
#include <utility>

#include <gtest/gtest.h>

#include "src/lib/unwinder/cfi_unwinder.h"
#include "src/lib/unwinder/dwarf_expr.h"
#include "src/lib/unwinder/elf_module_cache.h"
#include "src/lib/unwinder/testing/mock_memory.h"
#include "src/lib/unwinder/testing/mock_unwinder.h"

namespace unwinder {

namespace {

struct FakeElfWithDebugFrame {
  Elf64_Ehdr ehdr = {};
  Elf64_Phdr phdr = {};
  Elf64_Shdr shdr[2] = {};
  char strtab[32] = ".debug_frame\0.shstrtab";
  uint8_t debug_frame[128] = {};
};

// Populates an in-memory 64-bit ELF image containing a single .debug_frame CIE and FDE covering
// [load_addr + 0x1000, load_addr + 0x2000).
void InitFakeElfWithDebugFrame(FakeElfWithDebugFrame& fake, uint8_t code_alignment_factor,
                               uint8_t return_address_register, const uint8_t* fde_instructions,
                               size_t fde_instructions_size) {
  memcpy(fake.ehdr.e_ident, ELFMAG, SELFMAG);
  fake.ehdr.e_ident[EI_CLASS] = ELFCLASS64;
  fake.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  fake.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  fake.ehdr.e_type = ET_DYN;
  fake.ehdr.e_version = EV_CURRENT;
  fake.ehdr.e_phoff = offsetof(FakeElfWithDebugFrame, phdr);
  fake.ehdr.e_phnum = 1;
  fake.ehdr.e_phentsize = sizeof(Elf64_Phdr);
  fake.ehdr.e_shoff = offsetof(FakeElfWithDebugFrame, shdr);
  fake.ehdr.e_shnum = 2;
  fake.ehdr.e_shentsize = sizeof(Elf64_Shdr);
  fake.ehdr.e_shstrndx = 1;

  fake.phdr.p_type = PT_LOAD;
  fake.phdr.p_vaddr = 0x1000;
  fake.phdr.p_memsz = 0x1000;
  fake.phdr.p_flags = PF_R | PF_X;

  const uint8_t cie_hdr[] = {
      11,
      0,
      0,
      0,  // CIE length (11 bytes)
      0xff,
      0xff,
      0xff,
      0xff,                   // DWARF32 CIE ID
      4,                      // CIE version (.debug_frame = 4)
      0,                      // Empty augmentation string
      8,                      // Address size (8 bytes)
      0,                      // Segment selector size (0)
      code_alignment_factor,  // Code alignment factor (ULEB128)
      static_cast<uint8_t>(static_cast<uint32_t>(-8) &
                           0x7fU),  // Data alignment factor -8 (SLEB128)
      return_address_register,      // Return address register (ULEB128)
  };
  static_assert(sizeof(cie_hdr) == 15);

  const uint32_t fde_length = static_cast<uint32_t>(4 + 8 + 8 + fde_instructions_size);
  const uint32_t cie_offset = 0;
  const uint64_t pc_begin_rel = 0x1000;
  const uint64_t pc_range = 0x1000;

  size_t pos = 0;
  memcpy(fake.debug_frame + pos, cie_hdr, sizeof(cie_hdr));
  pos += sizeof(cie_hdr);
  memcpy(fake.debug_frame + pos, &fde_length, sizeof(fde_length));
  pos += sizeof(fde_length);
  memcpy(fake.debug_frame + pos, &cie_offset, sizeof(cie_offset));
  pos += sizeof(cie_offset);
  memcpy(fake.debug_frame + pos, &pc_begin_rel, sizeof(pc_begin_rel));
  pos += sizeof(pc_begin_rel);
  memcpy(fake.debug_frame + pos, &pc_range, sizeof(pc_range));
  pos += sizeof(pc_range);
  memcpy(fake.debug_frame + pos, fde_instructions, fde_instructions_size);
  pos += fde_instructions_size;

  fake.shdr[0].sh_name = 0;
  fake.shdr[0].sh_type = SHT_PROGBITS;
  fake.shdr[0].sh_offset = offsetof(FakeElfWithDebugFrame, debug_frame);
  fake.shdr[0].sh_size = pos;

  fake.shdr[1].sh_name = 13;
  fake.shdr[1].sh_type = SHT_STRTAB;
  fake.shdr[1].sh_offset = offsetof(FakeElfWithDebugFrame, strtab);
  fake.shdr[1].sh_size = sizeof(fake.strtab);
}

class UnwinderBaseTest : public ::testing::Test {
 public:
  UnwinderBaseTest() : module_cache_({}), mock_unwinder_(module_cache_) {}

 protected:
  ElfModuleCache module_cache_;
  MockUnwinder mock_unwinder_;
  MockMemory mock_memory_;
};

TEST_F(UnwinderBaseTest, TryUnwinderSuccess) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  Frame next_expected(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  next_expected.regs.SetPC(0x2000);
  mock_unwinder_.SetFrames({next_expected});

  Frame next(Registers(Registers::Arch::kX64), true, Frame::Trust::kCFI);
  auto err = TryUnwinder(&mock_unwinder_, &mock_memory_, current, next);

  EXPECT_TRUE(err.ok());
  uint64_t pc;
  EXPECT_TRUE(next.regs.GetPC(pc).ok());
  EXPECT_EQ(0x2000UL, pc);
  EXPECT_EQ(Frame::Trust::kCFI, next.trust);
}

TEST_F(UnwinderBaseTest, TryUnwinderError) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  Frame next(Registers(Registers::Arch::kX64), true, Frame::Trust::kCFI);

  mock_unwinder_.SetStepError(Error("Failed"));

  auto err = TryUnwinder(&mock_unwinder_, &mock_memory_, current, next);
  EXPECT_TRUE(err.has_err());
}

TEST_F(UnwinderBaseTest, UnwindLoop) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  regs.SetSP(0x10000);

  Frame frame1(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  frame1.regs.SetPC(0x2000);
  mock_unwinder_.SetFrames({frame1});

  auto frames = mock_unwinder_.Unwind(nullptr, regs);

  ASSERT_EQ(2UL, frames.size());
  uint64_t pc;
  frames[0].regs.GetPC(pc);
  EXPECT_EQ(0x1000UL, pc);
  frames[1].regs.GetPC(pc);
  EXPECT_EQ(0x2000UL, pc);
}

TEST_F(UnwinderBaseTest, AsyncUnwindLoop) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  regs.SetSP(0x10000);

  MockAsyncMemoryDelegate delegate;
  AsyncMemory async_memory(&delegate);

  Frame frame1(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  frame1.regs.SetPC(0x2000);
  mock_unwinder_.SetFrames({frame1});

  bool done = false;
  mock_unwinder_.AsyncUnwind(&async_memory, regs, 50, [&](std::vector<Frame> frames) {
    ASSERT_EQ(2UL, frames.size());
    uint64_t pc;
    frames[0].regs.GetPC(pc);
    EXPECT_EQ(0x1000UL, pc);
    frames[1].regs.GetPC(pc);
    EXPECT_EQ(0x2000UL, pc);
    done = true;
  });

  EXPECT_TRUE(done);
}

TEST_F(UnwinderBaseTest, TryUnwinderSignalFrame) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  Frame next_expected(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  next_expected.regs.SetPC(0x2000);
  next_expected.is_signal_frame = true;
  mock_unwinder_.SetFrames({next_expected});

  Frame next(Registers(Registers::Arch::kX64), true, Frame::Trust::kCFI);
  auto err = TryUnwinder(&mock_unwinder_, &mock_memory_, current, next);
  EXPECT_TRUE(err.ok());
  EXPECT_TRUE(next.is_signal_frame);
  EXPECT_FALSE(next.pc_is_return_address);
}

TEST_F(UnwinderBaseTest, TryUnwinderPcIsReturnAddress) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  Frame next_expected(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  next_expected.regs.SetPC(0x2000);
  // Do NOT set rax.
  mock_unwinder_.SetFrames({next_expected});

  Frame next(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  auto err = TryUnwinder(&mock_unwinder_, &mock_memory_, current, next);
  EXPECT_TRUE(err.ok());
  EXPECT_TRUE(next.pc_is_return_address);
}

TEST_F(UnwinderBaseTest, TryUnwinderPcIsNOTReturnAddress) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  Frame next_expected(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  next_expected.regs.SetPC(0x2000);
  next_expected.regs.Set(RegisterID::kX64_rax, 0);  // Set rax.
  mock_unwinder_.SetFrames({next_expected});

  Frame next(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  auto err = TryUnwinder(&mock_unwinder_, &mock_memory_, current, next);
  EXPECT_TRUE(err.ok());
  EXPECT_FALSE(next.pc_is_return_address);
}

TEST_F(UnwinderBaseTest, TryAsyncUnwinderSuccess) {
  Registers regs(Registers::Arch::kX64);
  regs.SetPC(0x1000);
  Frame current(regs, false, Frame::Trust::kContext);

  MockAsyncMemoryDelegate delegate;
  AsyncMemory async_memory(&delegate);

  Frame next_expected(Registers(Registers::Arch::kX64), false, Frame::Trust::kCFI);
  next_expected.regs.SetPC(0x2000);
  mock_unwinder_.SetFrames({next_expected});

  bool done = false;
  TryAsyncUnwinder(&mock_unwinder_, &async_memory, current, [&](Error err, Frame next) {
    EXPECT_TRUE(err.ok());
    uint64_t pc;
    EXPECT_TRUE(next.regs.GetPC(pc).ok());
    EXPECT_EQ(0x2000UL, pc);
    EXPECT_EQ(Frame::Trust::kCFI, next.trust);
    done = true;
  });

  EXPECT_TRUE(done);
}

TEST_F(UnwinderBaseTest, SetTrust) {
  mock_unwinder_.SetTrust(Frame::Trust::kFP);
  EXPECT_EQ(Frame::Trust::kFP, mock_unwinder_.trust());
}

TEST_F(UnwinderBaseTest, CfiUnwinderAsyncStepArm64ValExpressionDoesNotFetchMemory) {
  // Construct ARM64 CFI instructions with:
  //   DW_CFA_def_cfa: sp (31) + 32
  //   DW_CFA_offset: x29 (fp) at cfa - 32 (4 * -8)
  //   DW_CFA_offset: x30 (lr) at cfa - 24 (3 * -8)
  //   DW_CFA_val_expression: x18 (scs) = x18 - 8 (DW_OP_breg18, -8)
  const uint8_t cfi_instructions[] = {
      0x0c,
      31,
      32,  // DW_CFA_def_cfa sp, 32
      0x80U | 29U,
      4,  // DW_CFA_offset x29, 4 * -8
      0x80U | 30U,
      3,  // DW_CFA_offset x30, 3 * -8
      0x16,
      18,
      2,
      DW_OP_breg18,
      static_cast<uint8_t>(static_cast<uint32_t>(-8) &
                           0x7fU),  // DW_CFA_val_expression x18, {breg18 - 8}
  };

  FakeElfWithDebugFrame fake_elf;
  InitFakeElfWithDebugFrame(fake_elf, 4, std::to_underlying(RegisterID::kArm64_lr),
                            cfi_instructions, sizeof(cfi_instructions));
  LocalMemory elf_mem;
  const uint64_t load_addr = reinterpret_cast<uint64_t>(&fake_elf);
  std::vector<Module> modules = {
      Module(load_addr, &elf_mem, &elf_mem, Module::AddressMode::kFile),
  };
  ElfModuleCache module_cache(modules);
  CfiUnwinder cfi_unwinder(module_cache);

  Registers current(Registers::Arch::kArm64);
  ASSERT_TRUE(current.SetSP(0x1000).ok());
  ASSERT_TRUE(current.SetPC(load_addr + 0x1010).ok());
  ASSERT_TRUE(current.SetSCS(0x20000008).ok());

  // Populate only the stack frame at [0x1000, 0x1010). Do not populate the shadow call stack at
  // 0x20000000 because DW_CFA_val_expression computes the register value directly without
  // dereferencing memory.
  MockAsyncMemoryDelegate delegate;
  uint64_t saved_fp_and_lr[2] = {0x1080, load_addr + 0x1050};
  std::vector<uint8_t> stack_bytes(sizeof(saved_fp_and_lr));
  memcpy(stack_bytes.data(), saved_fp_and_lr, sizeof(saved_fp_and_lr));
  delegate.AddMemory(0x1000, std::move(stack_bytes));

  AsyncMemory async_stack(&delegate);
  bool done = false;
  Frame current_frame(current, false, Frame::Trust::kContext);
  cfi_unwinder.AsyncStep(&async_stack, current_frame, [&](Error err, Registers next) {
    ASSERT_TRUE(err.ok()) << err.msg();
    uint64_t sp = 0;
    uint64_t fp = 0;
    uint64_t pc = 0;
    uint64_t scs = 0;
    EXPECT_TRUE(next.GetSP(sp).ok());
    EXPECT_TRUE(next.GetFP(fp).ok());
    EXPECT_TRUE(next.GetPC(pc).ok());
    EXPECT_TRUE(next.GetSCS(scs).ok());
    EXPECT_EQ(0x1020UL, sp);
    EXPECT_EQ(0x1080UL, fp);
    EXPECT_EQ(load_addr + 0x1050, pc);
    EXPECT_EQ(0x20000000UL, scs);
    done = true;
  });

  EXPECT_TRUE(done);
  EXPECT_EQ(1UL, delegate.fetch_calls());
  EXPECT_EQ(0UL, delegate.unfetched_read_calls());
  ASSERT_EQ(2UL, delegate.fetched_ranges().size());
  EXPECT_EQ(std::make_pair(uint64_t{0x1000}, uint32_t{8}), delegate.fetched_ranges()[0]);
  EXPECT_EQ(std::make_pair(uint64_t{0x1008}, uint32_t{8}), delegate.fetched_ranges()[1]);
}

TEST_F(UnwinderBaseTest, CfiUnwinderAsyncStepExpressionSkipsFailedEval) {
  // Construct x64 CFI instructions with:
  //   DW_CFA_def_cfa: rsp (7) + 16
  //   DW_CFA_offset: rip (16) at cfa - 8 (1 * -8)
  //   DW_CFA_expression: rbp (6) at address (rsp + 0) via {DW_OP_breg7, 0}
  //   DW_CFA_expression: rbx (3) via {DW_OP_breg3, 0} where rbx is unset in current registers
  const uint8_t cfi_instructions[] = {
      0x0c,        7, 16,                  // DW_CFA_def_cfa rsp, 16
      0x80U | 16U, 1,                      // DW_CFA_offset rip, 1 * -8
      0x10,        6, 2,  DW_OP_breg7, 0,  // DW_CFA_expression rbp, {breg7 + 0}
      0x10,        3, 2,  DW_OP_breg3, 0,  // DW_CFA_expression rbx, {breg3 + 0} (fails evaluation)
  };

  FakeElfWithDebugFrame fake_elf;
  InitFakeElfWithDebugFrame(fake_elf, 1, std::to_underlying(RegisterID::kX64_rip), cfi_instructions,
                            sizeof(cfi_instructions));
  LocalMemory elf_mem;
  const uint64_t load_addr = reinterpret_cast<uint64_t>(&fake_elf);
  std::vector<Module> modules = {
      Module(load_addr, &elf_mem, &elf_mem, Module::AddressMode::kFile),
  };
  ElfModuleCache module_cache(modules);
  CfiUnwinder cfi_unwinder(module_cache);

  Registers current(Registers::Arch::kX64);
  ASSERT_TRUE(current.SetSP(0x2000).ok());
  ASSERT_TRUE(current.SetPC(load_addr + 0x1010).ok());

  MockAsyncMemoryDelegate delegate;
  uint64_t saved_rbp_and_rip[2] = {0x2080, load_addr + 0x1099};
  std::vector<uint8_t> stack_bytes(sizeof(saved_rbp_and_rip));
  memcpy(stack_bytes.data(), saved_rbp_and_rip, sizeof(saved_rbp_and_rip));
  delegate.AddMemory(0x2000, std::move(stack_bytes));

  AsyncMemory async_stack(&delegate);
  bool done = false;
  Frame current_frame(current, false, Frame::Trust::kContext);
  cfi_unwinder.AsyncStep(&async_stack, current_frame, [&](Error err, Registers next) {
    ASSERT_TRUE(err.ok()) << err.msg();
    uint64_t sp = 0;
    uint64_t fp = 0;
    uint64_t pc = 0;
    EXPECT_TRUE(next.GetSP(sp).ok());
    EXPECT_TRUE(next.GetFP(fp).ok());
    EXPECT_TRUE(next.GetPC(pc).ok());
    EXPECT_EQ(0x2010UL, sp);
    EXPECT_EQ(0x2080UL, fp);
    EXPECT_EQ(load_addr + 0x1099, pc);
    done = true;
  });

  EXPECT_TRUE(done);
  EXPECT_EQ(1UL, delegate.fetch_calls());
  EXPECT_EQ(0UL, delegate.unfetched_read_calls());
  // Only the two valid locations (rbp at 0x2000 and rip at 0x2008) should be requested.
  ASSERT_EQ(2UL, delegate.fetched_ranges().size());
  EXPECT_EQ(std::make_pair(uint64_t{0x2000}, uint32_t{8}), delegate.fetched_ranges()[0]);
  EXPECT_EQ(std::make_pair(uint64_t{0x2008}, uint32_t{8}), delegate.fetched_ranges()[1]);
}

}  // namespace

}  // namespace unwinder
