// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/impl/uniform_buffer_pool.h"

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/impl/naive_buffer.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/vk/gpu_allocator.h"

namespace escher {
namespace impl {

// TODO: obtain max uniform-buffer size from Vulkan.  64kB is typical.
constexpr vk::DeviceSize kBufferSize = 65536;

UniformBufferPool::UniformBufferPool(EscherWeakPtr escher, size_t ring_size,
                                     GpuAllocator* allocator,
                                     vk::MemoryPropertyFlags additional_flags)
    : ResourceManager(escher),
      allocator_(allocator ? allocator : escher->gpu_allocator()),
      flags_(additional_flags | vk::MemoryPropertyFlagBits::eHostVisible),
      buffer_size_(kBufferSize),
      ring_size_(ring_size),
      weak_factory_(this) {
  FX_DCHECK(ring_size >= 1 && ring_size <= kMaxRingSize);
}

UniformBufferPool::~UniformBufferPool() {}

BufferPtr UniformBufferPool::Allocate() {
  if (ring_[0].empty()) {
    InternalAllocate();
  }
  BufferPtr buf(ring_[0].back().release());
  ring_[0].pop_back();
  return buf;
}

void UniformBufferPool::InternalAllocate() {
  // Create a batch of buffers.
  constexpr uint32_t kBufferBatchSize = 10;
  vk::Buffer new_buffers[kBufferBatchSize];
  vk::BufferCreateInfo info;
  info.size = buffer_size_;
  info.usage = vk::BufferUsageFlagBits::eUniformBuffer;
  info.sharingMode = vk::SharingMode::eExclusive;
  for (uint32_t i = 0; i < kBufferBatchSize; ++i) {
    new_buffers[i] = ESCHER_CHECKED_VK_RESULT(vk_device().createBuffer(info));
  }

  // Determine the memory requirements for a single buffer.
  vk::MemoryRequirements reqs = vk_device().getBufferMemoryRequirements(new_buffers[0]);

  // It is possible that the Vulkan device requires a larger memory size to hold
  // the buffer (for metadata or alignment). In that case, we'll need to
  // increase the allocation size and make it aligned to alignment requirement.
  auto single_buffer_alloc_size =
      reqs.size + (reqs.alignment - reqs.size % reqs.alignment) % reqs.alignment;

  // Allocate enough memory for all of the buffers.
  reqs.size = single_buffer_alloc_size * kBufferBatchSize;
  auto batch_mem = allocator_->AllocateMemory(reqs, flags_);

  // See below: when OnReceiveOwnable() receives the newly-allocated buffer we
  // need to know that it is new and can therefore be used immediately instead
  // added to the back of the ring.
  is_allocating_ = true;

  for (uint32_t i = 0; i < kBufferBatchSize; ++i) {
    // Validation layer complains if we bind a buffer to memory without first
    // querying it's memory requirements.  This shouldn't be necessary, since
    // all buffers are identically-configured.
    // TODO: disable this in release mode.
    auto reqs = vk_device().getBufferMemoryRequirements(new_buffers[i]);

    // Sub-allocate memory for each buffer.
    auto mem = batch_mem->Suballocate(single_buffer_alloc_size, i * single_buffer_alloc_size);

    // Workaround for dealing with RefPtr/Reffable Adopt() semantics.  Let the
    // RefPtr go out of scope immediately; the Buffer will be added to
    // free_buffers_ via OnReceiveOwnable().
    NaiveBuffer::AdoptVkBuffer(this, std::move(mem), single_buffer_alloc_size, new_buffers[i]);
  }

  is_allocating_ = false;
}

void UniformBufferPool::OnReceiveOwnable(std::unique_ptr<Resource> resource) {
  FX_DCHECK(resource->IsKindOf<Buffer>());
  size_t ring_index = is_allocating_ ? 0 : ring_size_ - 1;
  ring_[ring_index].emplace_back(static_cast<Buffer*>(resource.release()));
}

void UniformBufferPool::BeginFrame() {
  if (ring_size_ == 1)
    return;

  // Move all entries from ring_[1] to ring_[0].
  for (auto& buf : ring_[1]) {
    ring_[0].push_back(std::move(buf));
  }
  ring_[1].clear();

  // The ring cleared above is moved to the back, and all others are moved one
  // forward.
  //
  // TODO(https://fxbug.dev/42151324): This is a constant amount of cache-friendly work per frame
  // (just swapping pointers in the vectors), so it's probably not a performance
  // issue, but is worth looking into later.
  for (size_t i = 2; i < ring_size_; ++i) {
    std::swap(ring_[i], ring_[i - 1]);
  }
}

}  // namespace impl
}  // namespace escher
