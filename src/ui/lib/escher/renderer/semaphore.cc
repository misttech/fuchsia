// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/renderer/semaphore.h"

#include "src/ui/lib/escher/impl/semaphore_pool.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/util/trace_macros.h"

namespace escher {

Semaphore::Semaphore(vk::Device device, SemaphorePool* pool) : device_(device), pool_(pool) {
  TRACE_DURATION("gfx", "escher::Semaphore::New");
  vk::SemaphoreCreateInfo info;
  value_ = ESCHER_CHECKED_VK_RESULT(device_.createSemaphore(info));
}

Semaphore::~Semaphore() { device_.destroySemaphore(value_); }

SemaphorePtr Semaphore::New(vk::Device device, SemaphorePool* pool) {
  return fxl::MakeRefCounted<Semaphore>(device, pool);
}

bool Semaphore::OnZeroRefCount() {
  if (pool_) {
    // Retain the ref-count to prevent immediate deletion during ReturnSemaphore execution.
    pool_->ReturnSemaphore(SemaphorePtr(this));
    return false;  // Defer destruction!
  }
  return true;  // Destroy if unpooled.
}

}  // namespace escher
