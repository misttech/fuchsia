// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/impl/command_buffer_pool.h"

#include <lib/fit/defer.h>

#include "src/ui/lib/escher/impl/command_buffer_sequencer.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/util/trace_macros.h"

namespace escher {
namespace impl {

CommandBufferPool::CommandBufferPool(vk::Device device, vk::Queue queue,
                                     uint32_t queue_family_index, CommandBufferSequencer* sequencer,
                                     bool supports_graphics_and_compute, bool use_protected_memory)
    : device_(device),
      queue_(queue),
      sequencer_(sequencer),
      use_protected_memory_(use_protected_memory) {
  FX_DCHECK(device);
  FX_DCHECK(queue);
  vk::CommandPoolCreateInfo info;
  info.flags = vk::CommandPoolCreateFlagBits::eTransient |
               vk::CommandPoolCreateFlagBits::eResetCommandBuffer;
  if (use_protected_memory) {
    info.flags |= vk::CommandPoolCreateFlagBits::eProtected;
  }
  info.queueFamilyIndex = queue_family_index;
  pool_ = ESCHER_CHECKED_VK_RESULT(device_.createCommandPool(info));

  pipeline_stage_mask_ = vk::PipelineStageFlagBits::eTopOfPipe |
                         vk::PipelineStageFlagBits::eTransfer |
                         vk::PipelineStageFlagBits::eBottomOfPipe |
                         vk::PipelineStageFlagBits::eHost | vk::PipelineStageFlagBits::eAllCommands;
  if (supports_graphics_and_compute) {
    pipeline_stage_mask_ |=
        vk::PipelineStageFlagBits::eDrawIndirect | vk::PipelineStageFlagBits::eVertexInput |
        vk::PipelineStageFlagBits::eVertexShader |
        // TODO: cache supported stages at startup, otherwise
        //       validation layers would complain on devices that
        //       have geometry/tessellation shaders.
        // vk::PipelineStageFlagBits::eTessellationControlShader |
        // vk::PipelineStageFlagBits::eTessellationEvaluationShader |
        // vk::PipelineStageFlagBits::eGeometryShader |
        vk::PipelineStageFlagBits::eFragmentShader |
        vk::PipelineStageFlagBits::eEarlyFragmentTests |
        vk::PipelineStageFlagBits::eLateFragmentTests |
        vk::PipelineStageFlagBits::eColorAttachmentOutput |
        vk::PipelineStageFlagBits::eComputeShader | vk::PipelineStageFlagBits::eAllGraphics;
  }
}

CommandBufferPool::~CommandBufferPool() {
  Cleanup();
  if (!pending_buffers_.empty()) {
    // We didn't call waitIdle() above to avoid unnecessary blocking: there
    // may be other pools with pending buffers, and there is no need to wait
    // for them to finish if the initial call to Cleanup() successfully returns
    // all buffers to the free list.
    ESCHER_DCHECK_VK_RESULT(device_.waitIdle());
    Cleanup();
  }
  FX_DCHECK(pending_buffers_.empty());
  if (free_buffers_.size() > 0) {
    std::vector<vk::CommandBuffer> buffers_to_free;
    buffers_to_free.reserve(free_buffers_.size());
    while (!free_buffers_.empty()) {
      auto& buf = free_buffers_.front();
      buffers_to_free.push_back(buf->vk());
      device_.destroyFence(buf->fence());
      free_buffers_.pop();
    }
    device_.freeCommandBuffers(pool_, static_cast<uint32_t>(buffers_to_free.size()),
                               buffers_to_free.data());
  }
  device_.destroyCommandPool(pool_);
}

CommandBuffer* CommandBufferPool::GetCommandBuffer() {
  TRACE_DURATION("gfx", "escher::CommandBufferPool::GetCommandBuffer");

  // Find an existing CommandBuffer for reuse, or create a new one.
  CommandBuffer* buffer = nullptr;
  if (free_buffers_.empty()) {
    vk::CommandBufferAllocateInfo info;
    info.commandPool = pool_;
    info.level = vk::CommandBufferLevel::ePrimary;
    info.commandBufferCount = 1;
    auto allocated_vulkan_buffers = ESCHER_CHECKED_VK_RESULT(device_.allocateCommandBuffers(info));

    vk::Fence fence = ESCHER_CHECKED_VK_RESULT(device_.createFence(vk::FenceCreateInfo()));

    buffer = new CommandBuffer(device_, allocated_vulkan_buffers[0], fence, pipeline_stage_mask_,
                               use_protected_memory_);
    pending_buffers_.push(std::unique_ptr<CommandBuffer>(buffer));
  } else {
    buffer = free_buffers_.front().get();
    pending_buffers_.push(std::move(free_buffers_.front()));
    free_buffers_.pop();
  }
  buffer->Begin(sequencer_->GenerateNextCommandBufferSequenceNumber());
  return buffer;
}

bool CommandBufferPool::Cleanup() {
  TRACE_DURATION("gfx", "escher::CommandBufferPool::Cleanup");
  if (cleaning_up_) {
    // A `CommandBufferFinishedCallback` invoked below may trigger a nested call
    // to `Cleanup()`. Return false immediately so finished callbacks do not run
    // nested inside one another, bounding recursion and keeping callbacks in
    // FIFO order at one level while the active `Cleanup()` call on the stack
    // finishes draining `pending_buffers_`.
    return false;
  }
  cleaning_up_ = true;
  auto reset_cleaning_up = fit::defer([this]() { cleaning_up_ = false; });

  while (!pending_buffers_.empty()) {
    CommandBufferFinishedCallback callback;
    if (!pending_buffers_.front()->Retire(&callback)) {
      // The first buffer in the queue is not finished, so neither are the rest.
      // Return false to notify the caller that more cleanup will need to be
      // done in the future.
      // TODO: the "first buffer in the queue is not finished, so..." logic is
      // not necessarily true, but it should be close enough as long as everyone
      // is being a "good citizen" (e.g. by submitting buffers in a timely
      // fashion).
      return false;
    }
    std::unique_ptr<CommandBuffer> buffer = std::move(pending_buffers_.front());
    pending_buffers_.pop();

    // Return the retired buffer to `free_buffers_` and notify `sequencer_`
    // before running `callback`. This ensures that if `callback` submits new
    // work or releases resources, the buffer can be reused and the resources
    // can be recycled immediately.
    const uint64_t sequence_number = buffer->sequence_number();
    [[maybe_unused]] const CommandBuffer* buffer_ptr = buffer.get();
    free_buffers_.push(std::move(buffer));
    sequencer_->CommandBufferFinished(sequence_number);

    if (callback) {
      // Keep the legacy "Retire::callback" name for existing trace consumers.
      TRACE_DURATION("gfx", "escher::CommandBuffer::Retire::callback", "ptr",
                     TA_POINTER(buffer_ptr));
      callback();
    }
  }

  // No pending buffers remain.
  return true;
}

}  // namespace impl
}  // namespace escher
