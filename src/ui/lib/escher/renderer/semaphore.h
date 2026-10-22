// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_RENDERER_SEMAPHORE_H_
#define SRC_UI_LIB_ESCHER_RENDERER_SEMAPHORE_H_

#include <cstdint>

#include "src/lib/fxl/macros.h"
#include "src/ui/lib/escher/base/reffable.h"

#include <vulkan/vulkan.hpp>

#ifdef __Fuchsia__
namespace zx {
class event;
}  // namespace zx
#endif

namespace escher {

class SemaphorePool;
#ifdef __Fuchsia__
class VulkanDeviceQueues;
#endif

class Semaphore;
typedef fxl::RefPtr<Semaphore> SemaphorePtr;

class Semaphore : public Reffable {
 public:
  // Uses `uint64_t` instead of `zx_koid_t` because this header is also compiled for host.
  static constexpr uint64_t kInvalidKoid = 0;

  explicit Semaphore(vk::Device device, SemaphorePool* pool = nullptr);
  ~Semaphore() override;

  // Convenient.
  static SemaphorePtr New(vk::Device device, SemaphorePool* pool = nullptr);

  vk::Semaphore vk_semaphore() const { return value_; }

  bool is_imported() const { return koid_ != kInvalidKoid; }

  // Kernel object ID (`zx_koid_t`) of the imported Zircon event, or `kInvalidKoid`
  // (`ZX_KOID_INVALID`) if unimported. Read by `CommandBuffer` for wait/signal deduplication,
  // `SemaphorePool::ReturnSemaphore()` via `is_imported()`, and `VkRenderer::Render()` for trace
  // flow IDs.
  uint64_t koid() const { return koid_; }

 protected:
  bool OnZeroRefCount() override;

 private:
  friend class SemaphorePool;
#ifdef __Fuchsia__
  friend SemaphorePtr GetSemaphoreForEvent(VulkanDeviceQueues* device, zx::event event);
#endif

  // Must be called only after importing the Zircon event with `koid` (or when resetting to
  // `kInvalidKoid` in `SemaphorePool::ReturnSemaphore()`). Also defines `is_imported()`.
  void set_koid(uint64_t koid) { koid_ = koid; }

  vk::Device device_;
  vk::Semaphore value_;
  SemaphorePool* pool_ = nullptr;
  uint64_t koid_ = kInvalidKoid;

  FXL_DISALLOW_COPY_AND_ASSIGN(Semaphore);
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_RENDERER_SEMAPHORE_H_
