// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/arm_ehabi_unwinder.h"

#include "src/lib/unwinder/arm_ehabi_module.h"
#include "src/lib/unwinder/cfi_unwinder.h"
#include "src/lib/unwinder/error.h"
#include "src/lib/unwinder/module.h"
#include "src/lib/unwinder/registers.h"

namespace unwinder {

Error ArmEhAbiUnwinder::Step(Memory* stack, const Frame& current, Frame& next) {
  if (current.regs.arch() != Registers::Arch::kArm64 &&
      current.regs.arch() != Registers::Arch::kArm32) {
    return Error("Not ARM architecture: %s", ArchToString(current.regs.arch()).c_str());
  }

  uint64_t pc = 0;
  if (auto err = current.regs.GetPC(pc); err.has_err()) {
    return err;
  }

  Registers regs = current.regs;

  // pc_is_return_address indicates whether pc in the current registers is a return address from a
  // previous "Step". If it is, we need to subtract 1 to find the call site because "call" could
  // be the last instruction of a nonreturn function and now the PC is pointing outside of the
  // valid code boundary.
  //
  // Subtracting 1 is sufficient here because in |Search| above, we binary search function start
  // addresses to find the unwinding instructions corresponding to this address. So it's still
  // correct even if pc is not pointing to the beginning of an instruction.
  if (current.pc_is_return_address) {
    pc -= 1;
    regs.SetPC(pc);
  }

  const Module* elf_module;
  if (auto err = cfi_unwinder_->GetModuleForPc(pc, &elf_module); err.has_err()) {
    return err;
  }

  switch (elf_module->size) {
    case Module::AddressSize::k32Bit:
      // Make sure we mark the next registers as 32 bit so we're setting the expected PC, LR, and SP
      // registers.
      next.regs = Registers(Registers::Arch::kArm32);
      return Step(stack, elf_module, regs, next.regs);
    case Module::AddressSize::k64Bit:
      return Error("Module for PC is not 32 bit.");
    default:
      return Error("Unknown ELF Class");
  }
}

Error ArmEhAbiUnwinder::Step(Memory* stack, const Module* elf_module, const Registers& current,
                             Registers& next) {
  ArmEhAbiModule* ehabi_module;
  if (auto result = GetEhAbiModuleFromModuleInfo(elf_module); result.is_ok()) {
    ehabi_module = result.value().ehabi_module;
  } else {
    return result.error_value();
  }

  return ehabi_module->Step(stack, current, next);
}

void ArmEhAbiUnwinder::AsyncStep(AsyncMemory* stack, const Frame& current,
                                 fit::callback<void(Error, Registers)> cb) {
  if (current.regs.arch() != Registers::Arch::kArm64 &&
      current.regs.arch() != Registers::Arch::kArm32) {
    cb(Error("Not ARM architecture: %s", ArchToString(current.regs.arch()).c_str()),
       Registers(current.regs.arch()));
    return;
  }
  uint64_t pc;
  if (auto err = current.regs.GetPC(pc); err.has_err()) {
    return cb(err, Registers(current.regs.arch()));
  }

  Registers regs = current.regs;

  if (current.pc_is_return_address) {
    pc -= 1;
    regs.SetPC(pc);
  }

  const Module* elf_module = nullptr;
  if (auto err = cfi_unwinder_->GetModuleForPc(pc, &elf_module); err.has_err()) {
    return cb(err, Registers(current.regs.arch()));
  }

  if (elf_module->size != Module::AddressSize::k32Bit) {
    cb(Error("Module for PC is not 32 bit."), Registers(current.regs.arch()));
    return;
  }

  AsyncStep(stack, elf_module, regs, std::move(cb));
}

void ArmEhAbiUnwinder::AsyncStep(AsyncMemory* stack, const Module* elf_module,
                                 const Registers& current,
                                 fit::callback<void(Error, Registers)> cb) {
  auto result = GetEhAbiModuleFromModuleInfo(elf_module);
  if (result.is_error()) {
    return cb(result.error_value(), Registers(current.arch()));
  }

  ArmEhAbiModule* ehabi_module = result.value().ehabi_module;

  uint64_t sp;
  if (auto err = current.GetSP(sp); err.has_err()) {
    return cb(err, Registers(current.arch()));
  }

  constexpr uint32_t kDefaultStackSize = 8192;
  if (result.value().should_synchronize_stack) {
    stack->FetchMemoryRanges({{sp, kDefaultStackSize}}, [=, cb = std::move(cb)]() mutable {
      ehabi_module->AsyncStep(stack, current, std::move(cb));
    });
  } else {
    ehabi_module->AsyncStep(stack, current, std::move(cb));
  }
}

fit::result<Error, ArmEhAbiUnwinder::EhAbiModuleResult>
ArmEhAbiUnwinder::GetEhAbiModuleFromModuleInfo(const Module* elf_module) {
  // The CFI Unwinder keeps a record of all the modules, so it can properly find the right module
  // for this PC. Since we don't have to keep track of anything other than the 32 bit modules here
  // we can just index on the load address of the already found module.
  auto it = module_map_.find(static_cast<uint32_t>(elf_module->load_address));

  EhAbiModuleResult result;

  if (it == module_map_.end()) {
    // Need to insert this module.
    auto ehabi_module =
        std::make_unique<ArmEhAbiModule>(elf_module->binary_memory, elf_module->load_address);

    if (auto err = ehabi_module->Load(); err.has_err()) {
      // Now try with the debug info memory if it's available.
      if (elf_module->debug_info_memory) {
        ehabi_module = std::make_unique<ArmEhAbiModule>(elf_module->debug_info_memory,
                                                        elf_module->load_address);
        if (auto debug_err = ehabi_module->Load(); debug_err.has_err()) {
          return fit::error(Error("Failed to load .ARM.exidx sections: stripped binary: " +
                                  err.msg() + "; unstripped binary: " + debug_err.msg()));
        }
      } else {
        return fit::error(err);
      }
    }

    // If either of the above worked, then we have a valid ARM EH ABI module to add to our cache.
    it =
        module_map_.insert(std::make_pair(elf_module->load_address, std::move(ehabi_module))).first;
    result.should_synchronize_stack = true;
  }

  result.ehabi_module = it->second.get();
  return fit::ok(result);
}

}  // namespace unwinder
