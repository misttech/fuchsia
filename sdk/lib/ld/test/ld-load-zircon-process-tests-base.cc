// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ld-load-zircon-process-tests-base.h"

#include <lib/elfldltl/machine.h>
#include <lib/ld/abi.h>
#include <lib/zx/job.h>
#include <zircon/processargs.h>

#include <gtest/gtest.h>

namespace ld::testing {

const char* LdLoadZirconProcessTestsBase::process_name() const {
  return ::testing::UnitTest::GetInstance()->current_test_info()->name();
}

void LdLoadZirconProcessTestsBase::set_process(zx::process process) {
  ASSERT_FALSE(process_);
  process_ = std::move(process);
}

void LdLoadZirconProcessTestsBase::CreateProcess() {
  ASSERT_FALSE(process_);

  std::string_view name = process_name();
  ASSERT_EQ(zx::process::create(*zx::job::default_job(), name.data(),
                                static_cast<uint32_t>(name.size()), 0, &process_, &root_vmar_),
            ZX_OK);

  ASSERT_EQ(zx::thread::create(this->process(), name.data(), static_cast<uint32_t>(name.size()), 0,
                               &thread_),
            ZX_OK);
}

int64_t LdLoadZirconProcessTestsBase::Wait() {
  int64_t result = -1;
  auto wait_for_termination = [process = std::exchange(process_, {}), &result]() {
    ASSERT_TRUE(process) << "Wait() called before Init()?";
    zx_signals_t signals;
    ASSERT_EQ(process.wait_one(ZX_PROCESS_TERMINATED, zx::time::infinite(), &signals), ZX_OK);
    ASSERT_TRUE(signals & ZX_PROCESS_TERMINATED);
    zx_info_process_t info;
    ASSERT_EQ(process.get_info(ZX_INFO_PROCESS, &info, sizeof(info), nullptr, nullptr), ZX_OK);
    ASSERT_TRUE(info.flags & ZX_INFO_PROCESS_FLAG_STARTED);
    ASSERT_TRUE(info.flags & ZX_INFO_PROCESS_FLAG_EXITED);
    result = info.return_code;
  };
  wait_for_termination();

  return result;
}

void LdLoadZirconProcessTestsBase::Start() {
  // Allocate the stack.  This is delayed until here in case the test uses
  // bootstrap() methods after Init() that affect bootstrap().GetStackSize().
  zx::vmo stack_vmo;
  uintptr_t sp;
  std::optional<size_t> bootstrap_stack_size = stack_size_;
  if (!bootstrap_stack_size) {
    // TODO(https://fxbug.dev/479521328): stack use too big for procargs piddly
    // default bootstrap_stack_size = bootstrap.GetStackSize();
    bootstrap_stack_size = 64 << 10;
  }

  const size_t page_size = zx_system_get_page_size();
  const size_t stack_vmo_size = (*bootstrap_stack_size + page_size - 1) & -page_size;
  const size_t stack_vmar_size = stack_vmo_size + page_size;

  ASSERT_EQ(zx::vmo::create(stack_vmo_size, 0, &stack_vmo), ZX_OK);

  zx::vmar stack_vmar;
  uintptr_t stack_vmar_base;
  ASSERT_EQ(root_vmar().allocate(ZX_VM_CAN_MAP_SPECIFIC | ZX_VM_CAN_MAP_READ | ZX_VM_CAN_MAP_WRITE,
                                 0, stack_vmar_size, &stack_vmar, &stack_vmar_base),
            ZX_OK);

  zx_vaddr_t stack_base;
  ASSERT_EQ(stack_vmar.map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_SPECIFIC | ZX_VM_ALLOW_FAULTS,
                           page_size, stack_vmo, 0, stack_vmo_size, &stack_base),
            ZX_OK);

  if (!procargs_.empty()) {
    ASSERT_NO_FATAL_FAILURE(procargs_.AddStackVmo(std::move(stack_vmo)));
  }

  sp = elfldltl::AbiTraits<>::InitialStackPointer(stack_base, stack_vmo_size);

  // Pack up the bootstrap message and start the process running.
  zx::channel bootstrap_receiver;
  if (procargs_.empty()) {
    // There's startup dynamic linker message being sent.  Just create the
    // channel so bootstrap_sender() can be used.
    bootstrap_receiver = procargs_.MakeBootstrap();
  } else {
    bootstrap_receiver = procargs_.PackBootstrap();
  }

  ASSERT_EQ(this->process().start(thread(), entry_, sp, std::move(bootstrap_receiver), vdso_base_),
            ZX_OK);
}

int64_t LdLoadZirconProcessTestsBase::Run() {
  Start();
  return ::testing::Test::HasFatalFailure() ? -1 : Wait();
}

LdLoadZirconProcessTestsBase::~LdLoadZirconProcessTestsBase() {
  if (process_) {
    EXPECT_EQ(process_.kill(), ZX_OK);
  }
}

}  // namespace ld::testing
