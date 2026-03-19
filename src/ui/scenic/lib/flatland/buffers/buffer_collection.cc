// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/buffers/buffer_collection.h"

#include <lib/zx/result.h>
#include <zircon/errors.h>

namespace flatland {

BufferCollectionInfo::~BufferCollectionInfo() {
  if (buffer_collection_ptr_) {
    buffer_collection_ptr_->Release();
  }
}

BufferCollectionInfo& BufferCollectionInfo::operator=(BufferCollectionInfo&& other) noexcept {
  if (buffer_collection_ptr_) {
    buffer_collection_ptr_->Release();
  }
  buffer_collection_ptr_ = std::move(other.buffer_collection_ptr_);
  buffer_collection_info_ = std::move(other.buffer_collection_info_);
  return *this;
}

BufferCollectionInfo::BufferCollectionInfo(BufferCollectionInfo&& other) noexcept {
  if (buffer_collection_ptr_) {
    buffer_collection_ptr_->Release();
  }
  buffer_collection_ptr_ = std::move(other.buffer_collection_ptr_);
  buffer_collection_info_ = std::move(other.buffer_collection_info_);
}

fit::result<fit::failed, BufferCollectionInfo> BufferCollectionInfo::New(
    fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> buffer_collection_token,
    std::optional<fuchsia::sysmem2::ImageFormatConstraints> image_format_constraints,
    fuchsia::sysmem2::BufferUsage buffer_usage,
    allocation::BufferCollectionUsage buffer_collection_usage) {
  if (!buffer_collection_token.is_valid()) {
    FX_LOGS(ERROR) << "Buffer collection token is not valid.";
    return fit::failed();
  }

  fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          // Bind the buffer collection token to get the local token. Valid tokens can
          // always be bound, so we do not do any error checking at this stage.
          .token(std::move(buffer_collection_token))
          // Use local token to create a BufferCollection and then sync. We can trust
          // |buffer_collection->Sync()| to tell us if we have a bad or malicious channel.
          // So if this call passes, then we know we have a valid BufferCollection.
          .buffer_collection_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollection>(
              buffer_collection.NewRequest().TakeChannel()))
          .Build());

  if (!result.ok()) {
    FX_LOGS(ERROR) << "Could not bind buffer collection. Status: " << result.status_string();
    return fit::failed();
  }

  fuchsia::sysmem2::Node_Sync_Result sync_result;
  zx_status_t status = buffer_collection->Sync(&sync_result);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Could not sync buffer collection. Status: " << status;
    return fit::failed();
  }

  fuchsia::sysmem2::NodeSetNameRequest set_name_request;
  set_name_request.set_priority(10u);
  set_name_request.set_name("FlatlandImageMemory");
  buffer_collection->SetName(std::move(set_name_request));

  // Set basic usage constraints, such as requiring at least one buffer and using Vulkan. This is
  // necessary because all clients with a token need to set constraints before the buffer collection
  // can be allocated.
  fuchsia::sysmem2::BufferCollectionConstraints constraints;
  constraints.set_min_buffer_count(1);

  if (buffer_usage.has_cpu()) {
    fuchsia::sysmem2::BufferUsage usage_to_set = fidl::Clone(buffer_usage);
    if (buffer_collection_usage == allocation::BufferCollectionUsage::kRenderTarget) {
      usage_to_set.set_cpu(usage_to_set.cpu() | fuchsia::sysmem2::CPU_USAGE_WRITE);
    } else {
      usage_to_set.set_cpu(usage_to_set.cpu() | fuchsia::sysmem2::CPU_USAGE_READ);
    }
    constraints.set_usage(std::move(usage_to_set));
  } else if (buffer_usage.has_vulkan()) {
    fuchsia::sysmem2::BufferUsage usage_to_set = fidl::Clone(buffer_usage);
    usage_to_set.set_vulkan(usage_to_set.vulkan() | fuchsia::sysmem2::VULKAN_IMAGE_USAGE_SAMPLED |
                            fuchsia::sysmem::VULKAN_IMAGE_USAGE_TRANSFER_SRC);
    constraints.set_usage(std::move(usage_to_set));
  } else {
    constraints.set_usage(std::move(buffer_usage));
  }

  if (image_format_constraints.has_value()) {
    constraints.mutable_image_format_constraints()->emplace_back(
        std::move(image_format_constraints.value()));
    image_format_constraints.reset();
  }

  fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  set_constraints_request.set_constraints(std::move(constraints));
  status = buffer_collection->SetConstraints(std::move(set_constraints_request));

  // From this point on, if we fail, we DCHECK, because we should have already caught errors
  // pertaining to both invalid tokens and wrong/malicious tokens/channels above, meaning that if
  // a failure occurs now, then there is some underlying issue unrelated to user input.
  FX_DCHECK(status == ZX_OK) << "Could not set constraints on buffer collection.";

  return fit::ok(BufferCollectionInfo(std::move(buffer_collection)));
}

bool BufferCollectionInfo::BuffersAreAllocated() {
  // If the buffer_collection_info_ struct is already populated, then we know the
  // collection is allocated and we can skip over this code.
  if (!buffer_collection_info_.has_buffers()) {
    // Check to see if the buffers are allocated and return false if not.
    fuchsia::sysmem2::BufferCollection_CheckAllBuffersAllocated_Result check_result;
    zx_status_t status = buffer_collection_ptr_->CheckAllBuffersAllocated(&check_result);
    if (status != ZX_OK || !check_result.is_response()) {
      if (status != ZX_OK) {
        FX_LOGS(ERROR) << "Collection was not allocated - status: " << status;
      } else if (check_result.is_framework_err()) {
        FX_LOGS(ERROR) << "Collection was not allocated - framework_err: "
                       << fidl::ToUnderlying(check_result.framework_err());
      } else {
        FX_DCHECK(check_result.is_err());
        FX_LOGS(ERROR) << "Collection was not allocated - err: "
                       << static_cast<uint32_t>(check_result.err());
      }
      return false;
    }

    // We still have to call WaitForBuffersAllocated() here in order to fill in
    // the data for buffer_collection_info_. This won't block, since we've already
    // guaranteed that the collection is allocated above.
    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
    status = buffer_collection_ptr_->WaitForAllBuffersAllocated(&wait_result);
    // Failures here would be an issue with sysmem, and so we DCHECK.
    FX_DCHECK(status == ZX_OK);
    FX_DCHECK(wait_result.is_response());

    buffer_collection_info_ = std::move(*wait_result.response().mutable_buffer_collection_info());

    // Perform a DCHECK here as well to insure the collection has at least one vmo, because
    // it shouldn't have been able to be allocated with less than that.
    FX_DCHECK(buffer_collection_info_.buffers().size() > 0);
  }
  return true;
}

}  // namespace flatland
