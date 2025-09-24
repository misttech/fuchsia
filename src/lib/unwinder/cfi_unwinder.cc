// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/cfi_unwinder.h"

#include <cinttypes>
#include <cstdint>
#include <limits>
#include <memory>
#include <utility>
#include <vector>

#include "src/lib/unwinder/cfi_module.h"
#include "src/lib/unwinder/error.h"

namespace unwinder {

namespace {
// Validates our assumptions about what registers we should have recovered in a "transition" 32 bit
// frame that was recovered from a 64 bit frame. For now, this should only ever happen via Starnix's
// custom CFI directives that get us the first restricted mode frame, which is what should be
// contained in |regs|. In this state, we should always successfully recover all of SP, LR, and PC,
// and they should all be pointing into the 32 bit restricted mode address space.
Error Validate32BitRegisters(const Registers& regs) {
  if (regs.arch() != Registers::Arch::kArm32) {
    return Error("New registers aren't kArm32?");
  }

  uint64_t val;
  if (auto err = regs.GetSP(val); err.has_err()) {
    return Error("32 bit registers don't have SP (r13) set: %s\n32 Bit Registers: %s",
                 err.msg().c_str(), regs.Describe().c_str());
  } else if (val > std::numeric_limits<uint32_t>::max()) {
    return Error("32 bit SP (r13) %" PRIx64 " > uint32::MAX", val);
  }

  if (auto err = regs.GetReturnAddress(val); err.has_err()) {
    return Error("32 bit registers don't have LR (r14) set: %s\n32 Bit Registers: %s",
                 err.msg().c_str(), regs.Describe().c_str());
  } else if (val > std::numeric_limits<uint32_t>::max()) {
    return Error("32 bit LR (r14) %" PRIx64 " > uint32::MAX", val);
  }

  if (auto err = regs.GetPC(val); err.has_err()) {
    return Error("32 bit registers don't have PC (r15) set: %s\n32 Bit Registers: %s",
                 err.msg().c_str(), regs.Describe().c_str());
  } else if (val > std::numeric_limits<uint32_t>::max()) {
    return Error("32 bit PC (r15) %" PRIx64 " > uint32::MAX", val);
  }

  return Success();
}

// Returns an error if the |next| registers do not have PC and LR populated with 32 bit values.
fit::result<Error, Registers> TryConvertRegistersTo32Bit(const Registers& current,
                                                         const Registers& next,
                                                         const Module* next_module) {
  if (current.arch() != Registers::Arch::kArm64) {
    return fit::error(Error("Current registers are not kArm64."));
  } else if (next.arch() == Registers::Arch::kArm32) {
    return fit::error(Error("Next registers are already kArm32, nothing to do."));
  }

  uint64_t pc = 0;
  uint64_t ra = 0;

  // As of today the only way we should ever successfully transition to a 32 bit frame is when we
  // have just recovered all of the registers specified by Starnix's CFI directives to reconstruct
  // the restricted mode stack. That means that we should _always_ have both PC and LR available
  // from the CFI (which should be finished processing by the time this is called). Therefore if we
  // cannot fetch either of them from |next| we bail out.
  if (next.GetPC(pc).has_err()) {
    return fit::error(Error("Next registers do not have PC set: %s", next.Describe().c_str()));
  }

  if (next.GetReturnAddress(ra).has_err()) {
    return fit::error(Error("Next registers do not have LR set: %s", next.Describe().c_str()));
  }

  if (!(pc < std::numeric_limits<uint32_t>::max()) ||
      !(ra < std::numeric_limits<uint32_t>::max())) {
    return fit::error(Error("PC [%" PRIx64 "] or LR [%" PRIx64
                            "] contains address greater than 32 bit address space.",
                            pc, ra));
  }

  return next.To32Bit();
}

}  // namespace

bool CfiModuleInfo::IsValidPC(uint64_t pc) const {
  return ((binary && binary->IsValidPC(pc)) || (debug_info && debug_info->IsValidPC(pc)));
}

CfiUnwinder::CfiUnwinder(const std::vector<Module>& modules) : UnwinderBase(this) {
  for (const auto& module : modules) {
    module_map_.emplace(module.load_address,
                        CfiModuleInfo{.module = module, .binary = nullptr, .debug_info = nullptr});
  }
}

Error CfiUnwinder::Step(Memory* stack, const Frame& current, Frame& next) {
  if (auto result = Step(stack, current.regs, next.regs, current.pc_is_return_address);
      result.is_error()) {
    return result.error_value();
  } else {
    next.is_signal_frame = result.value();
  }

  return Success();
}

fit::result<Error, bool> CfiUnwinder::Step(Memory* stack, const Registers& current, Registers& next,
                                           bool is_return_address) {
  uint64_t pc;
  if (auto err = current.GetPC(pc); err.has_err()) {
    return fit::error(err);
  }

  Registers regs = current;

  // is_return_address indicates whether pc in the current registers is a return address from a
  // previous "Step". If it is, we need to subtract 1 to find the call site because "call" could
  // be the last instruction of a nonreturn function and now the PC is pointing outside of the
  // valid code boundary.
  //
  // Subtracting 1 is sufficient here because in CfiParser::ParseInstructions, we scan CFI until
  // pc > pc_limit. So it's still correct even if pc_limit is not pointing to the beginning of an
  // instruction.
  if (is_return_address) {
    pc -= 1;
    regs.SetPC(pc);
  }

  // We might have a PC in a 32 bit module. If we do, we'll need to convert the registers to the 32
  // bit arch so all the named registers correspond to their 32 bit register numbers instead of 64
  // bit. Checking that PC < UINT32_MAX is just a heuristic and doesn't actually indicate
  // anything about address size for the module.
  if (regs.arch() != Registers::Arch::kArm32 && pc < std::numeric_limits<uint32_t>::max()) {
    Module* next_module = nullptr;
    if (auto err = GetModuleForPc(pc, &next_module); err.has_err()) {
      return fit::error(err);
    };

    if (next_module->size == Module::AddressSize::k32Bit) {
      // In the error case, the message is probably only useful for developing and debugging the
      // unwinder itself and will happen frequently enough that we shouldn't log it, but for
      // debugging purposes can be displayed if needed. The validation step in the success case is a
      // fatal error since we have strict expectations of what registers are restored by Starnix,
      // which is currently the only way we should ever transition to code in a 32 bit address
      // space.
      if (auto maybe_32bit = TryConvertRegistersTo32Bit(current, regs, next_module);
          maybe_32bit.is_ok()) {
        // Both PC and LR in |next| appear to be 32 bit addresses, now validate that the converted
        // 32 bit registers actually have everything that we expect to get from Starnix's CFI: PC,
        // LR, and SP should all be populated and have 32 bit addresses, if this fails at this
        // point, it's an error.
        if (auto err = Validate32BitRegisters(*maybe_32bit); err.has_err()) {
          return fit::error(err);
        }

        // Validations succeeded, |next| is a 32 bit frame recovered from Starnix restricted mode.
        // It's entirely possible at this point for the 32 bit binary to have CFI instructions for
        // this 32 bit PC value, so we continue on.
        regs = *maybe_32bit;
      }
    }
  }

  CfiModuleInfo* cfi;
  if (auto err = GetCfiModuleInfoForPc(pc, &cfi); err.has_err()) {
    return fit::error(err);
  }

  auto result = cfi->binary->Step(stack, regs, next);
  if (result.is_error()) {
    return result;
  }

  return fit::ok(result.value());
}

void CfiUnwinder::AsyncStep(AsyncMemory* stack, const Frame& current,
                            fit::callback<void(Error, Registers)> cb) {
  // TODO(https://fxbug.dev/316047562): Make CFI work on RISC-V.
  if (current.regs.arch() == Registers::Arch::kRiscv64) {
    return cb(Error("RISC-V is not supported with the CFI Unwinder."),
              Registers(current.regs.arch()));
  }

  AsyncStep(stack, current.regs, current.pc_is_return_address, std::move(cb));
}

void CfiUnwinder::AsyncStep(AsyncMemory* stack, Registers current, bool is_return_address,
                            fit::callback<void(Error, Registers)> cb) {
  uint64_t pc;
  if (auto err = current.GetPC(pc); err.has_err()) {
    return cb(err, Registers(current.arch()));
  }

  // is_return_address indicates whether pc in the current registers is a return address from a
  // previous "Step". If it is, we need to subtract 1 to find the call site because "call" could
  // be the last instruction of a nonreturn function and now the PC is pointing outside of the
  // valid code boundary.
  //
  // Subtracting 1 is sufficient here because in CfiParser::ParseInstructions, we scan CFI until
  // pc > pc_limit. So it's still correct even if pc_limit is not pointing to the beginning of an
  // instruction.
  if (is_return_address) {
    pc -= 1;
    current.SetPC(pc);
  }

  CfiModuleInfo* cfi_info;
  if (auto err = GetCfiModuleInfoForPc(pc, &cfi_info); err.has_err()) {
    return cb(err, Registers(current.arch()));
  }

  if (cfi_info->debug_info) {
    // Try stepping with the debug_info if it is available. This could contain both .debug_frame and
    // .eh_frame sections in the case of a fully unstripped binary, or just a .debug_frame section
    // in the case of a separated debug_info binary. Both have to fail for us to try again with the
    // "binary" file, which will only contain an .eh_frame section.
    cfi_info->debug_info->AsyncStep(
        stack, current,
        [cfi_info, stack, current, cb = std::move(cb)](Error err, Registers regs) mutable {
          if (err.has_err()) {
            // debug_info didn't work, try again with the binary module instead. If this fails it's
            // a fatal error for this unwinder.
            if (cfi_info->binary) {
              return cfi_info->binary->AsyncStep(
                  stack, current, [e = err, cb = std::move(cb)](Error err, Registers regs) mutable {
                    if (err.has_err()) {
                      // Propagate both errors up.
                      return cb(Error("debug_info:" + e.msg() + ";binary:" + err.msg()),
                                std::move(regs));
                    }

                    // Using the binary worked.
                    cb(err, std::move(regs));
                  });
            } else {
              return cb(Error("debug_info:" + err.msg() + ";binary not present."), regs);
            }
          }

          // Unwinding with the debug_info module worked, issue the callback.
          cb(err, std::move(regs));
        });
  } else if (cfi_info->binary) {
    // No debug_info available, unwind with the binary module.
    cfi_info->binary->AsyncStep(stack, current, std::move(cb));
  } else {
    return cb(Error("Module has no associated memory."), Registers(current.arch()));
  }
}

bool CfiUnwinder::IsValidPC(uint64_t pc) {
  CfiModuleInfo* cfi;
  return GetCfiModuleInfoForPc(pc, &cfi).ok();
}

Error CfiUnwinder::GetCfiModuleInfoForPc(uint64_t pc, CfiModuleInfo** out) {
  auto module_it = module_map_.upper_bound(pc);
  if (module_it == module_map_.begin()) {
    return Error("%#" PRIx64 " is not covered by any module", pc);
  }
  module_it--;
  uint64_t module_address = module_it->first;
  auto& module_info = module_it->second;

  if (!module_info.binary && module_info.module.binary_memory) {
    module_info.binary = std::make_unique<CfiModule>(module_info.module.binary_memory,
                                                     module_address, module_info.module);
    // Loading the main binary file should always contain either an eh_frame section or a
    // debug_frame section.
    if (auto err = module_info.binary->Load(); err.has_err()) {
      return err;
    }
  }

  if (!module_info.debug_info && module_info.module.debug_info_memory) {
    module_info.debug_info = std::make_unique<CfiModule>(module_info.module.debug_info_memory,
                                                         module_address, module_info.module);
    // A split debug info file may contain neither eh_frame nor debug_frame sections, it is not an
    // error if this fails to load.
    if (auto err = module_info.debug_info->Load(); err.has_err()) {
      // Reset the pointer to null to indicate that it should not be used for look ups later.
      module_info.debug_info.reset();
    }
  }

  if (!module_info.IsValidPC(pc)) {
    return Error("%#" PRIx64 " is not a valid PC in module %#" PRIx64, pc, module_address);
  }

  *out = &module_info;
  return Success();
}

Error CfiUnwinder::GetModuleForPc(uint64_t pc, Module** out) {
  auto module_it = module_map_.upper_bound(pc);
  if (module_it == module_map_.begin()) {
    return Error("%#" PRIx64 " is not covered by any module", pc);
  }
  module_it--;
  auto& module_info = module_it->second;

  // The actual low-level module object is owned by the CfiModuleInfo instance we have found, it's
  // always valid at this point. It's up to callers to determine whether the binary or debug_info
  // memory they want is valid before using them. The load address, mode, and address size will
  // always be safe to read even if the ELF is invalid.
  *out = &module_info.module;
  return Success();
}

}  // namespace unwinder
