// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/remote_unwinder.h"

#include <lib/syslog/cpp/macros.h>

#include <memory>

#include "src/developer/debug/ipc/unwinder_support.h"
#include "src/developer/debug/zxdb/client/elf_memory_region.h"
#include "src/developer/debug/zxdb/client/process_impl.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/thread_impl.h"
#include "src/developer/debug/zxdb/symbols/loaded_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/process_symbols.h"
#include "src/lib/unwinder/arm_ehabi_unwinder.h"
#include "src/lib/unwinder/cfi_unwinder.h"
#include "src/lib/unwinder/elf_module_cache.h"
#include "src/lib/unwinder/fp_unwinder.h"
#include "src/lib/unwinder/memory.h"
#include "src/lib/unwinder/plt_unwinder.h"
#include "src/lib/unwinder/scs_unwinder.h"
#include "src/lib/unwinder/sigreturn_unwinder.h"
#include "src/lib/unwinder/unwind.h"

namespace zxdb {

namespace {

// A RAII wrapper around the unwinder implementation and its module memory to use when unwinding
// asynchronously. This ensures that the underlying module memory, ElfModuleCache, and unwinder
// implementation stay alive for the duration of the unwinding and are destroyed in the proper
// order.
class RemoteUnwinderImplWrapper {
 public:
  explicit RemoteUnwinderImplWrapper(std::vector<unwinder::Module> modules,
                                     std::vector<std::unique_ptr<unwinder::Memory>> memory,
                                     unwinder::AsyncMemory::Delegate* delegate)
      : memory_(std::move(memory)), modules_(std::move(modules)), delegate_(delegate) {}

  ~RemoteUnwinderImplWrapper() {
    // Explicitly destroy the unwinder and module cache before freeing the underlying
    // ElfMemoryRegion instances in |memory_|.
    unwinder_impl_.reset();
    async_unwinder_.reset();
    module_cache_.reset();
    memory_.clear();
  }

  void Unwind(std::optional<debug_ipc::StackFrame::Trust> forced_unwinder,
              const unwinder::Registers& regs, size_t max_depth,
              fit::callback<void(std::vector<unwinder::Frame>)> on_done) {
    if (!forced_unwinder) {
      // If no specific unwinder implementation was given, then we use the hybrid unwinder API.
      async_unwinder_ = std::make_unique<unwinder::AsyncUnwinder>(delegate_, modules_);
      async_unwinder_->Unwind(regs, max_depth, std::move(on_done));
      return;
    }

    stack_.emplace(delegate_);
    module_cache_.emplace(modules_);
    switch (debug_ipc::ConvertTrust(*forced_unwinder)) {
      case unwinder::Frame::Trust::kCFI:
        unwinder_impl_ = std::make_unique<unwinder::CfiUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kFP:
        unwinder_impl_ = std::make_unique<unwinder::FramePointerUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kSCS:
        unwinder_impl_ = std::make_unique<unwinder::ShadowCallStackUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kPLT:
        unwinder_impl_ = std::make_unique<unwinder::PltUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kArmEhAbi:
        unwinder_impl_ = std::make_unique<unwinder::ArmEhAbiUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kSigReturn:
        unwinder_impl_ = std::make_unique<unwinder::SigReturnUnwinder>(*module_cache_);
        break;
      case unwinder::Frame::Trust::kContext:
        [[fallthrough]];
      case unwinder::Frame::Trust::kScan:
        on_done({unwinder::Frame(regs, false, unwinder::Frame::Trust::kContext)});
        return;
    }

    FX_DCHECK(unwinder_impl_);
    unwinder_impl_->AsyncUnwind(&*stack_, regs, max_depth, std::move(on_done));
  }

 private:
  std::vector<std::unique_ptr<unwinder::Memory>> memory_;
  std::vector<unwinder::Module> modules_;
  unwinder::AsyncMemory::Delegate* delegate_ = nullptr;
  std::optional<unwinder::AsyncMemory> stack_;
  std::optional<unwinder::ElfModuleCache> module_cache_;
  std::unique_ptr<unwinder::UnwinderBase> unwinder_impl_;
  std::unique_ptr<unwinder::AsyncUnwinder> async_unwinder_;
};

}  // namespace

RemoteUnwinder::RemoteUnwinder(ThreadImpl* thread) : thread_(thread) {}

void RemoteUnwinder::UnwindWithRegisters(
    const unwinder::Registers& regs, const Stack::SyncFrameOptions& options,
    fit::callback<void(std::vector<unwinder::Frame>)> on_done) {
  auto loaded_modules = thread_->process()->GetSymbols()->GetLoadedModuleSymbols();

  std::vector<unwinder::Module> modules;
  modules.reserve(loaded_modules.size());
  std::vector<std::unique_ptr<unwinder::Memory>> unwinder_memory;
  // The binary memory and the debug_info memory need their own unwinder::Memory instances.
  unwinder_memory.reserve(loaded_modules.size() * 2);

  for (const auto& loaded_module : loaded_modules) {
    auto build_id_entry =
        thread_->session()->system().GetSymbols()->build_id_index().EntryForBuildID(
            loaded_module->build_id());

    std::unique_ptr<ElfMemoryRegion> binary_file_memory = nullptr;
    std::unique_ptr<ElfMemoryRegion> debug_info_file_memory = nullptr;
    if (!build_id_entry.binary.empty()) {
      binary_file_memory =
          std::make_unique<ElfMemoryRegion>(loaded_module->load_address(), build_id_entry.binary);
    }

    if (!build_id_entry.debug_info.empty()) {
      debug_info_file_memory = std::make_unique<ElfMemoryRegion>(loaded_module->load_address(),
                                                                 build_id_entry.debug_info);
    }

    modules.emplace_back(loaded_module->load_address(), binary_file_memory.get(),
                         debug_info_file_memory.get(), unwinder::Module::AddressMode::kFile);

    if (binary_file_memory)
      unwinder_memory.push_back(std::move(binary_file_memory));
    if (debug_info_file_memory)
      unwinder_memory.push_back(std::move(debug_info_file_memory));
  }

  constexpr size_t kMaxDepth = 256;

  auto wrapper = std::make_unique<RemoteUnwinderImplWrapper>(
      std::move(modules), std::move(unwinder_memory), thread_->process());
  wrapper->Unwind(options.forced_unwinder, regs, kMaxDepth,
                  [wrapper = std::move(wrapper), on_done = std::move(on_done)](
                      std::vector<unwinder::Frame> frames) mutable { on_done(std::move(frames)); });
}

}  // namespace zxdb
