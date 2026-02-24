// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/loaded_elf_module.h"

#include <algorithm>
#include <cstring>
#include <iterator>
#include <type_traits>

#include <safemath/safe_math.h>

#include "src/lib/unwinder/elf_utils.h"
#include "src/lib/unwinder/module.h"

namespace unwinder {

namespace {

template <typename Ehdr>
fit::result<Error, Elf64_Ehdr> ReadEhdr(Memory* memory, uint64_t load_address) {
  Ehdr ehdr;
  if (auto err = memory->Read(load_address, ehdr); err.has_err()) {
    return fit::error(err);
  }

  Elf64_Ehdr ehdr64;
  if constexpr (std::is_same_v<Ehdr, Elf32_Ehdr>) {
    memcpy(ehdr64.e_ident, ehdr.e_ident, EI_NIDENT);
    ehdr64.e_type = ehdr.e_type;
    ehdr64.e_machine = ehdr.e_machine;
    ehdr64.e_version = ehdr.e_version;
    ehdr64.e_entry = ehdr.e_entry;
    ehdr64.e_phoff = ehdr.e_phoff;
    ehdr64.e_shoff = ehdr.e_shoff;
    ehdr64.e_flags = ehdr.e_flags;
    ehdr64.e_ehsize = ehdr.e_ehsize;
    ehdr64.e_phentsize = ehdr.e_phentsize;
    ehdr64.e_phnum = ehdr.e_phnum;
    ehdr64.e_shentsize = ehdr.e_shentsize;
    ehdr64.e_shnum = ehdr.e_shnum;
    ehdr64.e_shstrndx = ehdr.e_shstrndx;
  } else {
    ehdr64 = ehdr;
  }

  return fit::ok(ehdr64);
}

Elf64_Phdr UpcastPhdr(const Elf32_Phdr& phdr32) {
  Elf64_Phdr phdr;
  phdr.p_type = phdr32.p_type;
  phdr.p_offset = phdr32.p_offset;
  phdr.p_vaddr = phdr32.p_vaddr;
  phdr.p_paddr = phdr32.p_paddr;
  phdr.p_filesz = phdr32.p_filesz;
  phdr.p_memsz = phdr32.p_memsz;
  phdr.p_flags = phdr32.p_flags;
  phdr.p_align = phdr32.p_align;
  return phdr;
}

}  // namespace

fit::result<Error> LoadedElfModule::Load() {
  if (!module_.binary_memory) {
    return fit::error(Error("no binary memory"));
  }

  if (auto err = LoadElfHeader(); err.is_error()) {
    return err;
  }

  if (auto err = LoadPhdrs(*ehdr_); err.is_error()) {
    return err;
  }

  return fit::ok();
}

fit::result<Error> LoadedElfModule::LoadElfHeader() {
  if (ehdr_.has_value()) {
    return fit::ok();
  }

  // Callers are responsible for ensuring that |module_.binary_memory| is valid before calling this
  // method.
  switch (module_.size) {
    case Module::AddressSize::k32Bit: {
      auto ehdr = ReadEhdr<Elf32_Ehdr>(module_.binary_memory, module_.load_address);
      if (ehdr.is_error()) {
        return ehdr.take_error();
      }

      ehdr_ = *ehdr;
      break;
    }
    case Module::AddressSize::k64Bit: {
      auto ehdr = ReadEhdr<Elf64_Ehdr>(module_.binary_memory, module_.load_address);
      if (ehdr.is_error()) {
        return ehdr.take_error();
      }

      ehdr_ = *ehdr;
      break;
    }
    default:
      return fit::error(Error("Unknown ELF class."));
  }

  if (!elf_utils::VerifyElfIdentification(*ehdr_, module_.size == Module::AddressSize::k32Bit
                                                      ? elf_utils::ElfClass::k32Bit
                                                      : elf_utils::ElfClass::k64Bit)) {
    return fit::error(Error("Invalid ELF header"));
  }

  return fit::ok();
}

fit::result<Error> LoadedElfModule::LoadPhdrs(const Elf64_Ehdr& ehdr) {
  // Already loaded.
  if (!phdrs_.empty()) {
    return fit::ok();
  }

  if (ehdr.e_ident[EI_CLASS] == ELFCLASS64) {
    // Use resize here since we won't be inserting anything a.la. emplace_back, which means we need
    // the size of the vector to be initialized already so the loop below doesn't think that the
    // vector is empty.
    phdrs_.resize(ehdr.e_phnum);
    if (auto err = module_.binary_memory->ReadBytes(module_.load_address + ehdr.e_phoff,
                                                    ehdr.e_phnum * ehdr.e_phentsize, phdrs_.data());
        err.has_err()) {
      return fit::error(err);
    }
  } else {
    // Meanwhile here we have to to a translation anyway to upcast the Elf32_Phdrs to Elf64_Phdrs so
    // we can just reserve upfront and then the insertions below will increase the vector's size
    // without reallocating.
    phdrs_.reserve(ehdr.e_phnum);
    std::vector<Elf32_Phdr> phdr32_buf(ehdr.e_phnum);
    if (auto err =
            module_.binary_memory->ReadBytes(module_.load_address + ehdr.e_phoff,
                                             ehdr.e_phnum * ehdr.e_phentsize, phdr32_buf.data());
        err.has_err()) {
      return fit::error(err);
    }

    std::ranges::transform(
        phdr32_buf, std::inserter(phdrs_, phdrs_.begin()),
        [](const Elf32_Phdr& phdr32) -> Elf64_Phdr { return UpcastPhdr(phdr32); });
  }

  pc_begin_ = std::numeric_limits<uint64_t>::max();
  pc_end_ = std::numeric_limits<uint64_t>::min();

  for (const auto& phdr : phdrs_) {
    if (phdr.p_type == PT_LOAD) {
      // Note that we cannot limit the inspection of PT_LOAD segments to those that are marked
      // executable because this does not necessarily match the actual mapping of executable VMOs.
      // If we want to narrow the range of PCs further we should consult the |zx_info_maps_t| for
      // this process. Overflow and arithmetic errors are ignored here, we just need the minimum
      // over the entire set of PT_LOAD segments.
      std::ignore =
          safemath::CheckMin(safemath::CheckAdd(module_.load_address, phdr.p_vaddr), pc_begin_)
              .AssignIfValid(&pc_begin_);

      std::ignore =
          safemath::CheckMax(safemath::CheckAdd(module_.load_address, phdr.p_vaddr, phdr.p_memsz),
                             pc_end_)
              .AssignIfValid(&pc_end_);
    }
  }

  return fit::ok();
}

}  // namespace unwinder
