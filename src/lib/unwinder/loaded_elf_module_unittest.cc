// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/loaded_elf_module.h"

#include <elf.h>

#include <cstring>

#include <gtest/gtest.h>

#include "src/lib/unwinder/memory.h"

namespace unwinder {

namespace {

struct FakeElf64Module {
  Elf64_Ehdr ehdr;
  Elf64_Phdr phdr[3];
};

struct FakeElf32Module {
  Elf32_Ehdr ehdr;
  Elf32_Phdr phdr[3];
};

struct FakeElf64ModuleSingleSegment {
  Elf64_Ehdr ehdr;
  Elf64_Phdr phdr;
};

struct FakeElf32ModuleSingleSegment {
  Elf32_Ehdr ehdr;
  Elf32_Phdr phdr;
};

TEST(LoadedElfModule, Load64Bit) {
  FakeElf64Module fake;
  memset(&fake, 0, sizeof(fake));

  // Set up 64-bit ELF header
  memcpy(fake.ehdr.e_ident, ELFMAG, SELFMAG);
  fake.ehdr.e_ident[EI_CLASS] = ELFCLASS64;
  fake.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  fake.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  fake.ehdr.e_type = ET_DYN;
  fake.ehdr.e_machine = EM_X86_64;
  fake.ehdr.e_version = EV_CURRENT;
  fake.ehdr.e_phoff = offsetof(FakeElf64ModuleSingleSegment, phdr);
  fake.ehdr.e_phnum = 3;
  fake.ehdr.e_phentsize = sizeof(Elf64_Phdr);

  // Set up a PT_LOAD segment
  fake.phdr[0].p_type = PT_LOAD;
  fake.phdr[0].p_vaddr = 0x1000;
  fake.phdr[0].p_memsz = 0x2000;
  fake.phdr[0].p_flags = PF_R | PF_X;

  fake.phdr[1].p_type = PT_LOAD;
  fake.phdr[1].p_vaddr = 0x3000;
  fake.phdr[1].p_memsz = 0x2000;
  fake.phdr[1].p_flags = PF_R | PF_X;

  fake.phdr[2].p_type = PT_LOAD;
  fake.phdr[2].p_vaddr = 0x5000;
  fake.phdr[2].p_memsz = 0x2000;
  fake.phdr[2].p_flags = PF_R | PF_X;

  LocalMemory mem;
  uint64_t load_addr = reinterpret_cast<uint64_t>(&fake);
  Module module(load_addr, &mem, Module::AddressMode::kProcess);

  LoadedElfModule loaded(module);
  auto result = loaded.Load();
  ASSERT_TRUE(result.is_ok()) << result.error_value().msg();

  EXPECT_EQ(loaded.load_address(), load_addr);
  EXPECT_EQ(loaded.phdrs().size(), 3u);
  EXPECT_EQ(loaded.size(), Module::AddressSize::k64Bit);

  // IsValidPC should check (load_addr + p_vaddr) to (load_addr + p_vaddr + p_memsz)
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x1000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x2fff));
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x0fff));

  // Valid inside of the second phdr.
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x3000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x3500));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x4000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x4fff));

  // Valid inside of the third phdr.
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x5000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x5500));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x6000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x6fff));

  // Outside of the range of the last.
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x7000));
}

TEST(LoadedElfModule, Load64BitOnePhdr) {
  FakeElf64ModuleSingleSegment fake;
  memset(&fake, 0, sizeof(fake));

  // Set up 64-bit ELF header
  memcpy(fake.ehdr.e_ident, ELFMAG, SELFMAG);
  fake.ehdr.e_ident[EI_CLASS] = ELFCLASS64;
  fake.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  fake.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  fake.ehdr.e_type = ET_DYN;
  fake.ehdr.e_machine = EM_X86_64;
  fake.ehdr.e_version = EV_CURRENT;
  fake.ehdr.e_phoff = offsetof(FakeElf64ModuleSingleSegment, phdr);
  fake.ehdr.e_phnum = 1;
  fake.ehdr.e_phentsize = sizeof(Elf64_Phdr);

  // Set up a PT_LOAD segment
  fake.phdr.p_type = PT_LOAD;
  fake.phdr.p_vaddr = 0x1000;
  fake.phdr.p_memsz = 0x2000;
  fake.phdr.p_flags = PF_R | PF_X;

  LocalMemory mem;
  uint64_t load_addr = reinterpret_cast<uint64_t>(&fake);
  Module module(load_addr, &mem, Module::AddressMode::kProcess);

  LoadedElfModule loaded(module);
  auto result = loaded.Load();
  ASSERT_TRUE(result.is_ok()) << result.error_value().msg();

  EXPECT_EQ(loaded.load_address(), load_addr);
  EXPECT_EQ(loaded.size(), Module::AddressSize::k64Bit);

  // IsValidPC should check (load_addr + p_vaddr) to (load_addr + p_vaddr + p_memsz)
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x1000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x2fff));
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x0fff));
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x3000));
}

TEST(LoadedElfModule, Load32Bit) {
  FakeElf32Module fake;
  memset(&fake, 0, sizeof(fake));

  // Set up 32-bit ELF header
  memcpy(fake.ehdr.e_ident, ELFMAG, SELFMAG);
  fake.ehdr.e_ident[EI_CLASS] = ELFCLASS32;
  fake.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  fake.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  fake.ehdr.e_type = ET_DYN;
  fake.ehdr.e_machine = EM_ARM;
  fake.ehdr.e_version = EV_CURRENT;
  fake.ehdr.e_phoff = offsetof(FakeElf32ModuleSingleSegment, phdr);
  fake.ehdr.e_phnum = 3;
  fake.ehdr.e_phentsize = sizeof(Elf32_Phdr);

  // Set up a PT_LOAD segment
  fake.phdr[0].p_type = PT_LOAD;
  fake.phdr[0].p_vaddr = 0x1000;
  fake.phdr[0].p_memsz = 0x2000;

  fake.phdr[1].p_type = PT_LOAD;
  fake.phdr[1].p_vaddr = 0x3000;
  fake.phdr[1].p_memsz = 0x2000;

  fake.phdr[2].p_type = PT_LOAD;
  fake.phdr[2].p_vaddr = 0x5000;
  fake.phdr[2].p_memsz = 0x2000;

  LocalMemory mem;
  uint64_t load_addr = reinterpret_cast<uint64_t>(&fake);
  Module module(load_addr, &mem, Module::AddressMode::kProcess);

  LoadedElfModule loaded(module);
  auto result = loaded.Load();
  ASSERT_TRUE(result.is_ok()) << result.error_value().msg();

  EXPECT_EQ(loaded.load_address(), load_addr);
  EXPECT_EQ(loaded.size(), Module::AddressSize::k32Bit);
  EXPECT_EQ(loaded.phdrs().size(), 3u);

  // Verify upcasting
  EXPECT_EQ(loaded.phdrs()[0].p_type, static_cast<uint32_t>(PT_LOAD));
  EXPECT_EQ(loaded.phdrs()[0].p_vaddr, 0x1000u);
  EXPECT_EQ(loaded.phdrs()[0].p_memsz, 0x2000u);
  EXPECT_EQ(loaded.phdrs()[1].p_type, static_cast<uint32_t>(PT_LOAD));
  EXPECT_EQ(loaded.phdrs()[1].p_vaddr, 0x3000u);
  EXPECT_EQ(loaded.phdrs()[1].p_memsz, 0x2000u);
  EXPECT_EQ(loaded.phdrs()[2].p_type, static_cast<uint32_t>(PT_LOAD));
  EXPECT_EQ(loaded.phdrs()[2].p_vaddr, 0x5000u);
  EXPECT_EQ(loaded.phdrs()[2].p_memsz, 0x2000u);

  // Valid PCs don't start until the beginning of the first loaded segment.
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x0fff));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x1500));

  // Valid inside of the second phdr.
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x3000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x3500));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x4000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x4fff));

  // Valid inside of the third phdr.
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x5000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x5500));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x6000));
  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x6fff));

  // Outside of the range of the last.
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x7000));
}

TEST(LoadedElfModule, Load32BitOnePhdr) {
  FakeElf32ModuleSingleSegment fake;
  memset(&fake, 0, sizeof(fake));

  // Set up 32-bit ELF header
  memcpy(fake.ehdr.e_ident, ELFMAG, SELFMAG);
  fake.ehdr.e_ident[EI_CLASS] = ELFCLASS32;
  fake.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
  fake.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
  fake.ehdr.e_type = ET_DYN;
  fake.ehdr.e_machine = EM_ARM;
  fake.ehdr.e_version = EV_CURRENT;
  fake.ehdr.e_phoff = offsetof(FakeElf32ModuleSingleSegment, phdr);
  fake.ehdr.e_phnum = 1;
  fake.ehdr.e_phentsize = sizeof(Elf32_Phdr);

  // Set up a PT_LOAD segment
  fake.phdr.p_type = PT_LOAD;
  fake.phdr.p_vaddr = 0x1000;
  fake.phdr.p_memsz = 0x2000;

  LocalMemory mem;
  uint64_t load_addr = reinterpret_cast<uint64_t>(&fake);
  Module module(load_addr, &mem, Module::AddressMode::kProcess);

  LoadedElfModule loaded(module);
  auto result = loaded.Load();
  ASSERT_TRUE(result.is_ok()) << result.error_value().msg();

  EXPECT_EQ(loaded.size(), Module::AddressSize::k32Bit);
  EXPECT_EQ(loaded.phdrs().size(), 1u);
  // Verify upcasting
  EXPECT_EQ(loaded.phdrs()[0].p_type, static_cast<uint32_t>(PT_LOAD));
  EXPECT_EQ(loaded.phdrs()[0].p_vaddr, 0x1000u);
  EXPECT_EQ(loaded.phdrs()[0].p_memsz, 0x2000u);

  EXPECT_TRUE(loaded.IsValidPC(load_addr + 0x1500));
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x0fff));
  EXPECT_FALSE(loaded.IsValidPC(load_addr + 0x3000));
}

TEST(LoadedElfModule, InvalidHeader) {
  FakeElf64ModuleSingleSegment fake;
  memset(&fake, 0, sizeof(fake));
  // Wrong magic
  memcpy(fake.ehdr.e_ident, "NOTELF", 6);

  LocalMemory mem;
  uint64_t load_addr = reinterpret_cast<uint64_t>(&fake);
  // Module constructor might fail or ProbeElfModuleClass returns kUnknown
  Module module(load_addr, &mem, Module::AddressMode::kProcess);

  LoadedElfModule loaded(module);
  auto result = loaded.Load();
  EXPECT_TRUE(result.is_error());
}

}  // namespace

}  // namespace unwinder
