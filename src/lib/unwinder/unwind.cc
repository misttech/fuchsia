// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/unwind.h"

#include <cstdint>
#include <cstdio>
#include <memory>
#include <utility>
#include <vector>

#include "src/lib/unwinder/arm_ehabi_unwinder.h"
#include "src/lib/unwinder/cfi_unwinder.h"
#include "src/lib/unwinder/error.h"
#include "src/lib/unwinder/fp_unwinder.h"
#include "src/lib/unwinder/memory.h"
#include "src/lib/unwinder/module.h"
#include "src/lib/unwinder/plt_unwinder.h"
#include "src/lib/unwinder/registers.h"
#include "src/lib/unwinder/scs_unwinder.h"
#include "src/lib/unwinder/sigreturn_unwinder.h"
#include "src/lib/unwinder/unwinder_base.h"

namespace unwinder {

namespace {

bool PcIsReturnAddress(const Registers& regs) {
  // If |regs| is recovered from a regular function call, rax/lr/ra will be scratched.
  // Otherwise, they will be available.
  RegisterID reg_id;
  switch (regs.arch()) {
    case Registers::Arch::kX64:
      reg_id = RegisterID::kX64_rax;
      break;
    case Registers::Arch::kArm32:
      reg_id = RegisterID::kArm32_lr;
      break;
    case Registers::Arch::kArm64:
      reg_id = RegisterID::kArm64_lr;
      break;
    case Registers::Arch::kRiscv64:
      reg_id = RegisterID::kRiscv64_ra;
      break;
  }
  uint64_t val;

  return regs.Get(reg_id, val).has_err();
}

void FixupFrame(CfiUnwinder* cfi_unwinder, Frame& next) {
  // If the frame was identified with an S augmentation by the CFI unwinder, then we know that
  // this definitely not a return address.
  if (next.is_signal_frame) {
    next.pc_is_return_address = false;
    return;
  }

  // Successfully probing a sigreturn frame means the next frame needs to be unwound by the
  // sigreturn unwinder. Only do this if the CFI unwinder failed to detect the 'S' augmentation.
  if (!next.is_signal_frame) {
    if (auto err = SigReturnUnwinder::ProbePCForSigReturn(cfi_unwinder, next.regs); err.ok()) {
      next.pc_is_return_address = false;
      next.is_signal_frame = true;
      return;
    }
  }

  // Otherwise defer to the value of the return address register.
  if (next.trust == Frame::Trust::kCFI) {
    next.pc_is_return_address = PcIsReturnAddress(next.regs);
  } else if (next.trust == Frame::Trust::kSigReturn) {
    next.pc_is_return_address = false;
  } else {
    next.pc_is_return_address = true;
  }
}

Error TryUnwinder(UnwinderBase* unwinder, Memory* stack, const Frame& current, Frame& next) {
  auto err = unwinder->Step(stack, current, next);

  if (err.has_err()) {
    return err;
  }

  next.trust = unwinder->trust();
  FixupFrame(unwinder->cfi_unwinder(), next);

  return Success();
}

void TryAsyncUnwinder(UnwinderBase* unwinder, AsyncMemory* stack, const Frame& current,
                      fit::callback<void(Error, Frame)> cb) {
  unwinder->AsyncStep(stack, current, [=, cb = std::move(cb)](Error err, Registers next) mutable {
    Frame next_frame(std::move(next), false, unwinder->trust());

    if (err.has_err()) {
      next_frame.error = err;
      return cb(err, std::move(next_frame));
    }

    next_frame.trust = unwinder->trust();
    FixupFrame(unwinder->cfi_unwinder(), next_frame);
    return cb(Success(), std::move(next_frame));
  });
}

}  // namespace

std::string Frame::Describe() const {
  std::string res = "registers={" + regs.Describe() + "}  trust=";
  switch (trust) {
    case Trust::kScan:
      res += "Scan";
      break;
    case Trust::kSigReturn:
      res += "SigReturn";
      break;
    case Trust::kSCS:
      res += "SCS";
      break;
    case Trust::kFP:
      res += "FP";
      break;
    case Trust::kPLT:
      res += "PLT";
      break;
    case Trust::kArmEhAbi:
      res += "ArmEhAbi";
      break;
    case Trust::kCFI:
      res += "CFI";
      break;
    case Trust::kContext:
      res += "Context";
      break;
  }
  if (pc_is_return_address) {
    res += "  pc_is_return_address";
  }
  if (error.has_err()) {
    res += "  error=\"" + error.msg() + "\"";
  }
  return res;
}

Unwinder::Unwinder(const std::vector<Module>& modules) : cfi_unwinder_(modules) {}

std::vector<Frame> Unwinder::Unwind(Memory* stack, const Registers& registers, size_t max_depth) {
  UnavailableMemory unavailable_memory;
  if (!stack) {
    stack = &unavailable_memory;
  }

  std::vector<Frame> res = {{registers, false, Frame::Trust::kContext}};

  while (--max_depth) {
    Frame& current = res.back();

    Frame next(Registers(current.regs.arch()), /*placeholders*/ true, Frame::Trust::kCFI);

    Step(stack, current, next);

    // An undefined PC (e.g. on Linux) or 0 PC (e.g. on Fuchsia) marks the end of the unwinding.
    // Don't include this in the output because it's not a real frame and provides no information.
    // A failed unwinding will also end up with an undefined PC.
    if (uint64_t pc; next.regs.GetPC(pc).has_err() || pc == 0) {
      break;
    }

    res.push_back(std::move(next));
  }

  return res;
}

void Unwinder::Step(Memory* stack, Frame& current, Frame& next) {
  ArmEhAbiUnwinder arm_ehabi_unwinder(&cfi_unwinder_);
  FramePointerUnwinder fp_unwinder(&cfi_unwinder_);
  PltUnwinder plt_unwinder(&cfi_unwinder_);
  ShadowCallStackUnwinder scs_unwinder(&cfi_unwinder_);
  SigReturnUnwinder sigreturn_unwinder(&cfi_unwinder_);

  bool success = false;
  std::string err_msg;

  // Try sigreturn first, since it will be explicitly requested via |current.is_signal_frame|. This
  // means the CFI unwinder got an S augmentation or we already successfully probed sigreturn
  // instructions for |current.pc|.
  if (current.is_signal_frame) {
    if (auto err = TryUnwinder(&sigreturn_unwinder, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "SIGRETURN: " + err.msg();
    }
  }

  // For non-signal frames, try CFI first because it's the most accurate one.
  // TODO(https://fxbug.dev/316047562): Make CFI work on RISC-V.
  if (current.regs.arch() != Registers::Arch::kRiscv64) {
    if (auto err = TryUnwinder(&cfi_unwinder_, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "; CFI: " + err.msg();
    }
  }

  // Try ArmEhAbi before the others because it will play well with CFI. Note that this is only
  // possible today by running a 32 bit ARM binary in Starnix - which will be running as a typical
  // 64 bit Fuchsia program. The unwinder implementation will only participate in unwinding if it
  // can successfully probe that the current PC is within a 32 bit ELF module. It's also possible
  // for some binaries to have both CFI and EHABI for a particular address. We want to make sure
  // that the CFI gets to go first, since that will recover the most information, if and only if the
  // CFI was not able to recover PC, we should also consult the EHABI instructions for this address
  // as well.
  uint64_t maybe_pc = 0;
  if (!success || next.regs.GetPC(maybe_pc).has_err() || maybe_pc == 0) {
    if (auto err = TryUnwinder(&arm_ehabi_unwinder, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "; ARMEHABI: " + err.msg();
    }
  }

  if (!success && !current.pc_is_return_address) {
    // PLT unwinder only works for the first frame.
    if (auto err = TryUnwinder(&plt_unwinder, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "; PLT: " + err.msg();
    }
  }

  // Try frame pointers before SCS because it plays well with the CFI.
  if (!success) {
    if (auto err = TryUnwinder(&fp_unwinder, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "; FP: " + err.msg();
    }
  }

  // Try shadow call stacks last because it can only recover PC.
  if (!success) {
    if (auto err = TryUnwinder(&scs_unwinder, stack, current, next); err.ok()) {
      success = true;
    } else {
      err_msg += "; SCS: " + err.msg();
    }
  }

  current.fatal_error = !success;
  if (!err_msg.empty()) {
    current.error = Error(err_msg);
  }
}

AsyncUnwinder::AsyncUnwinder(const std::vector<Module>& modules) : cfi_unwinder_(modules) {
  // The order here is important! This will be the order that the unwinders are attempted and should
  // not be changed without careful thought.
  //
  // In general, the order goes like this:
  //   1. Always try CFI first (which will check both .debug_frame and .eh_frame sections). CFI is
  //      the most reliable unwinding method and will recover all possible register state as
  //      produced by the unwinding metadata. The CFI unwinder object is owned directly by this
  //      class rather than being instantiated as part of the vector for easier access.
  //   2. Next try the SigReturnUnwinder, which will restore all registers found in the sigcontext
  //      struct if one is found. This works well in this case since CFI can then take over the
  //      following frame again.
  //   3. Arm EH ABI comes next, since it is also metadata based and can restore as many registers
  //      as the metadata specifies. One downside of this method (which makes it inferior to CFI) is
  //      that it only captures the register state at the beginning of a function call, if SP is
  //      modified after that point then the unwinding information is no longer valid.
  //   4. After that we try frame pointers, which also work well with all of the above since both SP
  //      and PC are recovered for each frame.
  //   5. Finally try to use the shadow call stack. If it works, then any remaining frames must also
  //      be unwound from the shadow call stack as well since the SP is lost.
  unwinders_.emplace_back(std::make_unique<SigReturnUnwinder>(&cfi_unwinder_));
  unwinders_.emplace_back(std::make_unique<ArmEhAbiUnwinder>(&cfi_unwinder_));
  unwinders_.emplace_back(std::make_unique<PltUnwinder>(&cfi_unwinder_));
  unwinders_.emplace_back(std::make_unique<FramePointerUnwinder>(&cfi_unwinder_));
  unwinders_.emplace_back(std::make_unique<ShadowCallStackUnwinder>(&cfi_unwinder_));
}

void AsyncUnwinder::Unwind(AsyncMemory::Delegate* delegate, const Registers& registers,
                           size_t max_depth, fit::callback<void(std::vector<Frame>)> cb) {
  if (!delegate) {
    // Memory delegate must be provided.
    return cb({});
  }

  stack_ = std::make_unique<AsyncMemory>(delegate);
  max_depth_ = max_depth;
  on_done_ = std::move(cb);

  result_ = {{registers, false, Frame::Trust::kContext}};

  uint64_t sp;
  if (auto err = registers.GetSP(sp); err.has_err()) {
    return cb(std::move(result_));
  }

  constexpr uint32_t kDefaultStackSize = 8192;

  // We'll mostly be working with the stack, so we request a chunk to start off with. 8KiB should be
  // plenty.
  stack_->FetchMemoryRanges({{sp, kDefaultStackSize}}, [this]() {
    // Now we can kick everything off with the contextual first frame.
    Step(result_.back());
  });
}

void AsyncUnwinder::Step(const Frame& current) {
  // TODO(https://fxbug.dev/316047562): Make CFI work on RISC-V.
  TryAsyncUnwinder(&cfi_unwinder_, stack_.get(), current,
                   [this](const Error& err, Frame next) { OnUnwinderStep(err, std::move(next)); });
}

void AsyncUnwinder::OnUnwinderStep(const Error& err, Frame next) {
  if (err.ok()) {
    // The current unwinder reported success, and we have a new valid frame. Reset the current
    // unwinder state and report success.
    current_unwinder_ = std::nullopt;
    OnStep(std::move(next));
    return;
  }

  UnwinderBase* unwinder;
  if (auto result = NextUnwinder(); result.is_ok()) {
    unwinder = result.value();
  } else {
    // Indicate that we're done by setting PC to 0.
    next.regs.SetPC(0);
    OnStep(std::move(next));
    return;
  }

  TryAsyncUnwinder(unwinder, stack_.get(), result_.back(),
                   [this](const Error& err, Frame next) { OnUnwinderStep(err, std::move(next)); });
}

void AsyncUnwinder::OnStep(Frame next) {
  // An undefined PC (e.g. on Linux) or 0 PC (e.g. on Fuchsia) marks the end of the unwinding.
  // Don't include this in the output because it's not a real frame and provides no
  // information. A failed unwinding will also end up with an undefined PC.
  if (uint64_t pc; next.regs.GetPC(pc).has_err() || pc == 0 || max_depth_ == 0) {
    return on_done_(std::move(result_));
  }

  result_.push_back(std::move(next));
  max_depth_--;
  Step(result_.back());
}

fit::result<Error, UnwinderBase*> AsyncUnwinder::NextUnwinder() {
  // The first unwinder failed, so we need to initialize |current_unwinder_| now.
  if (!current_unwinder_) {
    current_unwinder_ = 0;
  } else if (current_unwinder_.value() == unwinders_.size() - 1) {
    return fit::error(Error("No more unwinders."));
  } else {
    // Else advance to the next unwinder.
    (*current_unwinder_)++;
  }

  return fit::ok(unwinders_[*current_unwinder_].get());
}

std::vector<Frame> Unwind(Memory* memory, const std::vector<uint64_t>& modules,
                          const Registers& registers, size_t max_depth) {
  std::vector<Module> converted;
  converted.reserve(modules.size());
  for (const auto& addr : modules) {
    converted.emplace_back(addr, memory, Module::AddressMode::kProcess);
  }
  return Unwinder(converted).Unwind(memory, registers, max_depth);
}

}  // namespace unwinder
