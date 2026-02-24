// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_UNWINDER_CFI_UNWINDER_H_
#define SRC_LIB_UNWINDER_CFI_UNWINDER_H_

#include <cstdint>
#include <functional>
#include <map>
#include <memory>

#include "sdk/lib/fit/include/lib/fit/function.h"
#include "src/lib/unwinder/cfi_module.h"
#include "src/lib/unwinder/elf_module_cache.h"
#include "src/lib/unwinder/memory.h"
#include "src/lib/unwinder/registers.h"
#include "src/lib/unwinder/unwinder_base.h"

namespace unwinder {

// Contains the CFI modules for a given ElfModule. The CfiModules are lazily loaded.
struct CfiModuleInfo {
  explicit CfiModuleInfo(Module elf_module) : module(elf_module) {}
  Module module;
  // The loaded binary file corresponding to this module.. This is the (possibly stripped) binary
  // file. It may contain an .eh_frame section, and optionally a .debug_frame section (in the case
  // it is unstripped).
  std::unique_ptr<CfiModule> binary = nullptr;
  // The loaded debug info file, if present. This is an optional addition to the binary file
  // above. When non-null, this file will be inspected for a .debug_frame section before the
  // binary file. This is only loaded and used if the |debug_info_memory| is non-null in |module|.
  std::unique_ptr<CfiModule> debug_info = nullptr;
};

class CfiUnwinder : public UnwinderBase {
 public:
  explicit CfiUnwinder(const ElfModuleCache& elf_module_cache) : UnwinderBase(elf_module_cache) {}

  Error Step(Memory* stack, const Frame& current, Frame& next) override;

  void AsyncStep(AsyncMemory* stack, const Frame& current,
                 fit::callback<void(Error, Registers)> cb) override;

  Frame::Trust trust() const override { return Frame::Trust::kCFI; }

 private:
  // If the returned value is fit::ok, then the contained boolean indicates whether the next frame
  // is a signal frame or not. Otherwise the encased Error type will have more information.
  fit::result<Error, bool> Step(Memory* stack, const Registers& current, Registers& next,
                                bool is_return_address);

  void AsyncStep(AsyncMemory* stack, const Registers& current, bool is_return_address,
                 fit::callback<void(Error, Registers)> cb);

  fit::result<Error, Registers> ConvertTo32BitIfNeeded(uint64_t pc, const Registers& current);

  using CfiModuleInfoRef = std::reference_wrapper<const CfiModuleInfo>;
  fit::result<Error, CfiModuleInfoRef> GetCfiModuleInfoForPc(uint64_t pc);

  // Mapping from module load addresses to a pair of (module description, lazily-initialized CFI
  // modules for the binary and optional debugging info).
  std::map<uint64_t, CfiModuleInfo> module_map_;
};

}  // namespace unwinder

#endif  // SRC_LIB_UNWINDER_CFI_UNWINDER_H_
