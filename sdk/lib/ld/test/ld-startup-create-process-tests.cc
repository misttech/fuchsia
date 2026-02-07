// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ld-startup-create-process-tests.h"

#include <lib/zx/job.h>
#include <zircon/process.h>

#include <gtest/gtest.h>

namespace ld::testing {

void LdStartupCreateProcessTestsBase::Init(std::initializer_list<std::string_view> args,
                                           std::initializer_list<std::string_view> env) {
  LdLoadZirconLdsvcTestsBase::Init(args, env);

  std::string_view name = process_name();
  ASSERT_NO_FATAL_FAILURE(CreateProcess());

  fbl::unique_fd log_fd;
  ASSERT_NO_FATAL_FAILURE(InitLog(log_fd));

  // Start packing the bootstrap message for the startup dynamic linker.
  // The packing will be completed in Run.
  ASSERT_NO_FATAL_FAILURE(  //
      LdStartupProcArgs(bootstrap(), std::move(log_fd), root_vmar().borrow())
          .AddProcess(process().borrow())
          .AddThread(thread().borrow()));
}

void LdStartupCreateProcessTestsBase::FinishLoad(zx::vmo executable_vmo) {
  // Send the executable VMO.
  ASSERT_NO_FATAL_FAILURE(bootstrap().AddExecutableVmo(std::move(executable_vmo)));

  // Prime the mock loader service from the Needed() calls.
  ASSERT_NO_FATAL_FAILURE(LdsvcExpectNeeded());

  // If a mock loader service has been set up by calls to Needed() et al,
  // send the client end over.
  if (zx::channel ldsvc = TakeLdsvc()) {
    ASSERT_NO_FATAL_FAILURE(bootstrap().AddLdsvc(std::move(ldsvc)));
  }
}

LdStartupCreateProcessTestsBase::~LdStartupCreateProcessTestsBase() = default;

}  // namespace ld::testing
