// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_LD_TEST_LD_LOAD_ZIRCON_PROCESS_TESTS_BASE_H_
#define LIB_LD_TEST_LD_LOAD_ZIRCON_PROCESS_TESTS_BASE_H_

#include <lib/elfldltl/soname.h>
#include <lib/ld/testing/test-processargs.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/vmar.h>

#include "ld-load-zircon-ldsvc-tests-base.h"

namespace ld::testing {

// This is the common base class for test fixtures to launch a Zircon process.
class LdLoadZirconProcessTestsBase : public LdLoadZirconLdsvcTestsBase {
 public:
  // The Fuchsia test executables (via modules/zircon-test-start.cc) link
  // directly to the vDSO, so it appears before other modules.
  static constexpr std::optional<elfldltl::Soname<>> kTestExecutableNeedsVdso{
      "libzircon.so",
  };

  static constexpr int64_t kRunFailureForTrap = ZX_TASK_RETCODE_EXCEPTION_KILL;
  static constexpr int64_t kRunFailureForBadPointer = ZX_TASK_RETCODE_EXCEPTION_KILL;

  ~LdLoadZirconProcessTestsBase();

  const char* process_name() const;

  zx::channel& bootstrap_sender() { return procargs_.bootstrap_sender(); }

  // This just folds together Start() and Wait(), below.
  int64_t Run();

 protected:
  const zx::process& process() const { return process_; }

  // A subclass calls this when not using CreateProcess().
  void set_process(zx::process process);

  // A subclass calls CreateProcess() to set process(), root_vmar(), and thread().
  void CreateProcess();

  // These are set by CreateProcess() and used by Start() and Run().
  const zx::vmar& root_vmar() { return root_vmar_; }
  const zx::thread& thread() { return thread_; }

  // This is used by Start() and Run().  If it's not empty() when they're
  // called, its pending startup dynamic linker message gets packed and sent.
  TestProcessArgs& bootstrap() { return procargs_; }

  // These are used by Start() and Run().  The subclass must set them first.
  uintptr_t entry() const { return entry_; }
  uintptr_t vdso_base() const { return vdso_base_; }
  std::optional<size_t> stack_size() const { return stack_size_; }
  void set_entry(uintptr_t entry) { entry_ = entry; }
  void set_vdso_base(uintptr_t vdso_base) { vdso_base_ = vdso_base; }
  void set_stack_size(std::optional<size_t> stack_size) { stack_size_ = stack_size; }

  // Start the process() using all those parameters.
  void Start();

  // Wait for the process to die and collect its exit code.
  // This clears the process() so a new one can be installed.
  int64_t Wait();

 private:
  zx::process process_;

  // Not all subclasses use these.
  zx::vmar root_vmar_;
  zx::thread thread_;
  TestProcessArgs procargs_;
  uintptr_t entry_ = 0;
  uintptr_t vdso_base_ = 0;
  std::optional<size_t> stack_size_;
};

}  // namespace ld::testing

#endif  // LIB_LD_TEST_LD_LOAD_ZIRCON_PROCESS_TESTS_BASE_H_
