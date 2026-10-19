// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DRIVERS_MSD_VSI_VIP_SRC_COMMAND_BUFFER_H_
#define SRC_GRAPHICS_DRIVERS_MSD_VSI_VIP_SRC_COMMAND_BUFFER_H_

#include <lib/magma_service/util/accessor.h>
#include <lib/magma_service/util/command_buffer.h>

#include "gpu_mapping.h"
#include "instructions.h"
#include "msd_vsi_context.h"

class CommandBuffer : public magma::CommandBuffer<MsdVsiContext, GpuMapping> {
 public:
  // The client is required to provide a buffer with at least 8 additional bytes available
  // and mapped, which the driver will write a LINK instruction in.
  static constexpr uint32_t kAdditionalBytes = kInstructionDwords * sizeof(uint32_t);

  // Only up to 2 resources are supported, the batch buffer and optional context state buffer.
  static constexpr uint32_t kMaxAllowedResources = 2;

  static std::unique_ptr<CommandBuffer> Create(
      std::shared_ptr<MsdVsiContext> context, msd::msd_client_id_t client_id,
      std::unique_ptr<msd::magma_command_buffer> cmd_buf, std::vector<ExecResource> resources,
      std::vector<std::shared_ptr<magma::PlatformSemaphore>> signal_semaphores) {
    if (cmd_buf->resource_count > kMaxAllowedResources) {
      MAGMA_LOG(ERROR, "Invalid resource count, only 1 additional context state is supported");
      return nullptr;
    }
    std::optional<uint32_t> context_state_buffer_resource_index;
    if (cmd_buf->resource_count == 2) {
      context_state_buffer_resource_index = cmd_buf->batch_buffer_resource_index == 0 ? 1 : 0;
    }
    auto command_buffer = std::make_unique<CommandBuffer>(context, client_id, std::move(cmd_buf),
                                                          context_state_buffer_resource_index);
    if (!command_buffer->InitializeResources(std::move(resources), {} /* wait_semaphores */,
                                             std::move(signal_semaphores))) {
      MAGMA_LOG(ERROR, "Failed to initialize resources");
      return nullptr;
    }
    return command_buffer;
  }

  CommandBuffer(std::weak_ptr<MsdVsiContext> context, uint64_t connection_id,
                std::unique_ptr<msd::magma_command_buffer> command_buffer,
                std::optional<uint32_t> csb_resource_index = std::nullopt)
      : magma::CommandBuffer<MsdVsiContext, GpuMapping>(context, connection_id,
                                                        std::move(command_buffer)),
        csb_index_(csb_resource_index) {}

  // Returns a pointer to the batch buffer.
  magma::PlatformBuffer* GetBatchBuffer() {
    if (batch_buffer_index() < exec_resources_.size()) {
      return exec_resources_[batch_buffer_index()].buffer->platform_buffer();
    }
    DASSERT(false);
    return nullptr;
  }

  // Returns the offset into the batch buffer that points to the end of the user data.
  uint32_t GetBatchBufferWriteOffset() {
    uint64_t length = magma::round_up(GetLength(), sizeof(uint64_t));
    return magma::to_uint32(batch_start_offset() + length);
  }

  // Returns a pointer to the resource for the context state buffer.
  // May be null if no context state buffer is present.
  const ExecResource* GetContextStateBufferResource() const {
    if (!csb_index_.has_value()) {
      return nullptr;
    }
    auto index = csb_index_.value();
    DASSERT(index < exec_resources_.size());
    return &exec_resources_[index];
  }

  // Returns a read only view of the context state buffer's GPU mapping.
  // May be null if no context state buffer is present.
  const GpuMappingView* GetContextStateBufferMapping() const {
    DASSERT(prepared_to_execute_);
    if (!csb_index_.has_value()) {
      return nullptr;
    }
    auto index = csb_index_.value();
    DASSERT(index < exec_resource_mappings_.size());
    return exec_resource_mappings_[index].get();
  }

  // Returns whether the batch buffer and context state buffer (if present) are valid.
  // This should only be called after |PrepareForExecution|.
  bool IsValidBatch() {
    DASSERT(prepared_to_execute_);

    if ((batch_start_offset() & (sizeof(uint64_t) - 1)) != 0) {
      MAGMA_LOG(ERROR, "batch start offset is not 8 byte aligned");
      return false;
    }
    auto mapping = GetBatchMapping();
    // |GetLength| returns the actual size of the user's data.
    if (!CheckOverflowAndRange(batch_start_offset(), GetLength(), mapping->length())) {
      MAGMA_LOG(ERROR, "insufficient space for LINK command, mapped %lu used %lu need %u\n",
                mapping->length(), GetLength(), kAdditionalBytes);
      return false;
    }

    auto csb = GetContextStateBufferResource();
    if (csb) {
      // Check that the mapped length can fit the user data and also an additional LINK command.
      auto csb_mapping = GetContextStateBufferMapping();
      DASSERT(csb_mapping);
      if (!CheckOverflowAndRange(0, csb->length, csb_mapping->length())) {
        MAGMA_LOG(ERROR, "CSB: insufficient space for LINK command, mapped %lu used %lu need %u\n",
                  csb_mapping->length(), csb->length, kAdditionalBytes);
        return false;
      }
    }
    return true;
  }

 private:
  static bool CheckOverflowAndRange(uint64_t offset, uint64_t length, uint64_t total_length) {
    uint64_t required_bytes;
    if (__builtin_add_overflow(offset, length, &required_bytes) ||
        __builtin_add_overflow(required_bytes, kAdditionalBytes, &required_bytes)) {
      return false;
    }
    return required_bytes <= total_length;
  }

  std::optional<uint32_t> csb_index_;
};

#endif  // SRC_GRAPHICS_DRIVERS_MSD_VSI_VIP_SRC_COMMAND_BUFFER_H_
